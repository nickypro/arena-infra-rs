//! `pods list`: the listing, enriched within a budget — a failing or slow provider degrades it, never fails it.

use std::sync::Arc;

use anyhow::Result;
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::pods::each_pod;
use crate::ssh_config::ssh_config_lines;
use crate::{money, reap, spend};

/// How long `pods list` waits for the best-effort pod details before rendering without them.
pub(crate) const ENRICH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `pods list`'s best-effort [`Provider::enrich`]: never fatal and never hangs the listing
/// (the HTTP client has no timeout of its own). Returns the one warning line to print when
/// details are incomplete; the pods keep whatever was filled before the failure.
pub(crate) async fn enrich_best_effort(
    provider: &dyn Provider,
    pods: &mut [arena_core::Pod],
    limit: std::time::Duration,
) -> Option<String> {
    let what = "warning: pod details incomplete (GPU/$/h/maintenance)";
    match arena_core::ratelimit::bounded(limit, provider.enrich(pods)).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{what}: {e}")),
        Err(_) => Some(format!("{what}: timed out after {limit:?}")),
    }
}

/// `pods list`'s GPU probe: `nvidia-smi` on every pod with an endpoint, concurrently over
/// `remote` (5s connect timeout, the whole probe within `PROBE_TIMEOUT` — see
/// `metrics::fetch_with`), so a wedged pod costs the listing at most that budget. Where a
/// pod answers, what the machine itself sees overrides the provider-reported GPU (type and
/// count); one that doesn't keeps what the provider said.
///
/// Results are keyed by the pod's position in `pods`, not its name: two pods can share a
/// name (a half-finished replace, a hand-made duplicate), and keyed by name one pod's GPU
/// was shown on both rows. Not by id either — ids are only unique per provider.
pub(crate) async fn probe_gpus(remote: &Arc<dyn Remote>, cfg: &Config, pods: &mut [arena_core::Pod]) {
    use arena_core::metrics::{self, ProbeOpts};
    let mut jobs = Vec::new();
    for (i, pod) in pods.iter().enumerate() {
        if let Ok(mut t) = SshTarget::from_pod(pod, cfg) {
            t.connect_timeout_secs = 5;
            let remote = remote.clone();
            let probe = async move { metrics::fetch_with(remote.as_ref(), &t, &ProbeOpts::default()).await };
            jobs.push((i, probe));
        }
    }
    let mut gpus = std::collections::HashMap::new();
    each_pod(jobs, |_, _, i, probed| {
        if let Some((g, n)) = probed.ok().and_then(|m| m.gpu_summary().map(|g| (g, m.gpus.len() as u32))) {
            gpus.insert(*i, (g, n));
        }
    })
    .await;
    for (i, p) in pods.iter_mut().enumerate() {
        if let Some((g, n)) = gpus.remove(&i) {
            // A MIG slice keeps the provider's name for it: nvidia-smi names the parent card
            // (`B300 SXM6 AC`), which is not what the pod has (finding #24).
            if arena_core::health::mig_slice_name(p).is_none() {
                p.gpu_type = Some(g);
            }
            p.gpu_count = Some(n);
        }
    }
}

/// The capacity spares' reader for this config (its Hetzner project), when there's a Hetzner
/// key — never in this crate's unit tests (no key reaches the network).
pub(crate) fn spare_reader(cfg: &Config) -> Option<Box<dyn arena_core::spare::SpareCloud>> {
    if cfg!(test) {
        return None;
    }
    crate::image::hetzner(cfg).ok().map(|h| Box::new(h) as Box<dyn arena_core::spare::SpareCloud>)
}

/// `pods list`, as (stdout, stderr lines) — returned rather than printed, so the contract
/// is tested: every provider's pods (`Listing` from `list_by_provider`, so a provider that
/// failed is known: a partial failure is one warning line, a total failure the same error
/// as ever), best-effort details, the GPU probe — and, for the table only, the balance line
/// under the fleet footer. The balance is observed inline alongside the listing (within
/// `INLINE_TIMEOUT`; live only when the last attempt is `FRESH` old), judged against this
/// listing's burns after the details fill the prices — a provider that failed to list is
/// an unknown burn, never $0/h — and is silent whatever happens to it. `--json` reads no
/// balance at all and is unchanged byte for byte.
pub(crate) async fn list_with(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    bal: &money::BalanceCtx,
    json: bool,
    probe: bool,
) -> Result<(String, Vec<String>)> {
    use arena_core::balance::{self, BalancePolicy, Mode};
    let now = arena_core::snapshot::unix_now();
    let (state, _) = if json { Default::default() } else { balance::load_opt(bal.state_path()) };
    let listing = async {
        let listing = arena_core::proxy::Listing::from_results(provider.list_by_provider().await);
        let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        if !listing.any_ok() && !errs.is_empty() {
            anyhow::bail!("all providers failed to list: {}", errs.join("; "));
        }
        let mut warnings = Vec::new();
        if !errs.is_empty() {
            warnings.push(format!("warning: some providers failed to list ({})", errs.join("; ")));
        }
        let mut pods = listing.pods();
        pods.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
        // Best-effort details the list API omits (RunPod: GPU, $/h, host maintenance
        // window) — one extra read-only query. Never fatal: on failure or a hang the list
        // still renders, just with fewer columns filled, after one warning line.
        if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
            warnings.push(warning);
        }
        if probe {
            // nvidia-smi over SSH (same source as the TUI), concurrently, bounded per pod.
            probe_gpus(remote, cfg, &mut pods).await;
        }
        anyhow::Ok((listing, pods, warnings))
    };
    let mode = if json { Mode::CacheOnly } else { bal.inline_mode() };
    let observing = async {
        if json {
            Vec::new()
        } else {
            balance::observe(bal.api.as_ref(), &state, mode, balance::INLINE_TIMEOUT, now).await
        }
    };
    // The capacity spares (Hetzner): in no fleet listing, so read on their own — for the line
    // under the table and the spend ledger. Never under `--json` (unchanged byte for byte).
    let spares = async {
        let cloud = if json { None } else { spare_reader(cfg) }?;
        arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, cloud.spares()).await.ok()?.ok()
    };
    let (listed, fetched, spares) = tokio::join!(listing, observing, spares);
    let (listing, pods, mut warnings) = listed?;
    // What each machine costs, recorded for `arena spend` (quietly), as of when it listed.
    let tracking = spend::SpendCtx::from_config(cfg);
    tracking.listing(&listing, &pods, now);
    // SSH_CONFIG_FILE (and staff's SSH_ADMIN_CONFIG_FILE) kept current from this listing — a
    // complete one only (a provider that failed would drop its pods' blocks); nothing at all
    // when they're unset or nothing moved.
    if listing.errors().is_empty() {
        let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
        warnings.extend(ssh_config_lines("pods list", arena_core::sshconfig::sync_all(cfg, &pods, home.as_deref())));
    }
    if json {
        return Ok((format!("{}\n", serde_json::to_string_pretty(&pods)?), warnings));
    }
    let mut out = String::new();
    // Columns + footer come from arena_core::fleet so the TUI/snapshot reuse them. A
    // provider that didn't answer makes the total a lower bound — said on the line — and an
    // empty table "not known to be empty".
    use arena_core::fleet;
    let failed: Vec<String> = listing.errors().iter().map(|(p, _)| p.to_string()).collect();
    if pods.is_empty() {
        out.push_str(&format!("{}\n", fleet::empty_listing_line(&failed)));
    } else {
        // The reaper's ENDS column, only where it keeps state (ARENA_STATE_DIR set and a state
        // file there): unchanged for everyone not using it. The table only — never `--json`.
        let ends = reap::ends_column(cfg, &pods, now, reap::real::tz());
        // Who each instance is for (`--owner`, `spend assign`), from the spend ledger just
        // written: a column only when some pod has an owner.
        let owners = arena_core::spend::owners_column(&tracking.state(), &pods);
        out.push_str(&fleet::render_pods_table_full(&pods, owners.as_deref(), ends.as_deref()));
        out.push_str(&format!("{}\n", fleet::listing_footer(&fleet::fleet_cost(&pods), &failed)));
    }
    if let Some(spares) = spares.filter(|_| listing.status("hetzner") == arena_core::proxy::ListingStatus::Ok) {
        spend::SpendCtx::from_config(cfg).spares("hetzner", &spares, now);
        if let Some(line) = arena_core::spare::footer(&spares) {
            out.push_str(&format!("{line}\n"));
        }
    }
    if !fetched.is_empty() {
        let (policy, _) = BalancePolicy::from_config(cfg);
        let views = money::listed_views(bal, state, &fetched, &listing, &pods, &policy, mode, now);
        if let Some(line) = balance::footer_line(&views, &policy, now) {
            out.push_str(&format!("{line}\n"));
        }
    }
    Ok((out, warnings))
}

/// `pods list` enrichment must degrade, not fail: a provider error or a hang yields one
/// warning and the pods (as listed) still render.
#[cfg(test)]
mod list_tests {
    use super::enrich_best_effort;
    use arena_core::{Error, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use std::time::Duration;

    enum Mode {
        Fill,
        Fail,
        Hang,
    }

    struct EnrichFake(Mode);

    #[async_trait]
    impl Provider for EnrichFake {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised")
        }
        async fn stop_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            match self.0 {
                Mode::Fill => {
                    pods.iter_mut().for_each(|p| p.cost_per_hr = Some(0.17));
                    Ok(())
                }
                Mode::Fail => Err(Error::provider("pod details HTTP 429 Too Many Requests")),
                Mode::Hang => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(())
                }
            }
        }
    }

    fn pods() -> Vec<Pod> {
        vec![Pod { id: "a".into(), name: "devtest-apple".into(), provider: "runpod".into(), ..Default::default() }]
    }

    #[tokio::test]
    async fn success_fills_and_stays_quiet() {
        let mut p = pods();
        assert_eq!(enrich_best_effort(&EnrichFake(Mode::Fill), &mut p, Duration::from_secs(5)).await, None);
        assert_eq!(p[0].cost_per_hr, Some(0.17));
    }

    #[tokio::test]
    async fn failure_is_one_warning_and_pods_survive() {
        let mut p = pods();
        let w = enrich_best_effort(&EnrichFake(Mode::Fail), &mut p, Duration::from_secs(5)).await.unwrap();
        assert!(w.starts_with("warning: pod details incomplete") && w.contains("429"), "{w}");
        assert!(!w.contains('\n'), "one line: {w}");
        assert_eq!(p, pods()); // listing untouched, still renders
    }

    #[tokio::test]
    async fn hang_times_out_instead_of_blocking_the_list() {
        let mut p = pods();
        let w = enrich_best_effort(&EnrichFake(Mode::Hang), &mut p, Duration::from_millis(50)).await.unwrap();
        assert!(w.contains("timed out after 50ms"), "{w}");
        assert_eq!(p, pods());
    }
}
