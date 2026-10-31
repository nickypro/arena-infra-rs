//! `arena spend`: what each machine instance cost and who it was for — compute per currency
//! and AI keys per owner — for the organisers; `arena spend assign` to set or fix an owner;
//! and the recording that feeds it from the commands that already list or change machines.
//!
//! The ledger, its fold, the views and the renderers are `arena_core::spend` (pure, table
//! tested). Here is only the wiring: where the state file is ([`SpendCtx`]), the provider
//! wrapper that records creates (with their owner), stops, starts and terminates at the
//! moment they succeed ([`Tracked`]), the key handover a create for a new owner makes, and the
//! command itself. Every recording is **quiet**: a failure to write changes no output and no
//! exit code; only `arena spend` says what it couldn't read.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::openrouter::{KeyApi, KeyInfo};
use arena_core::provider::{ContainerReset, Provider};
use arena_core::proxy::Listing;
use arena_core::selector::Naming;
use arena_core::snapshot::unix_now;
use arena_core::spend::{self, Event, HttpSpendApi, OwnerRule, SpendApi};
use arena_core::{Config, Pod, PodSpec};

use super::money::Printed;

/// The budget for each read `arena spend` makes besides the listing (keys, billing, currency).
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Where this fleet's spend ledger is, and how `arena spend` reads billing.
pub(crate) struct SpendCtx {
    /// `None`: no state directory (or a unit test that didn't ask for one) — nothing recorded.
    path: Option<std::result::Result<PathBuf, String>>,
    pub api: Arc<dyn SpendApi>,
    /// The fleet's names (`MACHINE_NAME_PREFIX`, `MACHINE_NAME_LIST`): which machines and keys
    /// are this fleet's.
    prefix: String,
    list: Vec<String>,
}

impl SpendCtx {
    /// This fleet's ledger (`<state dir>/<prefix>/spend.json`) and the real reads — except in
    /// this crate's unit tests, where there is no ledger unless the test's config sets
    /// `ARENA_STATE_DIR`, and no key reaches the network.
    pub(crate) fn from_config(cfg: &Config) -> Self {
        let api_cfg = if cfg!(test) { Config::parse("") } else { cfg.clone() };
        let path = (!cfg!(test) || cfg.get("ARENA_STATE_DIR").is_some())
            .then(|| arena_core::snapshot::state_file_path_for(cfg, spend::SPEND_FILE));
        let naming = Naming::from_config(cfg);
        SpendCtx {
            path,
            api: Arc::new(HttpSpendApi::from_config(&api_cfg)),
            prefix: naming.prefix.to_string(),
            list: naming.list.to_vec(),
        }
    }

    fn naming(&self) -> Naming<'_> {
        Naming { prefix: &self.prefix, list: &self.list }
    }

    /// The ledger, when there is one.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_ref().and_then(|p| p.as_ref().ok()).map(PathBuf::as_path)
    }

    /// The ledger as it stands (empty without one, or unreadable).
    pub(crate) fn state(&self) -> spend::SpendState {
        self.path().map(|p| spend::load(p).0).unwrap_or_default()
    }

    /// Record a listing (a surface that listed every provider: `pods list`, teardown, …)
    /// begun `at` — when the reads began, not when they came back: a stop or a mint recorded
    /// meanwhile is newer than what the listing saw, and the fold keeps it.
    pub(crate) fn listing(&self, listing: &Listing, pods: &[Pod], at: u64) {
        spend::record_listing(self.path(), listing, pods, &self.naming(), at);
    }

    /// Record a complete listing of `provider`'s capacity spares, begun `at` (they're in no
    /// fleet listing).
    pub(crate) fn spares(&self, provider: &str, spares: &[Pod], at: u64) {
        spend::record_spares(self.path(), provider, spares, &self.naming(), at);
    }

    /// Record a complete key listing begun `at`: each key's usage, read before anything
    /// deletes one.
    pub(crate) fn keys(&self, keys: &[KeyInfo], at: u64) {
        spend::record_keys(self.path(), keys, &self.naming(), at);
    }

    /// Keys arena just deleted (`deleted`: hashes), with their final usage as `listed` just
    /// before the delete — recorded in the same write that marks them gone.
    pub(crate) fn keys_deleted(&self, listed: &[&KeyInfo], deleted: &[String]) {
        let gone: Vec<KeyInfo> = listed.iter().filter(|k| deleted.contains(&k.hash)).map(|k| (*k).clone()).collect();
        if gone.is_empty() {
            return;
        }
        let now = unix_now();
        spend::record_quietly(self.path(), now, |s| spend::keys_deleted(s, &gone, &self.naming(), now));
    }

    /// Whether the AI keys are due an hourly listing (the snapshot cron's).
    pub(crate) fn keys_due(&self) -> bool {
        self.path().is_some_and(|p| spend::keys_due(&spend::load(p).0, unix_now()))
    }

    /// A key arena just minted for `machine` (`owner`: decided by the caller — a handover).
    pub(crate) fn key_minted(&self, hash: &str, machine: &str, owner: Option<&str>) {
        let now = unix_now();
        spend::record_quietly(self.path(), now, |s| spend::key_minted(s, hash, machine, owner, None, now));
    }

    /// A lifecycle change arena just made.
    pub(crate) fn event(&self, ev: &Event) {
        let now = unix_now();
        spend::record_quietly(self.path(), now, |s| spend::apply_event(s, ev, now));
    }
}

/// The owner rule for a `pods` command: `--owner` on `create`/`up`; `replace`/`migrate` keep the
/// owner of the machine they replace; nothing else creates.
pub(crate) fn owner_rule(owner: Option<&str>, replaces: bool) -> OwnerRule {
    match owner {
        Some(o) => OwnerRule::Given(o.to_string()),
        None if replaces => OwnerRule::Inherit,
        None => OwnerRule::None,
    }
}

/// `--owner NAME`'s parser.
pub(crate) fn parse_owner_arg(s: &str) -> std::result::Result<String, String> {
    spend::parse_owner(s)
}

/// `--owner` / `--keep-key` on `pods create` and `pods up`: the only flags existing commands
/// gained for spend tracking.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct OwnerArgs {
    /// Who the new machines are for: recorded per instance for `arena spend`
    #[arg(long, value_name = "NAME", value_parser = parse_owner_arg)]
    pub owner: Option<String>,
    /// With --owner: keep each name's OpenRouter key even if it was minted for someone else
    #[arg(long, requires = "owner")]
    pub keep_key: bool,
}

/// The fleet provider, recording into the spend ledger each create (with the command's owner
/// rule), stop, start and terminate the moment it succeeds — `create`, `up` and its
/// replacements, `replace`, `migrate`, `stop`, `start` and `terminate` all go through these, so
/// none is missed and no command learns about owners. Everything else is the inner
/// provider's, method for method.
pub(crate) struct Tracked<'a> {
    pub inner: &'a dyn Provider,
    pub spend: &'a SpendCtx,
    pub owner: OwnerRule,
    /// Its creates are capacity spares (`pods spare up`): recorded as spares from the start.
    pub spares: bool,
    /// Which provider each id it listed or made is on (`None`: the same id on two), so a stop
    /// or terminate by id records against the right row.
    pub seen: std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
}

impl<'a> Tracked<'a> {
    pub(crate) fn new(inner: &'a dyn Provider, spend: &'a SpendCtx, owner: OwnerRule) -> Self {
        Tracked { inner, spend, owner, spares: false, seen: Default::default() }
    }

    /// The same, for `pods spare up`: each create is recorded as a spare's
    /// ([`Event::SpareCreated`]), never as a fleet machine's.
    pub(crate) fn for_spares(inner: &'a dyn Provider, spend: &'a SpendCtx) -> Self {
        Tracked { spares: true, ..Self::new(inner, spend, OwnerRule::None) }
    }

    fn remember<'p>(&self, pods: impl IntoIterator<Item = &'p Pod>) {
        let mut seen = self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for p in pods {
            let e = seen.entry(p.id.clone()).or_insert_with(|| Some(p.provider.clone()));
            if e.as_deref() != Some(p.provider.as_str()) {
                *e = None;
            }
        }
    }

    fn provider_of(&self, id: &str) -> Option<String> {
        self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).cloned().flatten()
    }
}

#[async_trait::async_trait]
impl Provider for Tracked<'_> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn describe(&self, spec: &PodSpec) -> String {
        self.inner.describe(spec)
    }
    fn backend(&self) -> Option<arena_core::capability::Backend> {
        self.inner.backend()
    }
    fn supports(&self, op: arena_core::capability::Op, pod: Option<&Pod>) -> arena_core::capability::Support {
        self.inner.supports(op, pod)
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        let pods = self.inner.list_pods().await?;
        self.remember(&pods);
        Ok(pods)
    }
    async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
        let listed = self.inner.list_by_provider().await;
        self.remember(listed.iter().filter_map(|(_, r)| r.as_ref().ok()).flatten());
        listed
    }
    async fn enrich(&self, pods: &mut [Pod]) -> arena_core::Result<()> {
        self.inner.enrich(pods).await
    }
    async fn create_pod(&self, spec: &PodSpec) -> arena_core::Result<Pod> {
        let pod = self.inner.create_pod(spec).await?;
        self.remember([&pod]);
        if self.spares {
            self.spend.event(&Event::SpareCreated { pod: &pod });
        } else {
            self.spend.event(&Event::Created { pod: &pod, owner: &self.owner });
        }
        Ok(pod)
    }
    async fn stop_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.stop_pod(id).await?;
        self.spend.event(&Event::Stopped { provider: self.provider_of(id).as_deref(), id });
        Ok(())
    }
    async fn stop_pod_outcome(&self, id: &str) -> arena_core::Result<arena_core::provider::StopOutcome> {
        let out = self.inner.stop_pod_outcome(id).await?;
        self.spend.event(&Event::Stopped { provider: self.provider_of(id).as_deref(), id });
        Ok(out)
    }
    async fn restart_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.restart_pod(id).await
    }
    fn restart_wipes_container_disk(&self, pod: &Pod) -> bool {
        self.inner.restart_wipes_container_disk(pod)
    }
    fn restart_resets_container(&self, pod: &Pod) -> ContainerReset {
        self.inner.restart_resets_container(pod)
    }
    async fn start_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.start_pod(id).await?;
        self.spend.event(&Event::Started { provider: self.provider_of(id).as_deref(), id });
        Ok(())
    }
    fn ready_deadline(&self, pod: &Pod) -> Option<std::time::Duration> {
        self.inner.ready_deadline(pod)
    }
    async fn terminate_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.terminate_pod(id).await?;
        self.spend.event(&Event::Terminated { provider: self.provider_of(id).as_deref(), id });
        Ok(())
    }
    async fn terminate_pod_on(&self, provider: &str, id: &str) -> arena_core::Result<()> {
        self.inner.terminate_pod_on(provider, id).await?;
        self.spend.event(&Event::Terminated { provider: Some(provider), id });
        Ok(())
    }
    async fn rename_pod(&self, id: &str, new_name: &str) -> arena_core::Result<()> {
        self.inner.rename_pod(id, new_name).await
    }
    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> arena_core::Result<()> {
        self.inner.reimage_pod(id, image, env).await
    }
    async fn authorize_ssh_keys(&self, pod: &Pod, keys: &[String]) -> arena_core::Result<()> {
        self.inner.authorize_ssh_keys(pod, keys).await
    }
    async fn set_locked(&self, id: &str, locked: bool) -> arena_core::Result<()> {
        self.inner.set_locked(id, locked).await
    }
    fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> arena_core::Result<()> {
        self.inner.check_create_extra(extra)
    }
    fn preview_create_body(&self, spec: &PodSpec) -> arena_core::Result<serde_json::Value> {
        self.inner.preview_create_body(spec)
    }
    async fn pod_spec(&self, id: &str) -> arena_core::Result<PodSpec> {
        self.inner.pod_spec(id).await
    }
}

// ---------------------------------------------------------------------------------------
// The key handover (`pods up/create --owner B` for a name whose key was A's)
// ---------------------------------------------------------------------------------------

/// A name about to get a new instance for `owner` whose current AI key belongs to someone
/// else: that key is rotated first, so the old owner keeps what it spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handover {
    pub name: String,
    pub from: String,
}

/// The handovers creating `names` for `owner` makes (none with `--keep-key`, without an
/// owner, or with no ledger yet). No ledger to be had (no state directory resolves) or one
/// that can't be read is an error: whose keys these names hold is unknown, and creating
/// anyway could hand one person's key to another.
pub(crate) fn handovers(ctx: &SpendCtx, names: &[String], owner: Option<&str>, keep_key: bool) -> Result<Vec<Handover>> {
    let Some(owner) = owner.filter(|_| !keep_key) else { return Ok(Vec::new()) };
    let path = match &ctx.path {
        Some(Ok(p)) => p,
        Some(Err(e)) => anyhow::bail!(
            "no spend ledger ({e}), so whose OpenRouter keys these names hold is unknown — set ARENA_STATE_DIR (or HOME), \
             or pass --keep-key"
        ),
        // A unit test that didn't ask for a ledger.
        None => return Ok(Vec::new()),
    };
    let state = match spend::load(path) {
        (s, spend::Loaded::Ok | spend::Loaded::Missing) => s,
        (_, spend::Loaded::Ignored(why)) => anyhow::bail!(
            "the spend ledger {} can't be used ({why}), so whose OpenRouter keys these names hold is unknown. \
             `arena spend` moves it aside and starts afresh (owners recorded so far are lost); or pass --keep-key",
            path.display()
        ),
        (_, spend::Loaded::Unreadable(why)) => anyhow::bail!(
            "the spend ledger {} can't be read ({why}), so whose OpenRouter keys these names hold is unknown — \
             fix that, or pass --keep-key",
            path.display()
        ),
    };
    // A ledger restarted since a name's key was made: its owner may have been lost with the
    // old one, and this create would make it the new owner's, with what it spent.
    let lost: Vec<(&String, &spend::Restart)> = names.iter().filter_map(|n| spend::owner_lost(&state, n).map(|r| (n, r))).collect();
    if let Some((_, r)) = lost.first() {
        let names: Vec<&str> = lost.iter().map(|(n, _)| n.as_str()).collect();
        anyhow::bail!(
            "the OpenRouter key of {} may be someone else's: the spend ledger started afresh at {} (the old one is {}) \
             and whose it is isn't recorded since. `arena spend` (it lists the keys), then `arena spend assign <key hash> \
             <owner>` (`arena keys list` has the hashes) or `arena keys rotate <name>` — or pass --keep-key",
            names.join(", "),
            arena_core::snapshot::rfc3339(r.at),
            r.aside
        );
    }
    Ok(names
        .iter()
        .filter_map(|n| spend::handover_needed(&state, n, owner).map(|from| Handover { name: n.clone(), from: from.to_string() }))
        .collect())
}

/// What a dry run (or the plan before the prompt) says the owner and the handovers will do.
pub(crate) fn owner_lines(owner: Option<&str>, names: &[String], handovers: &[Handover], keep_key: bool, dry_run: bool) -> Vec<String> {
    let Some(owner) = owner else { return Vec::new() };
    let tag = if dry_run { "[dry-run] would record" } else { "will record" };
    let mut out = vec![format!("{tag} owner {owner} for the new instance(s) of: {}", names.join(", "))];
    for h in handovers {
        let verb = if dry_run { "[dry-run] would rotate" } else { "will rotate" };
        out.push(format!(
            "{verb} {}'s OpenRouter key first: it was {}'s (their spend stays theirs), {owner} gets a fresh one \
             (--keep-key: don't)",
            h.name, h.from
        ));
    }
    if keep_key {
        out.push("--keep-key: each name keeps its current OpenRouter key (what's spent on it stays with that key's owner)".into());
    }
    out
}

/// What [`hand_over`] did.
#[derive(Debug, Default)]
pub(crate) struct HandedOver {
    /// `✓`/`✗` lines, one per name (or one for them all).
    pub lines: Vec<String>,
    /// Names whose handover failed: nothing should be created for them.
    pub failed: Vec<String>,
    /// Whether the keys CSV changed (a fresh key saved, or a dead one's row dropped).
    pub csv_changed: bool,
}

/// Rotate each handover's key: one key listing (every key's usage recorded first), the name's
/// keys deleted (their final usage recorded with the delete), a fresh key minted for `owner`
/// and written to the keys CSV in `dir`. A name whose handover failed is in `failed`; if its
/// old key was deleted but no fresh one saved, its CSV row is dropped, so no new machine gets
/// a dead key.
pub(crate) async fn hand_over(ctx: &SpendCtx, cfg: &Config, api: Option<&dyn KeyApi>, dir: &Path, handovers: &[Handover], owner: &str) -> HandedOver {
    let mut done = HandedOver::default();
    if handovers.is_empty() {
        return done;
    }
    let names = || handovers.iter().map(|h| h.name.clone()).collect::<Vec<_>>();
    let Some(api) = api else {
        done.lines.push("✗ key handover: OPENROUTER_PROVISIONING_KEY isn't set — the keys can't be rotated".into());
        done.failed = names();
        return done;
    };
    let listed_at = unix_now();
    let keys = match api.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            done.lines.push(format!("✗ key handover: listing OpenRouter keys failed ({e})"));
            done.failed = names();
            return done;
        }
    };
    ctx.keys(&keys, listed_at);
    let naming = Naming::from_config(cfg);
    let prefix = naming.prefix.to_string();
    let limit = cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").unwrap_or(5.0);
    for h in handovers {
        let theirs: Vec<&KeyInfo> = super::machine_key_names(&naming, &h.name)
            .iter()
            .flat_map(|n| keys.iter().filter(move |k| k.name.as_deref() == Some(n.as_str())))
            .collect();
        let kn = arena_core::openrouter::key_name(&prefix, naming.list, &h.name);
        let (deleted, saved) = match super::rotate_key(api, &kn, &theirs, limit).await {
            Ok((deleted, ck)) => {
                ctx.keys_deleted(&theirs, &deleted);
                ctx.key_minted(&ck.hash, &h.name, Some(owner));
                let short = ck.hash[..ck.hash.len().min(12)].to_string();
                match super::write_openrouter_key(dir, &h.name, &ck.secret, &prefix) {
                    Ok(()) => {
                        done.lines.push(format!("✓ {}: OpenRouter key handed over ({} → {owner}, a fresh key {short})", h.name, h.from));
                        done.csv_changed = true;
                        (deleted, true)
                    }
                    Err(e) => {
                        // Its secret is lost (OpenRouter shows it once): revoke it rather than
                        // leave a live key nobody holds.
                        let fresh = KeyInfo { hash: ck.hash.clone(), name: Some(kn.clone()), usage: Some(0.0), ..KeyInfo::default() };
                        let revoked = api.delete_key(&ck.hash).await.is_ok();
                        if revoked {
                            ctx.keys_deleted(&[&fresh], std::slice::from_ref(&ck.hash));
                        }
                        let then = if revoked { "revoked it".to_string() } else { format!("revoke it: `arena keys revoke {}`", h.name) };
                        done.lines.push(format!("✗ {}: a fresh key {short} was minted for {owner} but couldn't be saved ({e:#}) — {then}", h.name));
                        (deleted, false)
                    }
                }
            }
            Err((deleted, why)) => {
                ctx.keys_deleted(&theirs, &deleted);
                done.lines.push(format!("✗ {}: key handover failed ({why})", h.name));
                (deleted, false)
            }
        };
        if !saved {
            done.failed.push(h.name.clone());
            // Its old key is gone and no fresh one saved: its CSV row would hand a dead key on.
            if !deleted.is_empty() {
                match super::drop_key_row(dir, &h.name) {
                    Ok(dropped) => done.csv_changed |= dropped,
                    Err(e) => done.lines.push(format!(
                        "✗ {}: its deleted key's row couldn't be removed from the keys CSV ({e}) — `arena keys revoke {}` drops it",
                        h.name, h.name
                    )),
                }
            }
        }
    }
    done
}

// ---------------------------------------------------------------------------------------
// `arena spend`
// ---------------------------------------------------------------------------------------

/// `arena spend assign`.
#[derive(clap::Subcommand, Debug, Clone)]
pub(crate) enum SpendCmd {
    /// Set or fix the owner of a machine, an instance id or an AI key
    Assign {
        /// A machine name (its running instance), an instance id, or an AI key hash
        target: String,
        /// The owner (a person's name, as in --owner)
        #[arg(value_parser = parse_owner_arg, required_unless_present = "clear")]
        owner: Option<String>,
        /// Remove the owner instead (the row goes back to "(no owner)")
        #[arg(long, conflicts_with = "owner")]
        clear: bool,
    },
}

/// What `arena spend` shows.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpendFlags {
    pub by_machine: bool,
    pub since: Option<u64>,
    pub csv: bool,
    pub json: bool,
}

/// `--by`'s values.
pub(crate) const BY: [&str; 2] = ["owner", "machine"];

/// `arena spend [--by machine] [--since DATE] [--csv|--json]`: list the fleet, the AI keys,
/// RunPod's billing per pod and the account's, and Hetzner's currency — all at once, each
/// bounded — record what was seen, then report from the ledger. What couldn't be read is a
/// warning on stderr; the exit is 0 unless there's no ledger to report from.
pub(crate) async fn spend_report(
    provider: &dyn Provider,
    cfg: &Config,
    ctx: &SpendCtx,
    keys: Option<&dyn KeyApi>,
    flags: &SpendFlags,
    now: u64,
) -> Result<Printed> {
    let path = match &ctx.path {
        Some(Ok(p)) => p.clone(),
        Some(Err(e)) => anyhow::bail!("no spend ledger: {e}"),
        None => anyhow::bail!("no spend ledger: no state directory"),
    };
    let mut stderr = Vec::new();
    let naming = Naming::from_config(cfg);
    let (before, _) = spend::load(&path);
    let late = || format!("no answer in {}s", READ_TIMEOUT.as_secs());
    let listing = async {
        let listing = Listing::from_results(provider.list_by_provider().await);
        let mut pods = listing.pods();
        // RunPod v1's listing has no price: the details query fills it, as for `pods list`.
        let warning = super::enrich_best_effort(provider, &mut pods, super::ENRICH_TIMEOUT).await;
        (listing, pods, warning)
    };
    let key_list = async {
        match keys {
            Some(api) => Some(match within(api.list_keys()).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(()) => Err(late()),
            }),
            None => None,
        }
    };
    let currency = async { within(ctx.api.hetzner_currency()).await.unwrap_or_else(|()| Some(Err(late()))) };
    // The capacity spares (Hetzner) are in no fleet listing: read on their own.
    let spares = async {
        let cloud = crate::pods::list::spare_reader(cfg)?;
        Some(match within(cloud.spares()).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(()) => Err(late()),
        })
    };
    // Recorded and reported as of `now`, when the reads began: what a command recorded
    // meanwhile is newer, and stands.
    let ((listing, pods, enrich_warning), key_list, currency, spares) = tokio::join!(listing, key_list, currency, spares);
    stderr.extend(enrich_warning);
    let spares = match spares {
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => {
            stderr.push(format!("warning: the capacity spares not read ({e}) — counted to when they were last seen"));
            None
        }
        None => None,
    };
    // Billing from where the window (or the ledger) begins.
    let from = arena_core::balance::floor_hour(flags.since.or(before.since).unwrap_or(now).min(now));
    let (pod_bill, account) = tokio::join!(
        async { within(ctx.api.pod_billing(from, now, now)).await.unwrap_or_else(|()| Some(Err(late()))) },
        async { within(ctx.api.account_billing(from, now)).await.unwrap_or_else(|()| Some(Err(late()))) },
    );
    for (p, e) in listing.errors() {
        stderr.push(format!("warning: {p} failed to list ({e}) — its machines are counted to when they were last seen"));
    }
    let keys_ok = match &key_list {
        Some(Ok(k)) => Some(k.as_slice()),
        Some(Err(e)) => {
            stderr.push(format!("warning: OpenRouter keys not read ({e}) — AI spend as last recorded"));
            None
        }
        None => None,
    };
    let currency_read = match currency {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => {
            stderr.push(format!(
                "warning: Hetzner's currency not read ({e}) — {} as last read",
                before.currencies.get("hetzner").map_or("EUR (arena's default)", String::as_str)
            ));
            None
        }
        None => None,
    };
    let (state, warning) = spend::record(&path, now, |mut s| {
        s = spend::observe(s, &listing, &pods, &naming, now);
        if let Some(sp) = &spares {
            s = spend::observe_spares(s, "hetzner", sp, &naming, now);
        }
        if let Some(k) = keys_ok {
            s = spend::observe_keys(s, k, &naming, now);
        }
        if let Some(c) = &currency_read {
            s.currencies.insert("hetzner".into(), c.clone());
        }
        s
    })
    .with_context(|| format!("recording into {}", path.display()))?;
    stderr.extend(warning);
    let pod_bill = match pod_bill {
        Some(Ok(b)) => Some(b),
        Some(Err(e)) => {
            stderr.push(format!("warning: RunPod's per-pod billing not read ({e}) — RunPod rows are metered only"));
            None
        }
        None => None,
    };
    let account = match account {
        Some(Ok(b)) => Some(b),
        Some(Err(e)) => {
            stderr.push(format!("warning: RunPod's account billing not read ({e}) — no accounted-for line"));
            None
        }
        None => None,
    };
    let report = spend::report(&state, &naming, flags.since, now, pod_bill.as_ref(), account.as_ref());
    let stdout = match (flags.json, flags.csv, flags.by_machine) {
        (true, _, _) => format!("{}\n", serde_json::to_string_pretty(&report)?),
        (_, true, true) => spend::csv_machines(&report),
        (_, true, false) => spend::csv_owners(&report),
        (_, false, true) => spend::render_machines(&report, &naming),
        (_, false, false) => spend::render_owners(&report),
    };
    Ok(Printed { stdout, stderr, code: 0 })
}

/// `f`, within [`READ_TIMEOUT`] (its rate-limit waits included).
async fn within<F: std::future::Future>(f: F) -> std::result::Result<F::Output, ()> {
    arena_core::ratelimit::bounded(READ_TIMEOUT, f).await.map_err(|_| ())
}

/// `arena spend assign <target> <owner>` / `--clear`: list the fleet first (a machine made a
/// moment ago is in the ledger then), set the owner, and say what changed — with a hint when
/// the machine's AI key is someone else's.
pub(crate) async fn spend_assign(provider: &dyn Provider, cfg: &Config, ctx: &SpendCtx, target: &str, owner: Option<&str>, now: u64) -> Result<String> {
    let path = match &ctx.path {
        Some(Ok(p)) => p.clone(),
        Some(Err(e)) => anyhow::bail!("no spend ledger: {e}"),
        None => anyhow::bail!("no spend ledger: no state directory"),
    };
    let naming = Naming::from_config(cfg);
    let listing = Listing::from_results(provider.list_by_provider().await);
    let pods = listing.pods();
    let bare = target.strip_prefix('@').unwrap_or(target);
    let names = vec![target.to_string(), bare.to_string(), arena_core::naming::canonical_name(naming.prefix, naming.list, target)];
    let mut outcome = None;
    let (state, warning) = spend::record(&path, now, |s| {
        let s = spend::observe(s, &listing, &pods, &naming, now);
        match spend::assign(s.clone(), &names, target, owner, now) {
            Ok((next, what)) => {
                outcome = Some(Ok(what));
                next
            }
            Err(e) => {
                outcome = Some(Err(e));
                s
            }
        }
    })
    .with_context(|| format!("recording into {}", path.display()))?;
    let what = outcome.expect("the closure ran").map_err(|e| anyhow::anyhow!(e))?;
    let mut out = String::new();
    if let Some(w) = warning {
        out.push_str(&format!("{w}\n"));
    }
    let new = owner.unwrap_or(spend::NO_OWNER);
    match what {
        spend::Assigned::Machine { name, provider, id, was } => {
            out.push_str(&format!("{name} ({provider} {id}): owner {} → {new}\n", was.as_deref().unwrap_or(spend::NO_OWNER)));
            if let (Some(o), Some(k)) = (owner, spend::key_owner(&state, &name)) {
                if k != o {
                    out.push_str(&format!(
                        "  its OpenRouter key is {k}'s: `arena keys rotate {name}` gives {o} a fresh one ({k} keeps what it spent)\n"
                    ));
                }
            }
        }
        spend::Assigned::Key { name, hash, was } => {
            out.push_str(&format!(
                "AI key {} ({name}): owner {} → {new}\n",
                &hash[..hash.len().min(12)],
                was.as_deref().unwrap_or(spend::NO_OWNER)
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::openrouter::CreatedKey;
    use std::sync::Mutex;

    /// A Hetzner project in memory: creates add a server (€0.01/h), terminates remove it.
    #[derive(Default)]
    struct FakeProvider {
        pods: Mutex<Vec<Pod>>,
        next: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            "hetzner"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(self.pods.lock().unwrap().clone())
        }
        async fn create_pod(&self, spec: &PodSpec) -> arena_core::Result<Pod> {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            let pod = Pod {
                id: format!("{}", 1000 + *n),
                name: spec.name.clone(),
                provider: "hetzner".into(),
                status: "running".into(),
                gpu_type: Some("cx23".into()),
                cost_per_hr: Some(0.01),
                ..Pod::default()
            };
            self.pods.lock().unwrap().push(pod.clone());
            Ok(pod)
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, id: &str) -> arena_core::Result<()> {
            self.pods.lock().unwrap().retain(|p| p.id != id);
            Ok(())
        }
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-cli-spend-{tag}-{}-{}", std::process::id(), unix_now()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn cfg(state: &Path) -> Config {
        Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nOPENROUTER_KEY_LIMIT=1\n", state.display()))
    }

    /// Keys on a fake OpenRouter: listed with their usage, deleted and minted as asked.
    #[derive(Default)]
    struct FakeKeys {
        keys: Mutex<Vec<KeyInfo>>,
        deleted: Mutex<Vec<String>>,
        minted: Mutex<u32>,
        /// Creates fail (OpenRouter down, the workspace at its key limit).
        refuse_create: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl KeyApi for FakeKeys {
        async fn list_keys(&self) -> arena_core::Result<Vec<KeyInfo>> {
            Ok(self.keys.lock().unwrap().clone())
        }
        async fn create_key(&self, name: &str, _limit: Option<f64>) -> arena_core::Result<CreatedKey> {
            if *self.refuse_create.lock().unwrap() {
                return Err(arena_core::Error::provider("HTTP 503"));
            }
            let mut n = self.minted.lock().unwrap();
            *n += 1;
            let hash = format!("hash{n}aaaaaaaa");
            self.keys.lock().unwrap().push(KeyInfo { hash: hash.clone(), name: Some(name.into()), usage: Some(0.0), ..KeyInfo::default() });
            Ok(CreatedKey { secret: format!("sk-or-v1-{n}"), hash, name: name.into() })
        }
        async fn delete_key(&self, hash: &str) -> arena_core::Result<()> {
            self.keys.lock().unwrap().retain(|k| k.hash != hash);
            self.deleted.lock().unwrap().push(hash.into());
            Ok(())
        }
        async fn rename_key(&self, _hash: &str, _new_name: &str) -> arena_core::Result<()> {
            Ok(())
        }
    }

    fn report_flags() -> SpendFlags {
        SpendFlags::default()
    }

    #[tokio::test]
    async fn the_tracked_provider_records_a_create_with_its_owner_and_a_terminate_at_once() {
        let d = tmp("tracked");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let fake = FakeProvider::default();
        let tracked = Tracked::new(&fake, &ctx, OwnerRule::Given("alice".into()));
        let spec = PodSpec { name: "devtest-alpha".into(), ..PodSpec::default() };
        let pod = tracked.create_pod(&spec).await.unwrap();
        let s = ctx.state();
        assert_eq!(s.machines.len(), 1);
        assert_eq!((s.machines[0].name.as_str(), s.machines[0].owner.as_deref()), ("devtest-alpha", Some("alice")));
        tracked.terminate_pod(&pod.id).await.unwrap();
        let s = ctx.state();
        assert_eq!(s.machines[0].ended.map(|e| e.how), Some(spend::EndHow::Terminated));
        // No ledger (a test config without ARENA_STATE_DIR): nothing written, nothing fails.
        let none = SpendCtx::from_config(&Config::parse("MACHINE_NAME_PREFIX=devtest\n"));
        assert!(none.path().is_none());
        let t = Tracked::new(&fake, &none, OwnerRule::None);
        t.create_pod(&PodSpec { name: "devtest-b".into(), ..PodSpec::default() }).await.unwrap();
    }

    #[tokio::test]
    async fn spend_reports_per_owner_with_a_no_owner_row_and_assign_fixes_it() {
        let d = tmp("report");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx { api: Arc::new(spend::FakeSpendApi::default()), ..SpendCtx::from_config(&cfg) };
        let fake = FakeProvider::default();
        let with = Tracked::new(&fake, &ctx, OwnerRule::Given("alice".into()));
        with.create_pod(&PodSpec { name: "devtest-alpha".into(), ..PodSpec::default() }).await.unwrap();
        let without = Tracked::new(&fake, &ctx, OwnerRule::None);
        without.create_pod(&PodSpec { name: "devtest-bravo".into(), ..PodSpec::default() }).await.unwrap();
        let out = spend_report(&fake, &cfg, &ctx, None, &report_flags(), unix_now()).await.unwrap();
        assert!(out.stdout.lines().nth(1).unwrap().starts_with("alice"), "{}", out.stdout);
        assert!(out.stdout.lines().nth(2).unwrap().starts_with("(no owner)") && out.stdout.contains("bravo"), "{}", out.stdout);
        assert_eq!(out.code, 0);
        let said = spend_assign(&fake, &cfg, &ctx, "bravo", Some("bob"), unix_now()).await.unwrap();
        assert!(said.contains("devtest-bravo") && said.contains("(no owner) → bob"), "{said}");
        let out = spend_report(&fake, &cfg, &ctx, None, &SpendFlags { csv: true, by_machine: true, ..Default::default() }, unix_now()).await.unwrap();
        assert!(out.stdout.starts_with("name,id,provider"), "{}", out.stdout);
        assert!(out.stdout.lines().any(|l| l.starts_with("devtest-bravo,") && l.contains(",bob,")), "{}", out.stdout);
        let json = spend_report(&fake, &cfg, &ctx, None, &SpendFlags { json: true, ..Default::default() }, unix_now()).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        assert_eq!(v["owners"].as_array().unwrap().len(), 2);
        assert!(spend_assign(&fake, &cfg, &ctx, "nope", Some("x"), unix_now()).await.is_err());
    }

    #[tokio::test]
    async fn a_create_for_a_new_owner_hands_the_names_key_over_keeping_the_old_owners_spend() {
        let d = tmp("handover");
        let keys_dir = d.0.join("keys");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let fake = FakeProvider::default();
        let api = FakeKeys::default();
        // alice's machine and its key, minted for her; she spends $1.25 on it.
        let alice = Tracked::new(&fake, &ctx, OwnerRule::Given("alice".into()));
        let pod = alice.create_pod(&PodSpec { name: "devtest-alpha".into(), ..PodSpec::default() }).await.unwrap();
        let ck = api.create_key("devtest-alpha", Some(1.0)).await.unwrap();
        ctx.key_minted(&ck.hash, "devtest-alpha", None);
        api.keys.lock().unwrap()[0].usage = Some(1.25);
        alice.terminate_pod(&pod.id).await.unwrap();
        let names = vec!["devtest-alpha".to_string()];
        let h = handovers(&ctx, &names, Some("bob"), false).unwrap();
        assert_eq!(h, [Handover { name: "devtest-alpha".into(), from: "alice".into() }]);
        assert!(handovers(&ctx, &names, Some("bob"), true).unwrap().is_empty(), "--keep-key");
        assert!(handovers(&ctx, &names, Some("alice"), false).unwrap().is_empty(), "the same owner");
        let plan = owner_lines(Some("bob"), &names, &h, false, true);
        assert!(plan[1].contains("would rotate devtest-alpha's OpenRouter key first: it was alice's"), "{plan:?}");
        let said = hand_over(&ctx, &cfg, Some(&api as &dyn KeyApi), &keys_dir, &h, "bob").await;
        assert!(said.lines[0].starts_with("✓ devtest-alpha: OpenRouter key handed over (alice → bob"), "{said:?}");
        assert_eq!(api.deleted.lock().unwrap().as_slice(), std::slice::from_ref(&ck.hash));
        let csv = std::fs::read_to_string(keys_dir.join("openrouter_api_keys.csv")).unwrap();
        assert!(csv.contains("devtest-alpha,sk-or-v1-2"), "{csv}");
        // bob's machine; the report keeps alice's $1.25 hers.
        let bob = Tracked::new(&fake, &ctx, OwnerRule::Given("bob".into()));
        bob.create_pod(&PodSpec { name: "devtest-alpha".into(), ..PodSpec::default() }).await.unwrap();
        let ctx = SpendCtx { api: Arc::new(spend::FakeSpendApi::default()), ..SpendCtx::from_config(&cfg) };
        let out = spend_report(&fake, &cfg, &ctx, Some(&api as &dyn KeyApi), &SpendFlags { csv: true, ..Default::default() }, unix_now()).await.unwrap();
        let alice_row = out.stdout.lines().find(|l| l.starts_with("alice,")).unwrap_or_else(|| panic!("{}", out.stdout));
        assert!(alice_row.contains(",1.2500,"), "{alice_row}");
        assert!(out.stdout.lines().any(|l| l.starts_with("bob,alpha,1,")), "{}", out.stdout);
    }

    #[tokio::test]
    async fn a_failed_handover_is_named_and_leaves_no_dead_key_in_the_csv() {
        let d = tmp("handfail");
        let keys_dir = d.0.join("keys");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let api = FakeKeys::default();
        let ck = api.create_key("devtest-alpha", Some(1.0)).await.unwrap();
        crate::write_openrouter_key(&keys_dir, "devtest-alpha", &ck.secret, "devtest").unwrap();
        ctx.key_minted(&ck.hash, "devtest-alpha", Some("alice"));
        api.keys.lock().unwrap()[0].usage = Some(0.75);
        let names = vec!["devtest-alpha".to_string()];
        let h = handovers(&ctx, &names, Some("bob"), false).unwrap();
        // The old key is deleted, the fresh one can't be minted.
        *api.refuse_create.lock().unwrap() = true;
        let done = hand_over(&ctx, &cfg, Some(&api as &dyn KeyApi), &keys_dir, &h, "bob").await;
        assert_eq!(done.failed, names, "{done:?}");
        assert!(done.lines[0].starts_with("✗ devtest-alpha: key handover failed (create:"), "{done:?}");
        assert!(done.csv_changed, "the dead key's row went");
        let csv = std::fs::read_to_string(keys_dir.join("openrouter_api_keys.csv")).unwrap();
        assert!(!csv.contains(&ck.secret), "{csv}");
        // alice's key: deleted by arena, its last usage kept hers.
        let k = &ctx.state().keys[0];
        assert_eq!((k.owner.as_deref(), k.last_usage(), k.gone.map(|g| g.how)), (Some("alice"), 0.75, Some(spend::KeyEndHow::Deleted)));
        // A retry: no live key for the name, so nothing to hand over — and none of alice's
        // left to copy onto bob's machine (its row went).
        assert!(handovers(&ctx, &names, Some("bob"), false).unwrap().is_empty());
        // No provisioning key, or a listing that fails: every name failed, nothing deleted.
        let none = hand_over(&ctx, &cfg, None, &keys_dir, &[Handover { name: "devtest-b".into(), from: "al".into() }], "bob").await;
        assert_eq!((none.failed.as_slice(), none.csv_changed), (["devtest-b".to_string()].as_slice(), false));
    }

    #[tokio::test]
    async fn a_fresh_key_that_cant_be_saved_is_revoked() {
        let d = tmp("unsaved");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let api = FakeKeys::default();
        let ck = api.create_key("devtest-alpha", Some(1.0)).await.unwrap();
        ctx.key_minted(&ck.hash, "devtest-alpha", Some("alice"));
        let names = vec!["devtest-alpha".to_string()];
        let h = handovers(&ctx, &names, Some("bob"), false).unwrap();
        // The keys folder can't be made (a file is in the way).
        std::fs::write(d.0.join("file"), "").unwrap();
        let done = hand_over(&ctx, &cfg, Some(&api as &dyn KeyApi), &d.0.join("file").join("keys"), &h, "bob").await;
        assert_eq!(done.failed, names);
        assert!(done.lines[0].contains("couldn't be saved") && done.lines[0].ends_with("revoked it"), "{done:?}");
        assert_eq!(api.deleted.lock().unwrap().len(), 2, "alice's key, then the unsaved one");
        assert!(api.keys.lock().unwrap().is_empty());
        assert!(ctx.state().keys.iter().all(|k| k.gone.is_some()), "{:?}", ctx.state().keys);
        assert!(handovers(&ctx, &names, Some("bob"), false).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dead_keys_row_that_cant_be_removed_is_said() {
        let d = tmp("deadrow");
        let keys_dir = d.0.join("keys");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let api = FakeKeys::default();
        let ck = api.create_key("devtest-alpha", Some(1.0)).await.unwrap();
        ctx.key_minted(&ck.hash, "devtest-alpha", Some("alice"));
        // The keys CSV can't be read (a folder in its place).
        std::fs::create_dir_all(keys_dir.join("openrouter_api_keys.csv")).unwrap();
        let h = handovers(&ctx, &["devtest-alpha".to_string()], Some("bob"), false).unwrap();
        *api.refuse_create.lock().unwrap() = true;
        let done = hand_over(&ctx, &cfg, Some(&api as &dyn KeyApi), &keys_dir, &h, "bob").await;
        assert!(done.lines.iter().any(|l| l.contains("couldn't be removed from the keys CSV") && l.contains("arena keys revoke devtest-alpha")), "{done:?}");
        assert!(!done.csv_changed);
    }

    #[test]
    fn a_key_whose_owner_a_restart_may_have_lost_stops_a_create_for_an_owner() {
        let d = tmp("lostowner");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let path = ctx.path().unwrap().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        // A quiet recorder moves it aside; then the key (made before) is listed with no owner.
        let now = unix_now();
        let old = KeyInfo { hash: "kOLD12345".into(), name: Some("devtest-alpha".into()), usage: Some(3.0), created_at: Some("2026-10-01T00:00:00Z".into()), ..KeyInfo::default() };
        ctx.keys(std::slice::from_ref(&old), now);
        let names = vec!["devtest-alpha".to_string()];
        let err = handovers(&ctx, &names, Some("bob"), false).unwrap_err().to_string();
        assert!(err.contains("devtest-alpha") && err.contains("started afresh") && err.contains("spend assign"), "{err}");
        assert!(handovers(&ctx, &names, Some("bob"), true).unwrap().is_empty(), "--keep-key");
        assert!(handovers(&ctx, &["devtest-bravo".to_string()], Some("bob"), false).unwrap().is_empty(), "another name");
    }

    #[test]
    fn an_unreadable_ledger_stops_a_create_for_an_owner_unless_keep_key() {
        let d = tmp("badledger");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        let names = vec!["devtest-alpha".to_string()];
        assert!(handovers(&ctx, &names, Some("bob"), false).unwrap().is_empty(), "no ledger yet: nothing to hand over");
        let path = ctx.path().unwrap().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let err = handovers(&ctx, &names, Some("bob"), false).unwrap_err().to_string();
        assert!(err.contains("can't be used") && err.contains("--keep-key"), "{err}");
        assert!(handovers(&ctx, &names, Some("bob"), true).unwrap().is_empty());
        assert!(handovers(&ctx, &names, None, false).unwrap().is_empty(), "no --owner: no question asked");
        // One that can't be read at all (permissions): the same, and the file is left alone.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_err() {
            let err = handovers(&ctx, &names, Some("bob"), false).unwrap_err().to_string();
            assert!(err.contains("can't be read") && err.contains("--keep-key"), "{err}");
            assert!(handovers(&ctx, &names, Some("bob"), true).unwrap().is_empty());
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn no_state_directory_stops_a_create_for_an_owner_unless_keep_key() {
        // A relative ARENA_STATE_DIR resolves to no ledger at all.
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR=relative/state\n");
        let ctx = SpendCtx::from_config(&cfg);
        assert!(ctx.path().is_none());
        let names = vec!["devtest-alpha".to_string()];
        let err = handovers(&ctx, &names, Some("bob"), false).unwrap_err().to_string();
        assert!(err.contains("no spend ledger") && err.contains("absolute") && err.contains("--keep-key"), "{err}");
        assert!(handovers(&ctx, &names, Some("bob"), true).unwrap().is_empty(), "--keep-key");
        assert!(handovers(&ctx, &names, None, false).unwrap().is_empty(), "no --owner");
    }

    #[tokio::test]
    async fn a_stop_by_id_records_against_the_provider_that_listed_it() {
        let d = tmp("twoids");
        let cfg = cfg(&d.0);
        let ctx = SpendCtx::from_config(&cfg);
        // Instance 77 on Hetzner and on Vast; the command's provider lists only Vast's.
        let hz = Pod { id: "77".into(), name: "devtest-a".into(), provider: "hetzner".into(), status: "running".into(), cost_per_hr: Some(0.01), ..Pod::default() };
        let vast = Pod { provider: "vast".into(), name: "devtest-b".into(), status: "running".into(), cost_per_hr: Some(0.2), ..hz.clone() };
        ctx.event(&Event::Created { pod: &hz, owner: &OwnerRule::None });
        ctx.event(&Event::Created { pod: &vast, owner: &OwnerRule::None });
        let fake = FakeProvider::default();
        fake.pods.lock().unwrap().push(vast.clone());
        let tracked = Tracked::new(&fake, &ctx, OwnerRule::None);
        tracked.list_pods().await.unwrap();
        tracked.stop_pod("77").await.unwrap();
        tracked.terminate_pod("77").await.unwrap();
        let s = ctx.state();
        let row = |p: &str| s.machines.iter().find(|m| m.provider == p).unwrap().clone();
        assert_eq!(row("vast").ended.map(|e| e.how), Some(spend::EndHow::Terminated));
        assert!(row("vast").segs.iter().any(|g| !g.running), "the stop: {:?}", row("vast").segs);
        assert!(row("hetzner").is_open() && row("hetzner").segs.iter().all(|g| g.running));
    }

    /// Every method of the `Provider` trait is forwarded by [`Tracked`] — and by `create`/`up`'s
    /// `RecordingProvider`: one it doesn't override would answer with the trait's default
    /// (`supports` saying yes to everything, say) instead of the fleet's.
    #[test]
    fn the_tracked_provider_forwards_every_provider_method() {
        let fns = |text: &str| -> Vec<String> {
            text.lines()
                .filter_map(|l| {
                    let l = l.trim_start();
                    let l = l.strip_prefix("async ").unwrap_or(l);
                    l.strip_prefix("fn ").map(|r| r.split(['(', '<']).next().unwrap_or("").to_string())
                })
                .collect()
        };
        let core = include_str!("../../core/src/provider/mod.rs");
        let start = core.find("pub trait Provider").expect("the trait");
        let end = start + core[start..].find("\n}\n").expect("its end");
        let required = fns(&core[start..end]);
        assert!(required.len() > 20 && required.contains(&"supports".to_string()), "the trait read: {required:?}");
        for (file, text, wrapper) in [("spend.rs", include_str!("spend.rs"), "Tracked"), ("pods/create.rs", include_str!("pods/create.rs"), "RecordingProvider")] {
            let s = text.find(&format!("impl Provider for {wrapper}")).expect("the impl");
            let e = s + text[s..].find("\n}\n").expect("its end");
            let forwarded = fns(&text[s..e]);
            let missing: Vec<&String> = required.iter().filter(|f| !forwarded.contains(f)).collect();
            assert!(missing.is_empty(), "{wrapper} ({file}) doesn't forward: {missing:?}");
        }
    }
}
