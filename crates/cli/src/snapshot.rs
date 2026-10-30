//! `arena snapshot`: a read-only fleet overview (pods, SSH reach, proxy, last health check), as a
//! table or the public JSON.

use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::selector::Naming;
use arena_core::Config;

use crate::keys::openrouter_client;
use crate::pods::health::health_cache_path;
use crate::pods::list::{enrich_best_effort, ENRICH_TIMEOUT};
use crate::{money, spend};

/// `arena snapshot`: see [`collect_snapshot`] for what it reads, [`render_snapshot`] for the
/// three outputs. `--out` replaces the file atomically — owner-only unless it's the public
/// JSON (which a web server has to read) — and warns when it isn't the public JSON. A
/// public file is kept (and the exit is non-zero) when the new one would show no machines
/// only because a provider failed to list ([`arena_core::snapshot::would_blank_the_page`]).
pub(crate) async fn handle_snapshot(
    provider: &dyn Provider,
    cfg: &Config,
    json: bool,
    public: bool,
    out: Option<&std::path::Path>,
    // How SSH ports are probed (`SshPortProbe` for real, scripted in tests); `None` =
    // `--no-probe`.
    reach: Option<Arc<dyn arena_core::snapshot::Reach>>,
) -> Result<()> {
    let bal = money::BalanceCtx::from_config(cfg);
    let (snap, warnings) = collect_snapshot(provider, cfg, arena_core::snapshot::unix_now(), reach, &bal).await?;
    for w in warnings {
        eprintln!("{w}");
    }
    let naming = Naming::from_config(cfg);
    let text = render_snapshot(&snap, &naming, json, public)?;
    match out {
        None => print!("{text}"),
        Some(path) => {
            if !public {
                eprintln!(
                    "warning: {} gets the INTERNAL snapshot (ids, endpoints, costs) — publish only `--public` output",
                    path.display()
                );
            } else if let Ok(previous) = std::fs::read_to_string(path) {
                let next = arena_core::snapshot::public_snapshot(&snap, &naming);
                if arena_core::snapshot::would_blank_the_page(&previous, &next) {
                    anyhow::bail!(
                        "{} failed to list and nothing else listed a machine — kept {} as it was rather than \
                         publish an empty fleet (the page shows it going stale); re-run once it answers",
                        snap.partial.join(", "),
                        path.display()
                    );
                }
            }
            let mode = if public { 0o644 } else { 0o600 };
            arena_core::snapshot::write_atomic(path, text.as_bytes(), mode)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Gather a [`arena_core::snapshot::FleetSnapshot`] — read-only by construction: one list
/// call per provider (each bounded), the best-effort details query `pods list` makes, the
/// local proxy config file, the health cache, and (with `reach`) one TCP probe of each
/// billing cohort machine's SSH port ([`arena_core::snapshot::reach_targets`]), all at once
/// and bounded ([`arena_core::snapshot::REACH_TIMEOUT`]):
/// a connect and the server's greeting, nothing sent. No `Remote` is even in reach: no SSH
/// session, no deep check, no proxy over SSH. A provider that fails to list makes the
/// snapshot partial (and the public one `complete: false`); if none answers it's an error
/// and nothing is written — a page going stale says more than a page that shows an empty
/// fleet. The prepaid balances come from the balance state file alone (`bal` only says
/// which accounts are configured): no balance call, nothing written there — a cron's snapshot
/// never observes balances. The listing is recorded in the spend ledger (quietly): with the
/// snapshot cron line, that's what keeps `arena spend` current. Returns the snapshot and the
/// warning lines to print.
async fn collect_snapshot(
    provider: &dyn Provider,
    cfg: &Config,
    now: u64,
    reach: Option<Arc<dyn arena_core::snapshot::Reach>>,
    bal: &money::BalanceCtx,
) -> Result<(arena_core::snapshot::FleetSnapshot, Vec<String>)> {
    use arena_core::snapshot::{self, HealthCache};
    let listing = arena_core::proxy::Listing::from_results(provider.list_by_provider().await);
    if !listing.any_ok() {
        let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        anyhow::bail!("no provider listed its pods ({}) — nothing written", errs.join("; "));
    }
    let mut warnings = Vec::new();
    let mut partial = Vec::new();
    for (p, e) in listing.errors() {
        warnings.push(format!("warning: {p} failed to list ({e}) — its pods are missing from the snapshot"));
        partial.push(p.to_string());
    }
    let mut pods = listing.pods();
    if let Some(w) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
        warnings.push(w);
    }
    // The one thing a snapshot writes: what it listed, into the spend ledger (quietly) — the
    // snapshot cron line is what keeps `arena spend` current with nobody at a terminal. The AI
    // keys too, once an hour (bounded, quiet): `--since` then finds each key's usage near a
    // day's start, and a key deleted outside arena keeps a recent last reading.
    let tracking = spend::SpendCtx::from_config(cfg);
    tracking.listing(&listing, &pods, now);
    if let Some(or) = openrouter_client(cfg).filter(|_| tracking.keys_due()) {
        let listed_at = arena_core::snapshot::unix_now();
        if let Ok(Ok(keys)) = arena_core::ratelimit::bounded(ENRICH_TIMEOUT, arena_core::openrouter::KeyApi::list_keys(&or)).await {
            tracking.keys(&keys, listed_at);
        }
    }
    let (proxy, w) = snapshot::local_proxy_text(cfg);
    warnings.extend(w);
    let health = match health_cache_path(cfg) {
        None => HealthCache::new(),
        Some(Err(e)) => {
            warnings.push(format!("warning: no health cache to read: {e}"));
            HealthCache::new()
        }
        Some(Ok(path)) => {
            let (cache, w) = HealthCache::load(&path);
            warnings.extend(w);
            cache
        }
    };
    let mut snap = snapshot::build(&pods, &partial, proxy.as_deref(), &health, &Naming::from_config(cfg), now);
    let accounts = bal.api.accounts();
    if !accounts.is_empty() {
        use arena_core::balance::{self, BalancePolicy, Fetched, Mode};
        let (state, _) = balance::load_opt(bal.state_path());
        let fetched: Vec<Fetched> = accounts.into_iter().map(|(a, fp)| Fetched::nothing(a, fp)).collect();
        let (policy, _) = BalancePolicy::from_config(cfg);
        snap.balance = balance::views(&state, &fetched, &balance::burns_with(&listing, &pods, &state, &policy), &policy, Mode::CacheOnly, now);
        snap.balance_policy = policy;
    }
    if let Some(reach) = reach {
        snapshot::probe_reachability(&mut snap, reach).await;
    }
    Ok((snap, warnings))
}

/// What `arena snapshot` prints (or writes): the public allowlisted JSON with `--public`
/// (whatever else is passed — it's the only form fit to publish), else the full internal
/// JSON with `--json`, else the table. Pure.
fn render_snapshot(
    snap: &arena_core::snapshot::FleetSnapshot,
    naming: &Naming,
    json: bool,
    public: bool,
) -> Result<String> {
    use arena_core::snapshot;
    Ok(if public {
        snapshot::public_json(&snapshot::public_snapshot(snap, naming))
    } else if json {
        format!("{}\n", serde_json::to_string_pretty(snap)?)
    } else {
        snapshot::render_table(snap)
    })
}

/// `arena snapshot`: read-only end to end (a provider that panics on any mutation, no
/// `Remote` in reach), the health cache written by `pods test --deep` read back, the
/// public JSON written atomically and allowlisted.
#[cfg(test)]
mod snapshot_tests;
