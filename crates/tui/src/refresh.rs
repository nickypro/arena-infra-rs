//! The background refresh: the listing (per provider), the SSH probes, the slower details round (the
//! provider's details query, balances, OpenRouter, RTT), the state files it reads, and what it publishes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use arena_core::balance::{self, AccountView, BalanceApi, BalancePolicy};
use arena_core::capability::Op;
use arena_core::metrics::{self, PodMetrics, ProbeOpts};
use arena_core::openrouter::{OpenRouter, UsageView};
use arena_core::provider::{ContainerReset, Provider};
use arena_core::proxy::Listing;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::snapshot::{self, HealthCache};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::state::{capture_details, carry_over, dashboard_snapshot, details_due, listing_note, mark_key, merge_details, overlay_details, provider_title, remember_good, summarize, Action, Endpoint, FleetSummary, PodDetails, RefreshStatus, Rtts, Throttle, RTT_WAIT};
use crate::Shared;

/// Shorter than backup/setup's 10s: a down pod shouldn't stall a whole metrics sweep.
const METRICS_CONNECT_TIMEOUT: u32 = 4;

/// How long one details query (`Provider::enrich`) may take before the refresh carries on
/// without it — the HTTP client has no timeout of its own (`pods list` uses the same 20s).
const ENRICH_TIMEOUT: Duration = Duration::from_secs(20);

/// The longest a dashboard provider call waits out a rate limit (a held quota, a 429's
/// `Retry-After`) before giving up for this refresh — its provider's last rows kept — instead:
/// waiting a minute window's reset would freeze the probes and the screen with it.
const PROVIDER_WAIT: Duration = Duration::from_secs(2);

/// When the background refresh runs: every `interval` seconds, or at once on `nudge`;
/// `details_wanted` (the `r` key) asks for the provider's details query too.
pub(crate) struct Cadence {
    pub interval: Arc<AtomicU64>,
    pub nudge: Arc<Notify>,
    pub details_wanted: Arc<AtomicBool>,
}

/// The background refresh loop: one [`refresh`] per cadence tick, waiting for the
/// cadence to elapse *or* a manual nudge (`r`/`f`/action), whichever comes first. `details`
/// starts with what the slower round reads besides the provider: balances, OpenRouter.
pub(crate) async fn fetch_loop(
    provider: Arc<dyn Provider>,
    remote: Arc<dyn Remote>,
    cfg: Config,
    progress_cmd: Option<String>,
    shared: Arc<Mutex<Shared>>,
    cadence: Cadence,
    mut details: DetailsState,
) {
    // The per-pod probe gathers GPU stats + branch + setup health in one SSH call.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let opts = ProbeOpts {
        progress_cmd,
        repo_path: Some(repo_path),
        key_remote: Some(cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string()),
        nvidia_smi_timeout: None,
        repo_timeout: None,
    };
    loop {
        shared.lock().unwrap().refreshing = true;
        let forced = cadence.details_wanted.swap(false, Ordering::Relaxed);
        refresh(provider.as_ref(), &remote, &cfg, &opts, &shared, &mut details, forced).await;

        let secs = cadence.interval.load(Ordering::Relaxed).max(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = cadence.nudge.notified() => {}
        }
    }
}

/// What the refresh remembers between rounds about the provider's details query.
#[derive(Default)]
pub(crate) struct DetailsState {
    /// The last good answer, re-applied to every listing until the next one.
    pub details: HashMap<String, PodDetails>,
    /// When the query last ran (tokio's clock, so tests can step it).
    pub last: Option<tokio::time::Instant>,
    /// Why the last query's answer is incomplete, if it is.
    pub warning: Option<String>,
    /// The prepaid balances, observed on the details cadence (`None`: not shown — tests).
    pub balance: Option<BalanceCtx>,
    /// OpenRouter's keys and credit, read on the details cadence (`None`: no provisioning key).
    pub ai: Option<AiCtx>,
    /// Each provider's last good listing ([`state::remember_good`]): a rate-limited
    /// provider's rows come from it.
    pub listed: Option<Listing>,
    /// When each provider last listed (tokio's clock): how old its carried rows are.
    pub listed_at: HashMap<String, tokio::time::Instant>,
    /// The footer's note on the rate-limited providers, this refresh.
    pub throttle_note: Option<String>,
    /// The RTT timings kept between rounds (`None`: none taken — tests, whose fixture
    /// endpoints must never be connected to).
    pub rtt: Option<Rtts>,
}

/// What the `AI$` column and the summary bar's OpenRouter part read with: the provisioning
/// API (scoped to `OPENROUTER_WORKSPACE`), when a read last started, the view the reads
/// update, and the read in flight. Reads run in the background: a slow listing (a workspace
/// of many pages) never holds a refresh, and what it read shows from the next one.
pub(crate) struct AiCtx {
    pub api: Arc<OpenRouter>,
    pub last: Option<tokio::time::Instant>,
    pub view: Arc<Mutex<UsageView>>,
    pub reading: Option<tokio::task::JoinHandle<()>>,
    /// Woken when a read asked for by a run that changed keys lands ([`Shared::ai_wanted`]): the
    /// refresh then shows it at once.
    pub nudge: Option<Arc<Notify>>,
}

impl AiCtx {
    pub(crate) fn new(api: OpenRouter) -> Self {
        AiCtx { api: Arc::new(api), last: None, view: Arc::default(), reading: None, nudge: None }
    }
}

/// How long the key listing may take (every page, the workspace lookup first) before the
/// last one is kept.
const AI_KEYS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the credit read may take: as long as an inline balance read.
const AI_CREDITS_TIMEOUT: Duration = balance::INLINE_TIMEOUT;

/// Read OpenRouter's keys and credit, each within its own bound, and apply them to `view`
/// apart ([`UsageView::apply`]): one failing never holds back the other's fresh answer. `done`
/// is woken once they're applied.
async fn read_ai(api: Arc<OpenRouter>, view: Arc<Mutex<UsageView>>, done: Option<Arc<Notify>>) {
    async fn bounded<T>(d: Duration, f: impl std::future::Future<Output = arena_core::Result<T>>) -> std::result::Result<T, String> {
        match arena_core::ratelimit::bounded(d, f).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err(format!("timed out after {}s", d.as_secs())),
        }
    }
    let (keys, credits) = tokio::join!(bounded(AI_KEYS_TIMEOUT, api.list_keys()), bounded(AI_CREDITS_TIMEOUT, api.credits()));
    view.lock().unwrap_or_else(std::sync::PoisonError::into_inner).apply(keys, credits);
    if let Some(n) = done {
        n.notify_one();
    }
}

/// What the summary bar's balance reads with: the reads, the state file the CLI keeps too
/// (`None`: live readings only) and the threshold — plus the state as last applied here,
/// so a state file that can't be saved never makes every round read live again.
pub(crate) struct BalanceCtx {
    api: Arc<dyn BalanceApi>,
    path: Option<PathBuf>,
    policy: BalancePolicy,
    /// The state as this dashboard last applied it.
    memory: Option<balance::BalanceState>,
    /// When the balance was last observed (tokio's clock).
    last: Option<tokio::time::Instant>,
}

impl BalanceCtx {
    pub(crate) fn new(api: Arc<dyn BalanceApi>, path: Option<PathBuf>, policy: BalancePolicy) -> Self {
        BalanceCtx { api, path, policy, memory: None, last: None }
    }

    /// The state to observe against: the file's, unless what this dashboard last applied
    /// is newer (the file couldn't be written, or another writer lost a race).
    fn state(&self) -> balance::BalanceState {
        let (file, _) = balance::load_opt(self.path.as_deref());
        let newest = |s: &balance::BalanceState| s.accounts.values().filter_map(|a| a.last_live_attempt()).max();
        match &self.memory {
            Some(m) if newest(m) > newest(&file) => m.clone(),
            _ => file,
        }
    }
}

/// Observe the prepaid balances (inline: ≤ INLINE_TIMEOUT, a live read only when the last
/// attempt is FRESH old), quiet whatever happens to them.
async fn observe_balance(b: &BalanceCtx, now: u64) -> (balance::BalanceState, Vec<balance::Fetched>) {
    let state = b.state();
    let fetched = balance::observe(b.api.as_ref(), &state, balance::Mode::Inline, balance::INLINE_TIMEOUT, now).await;
    (state, fetched)
}

/// Record what was observed against this listing's burns (unknown for a provider that
/// failed to list — every one, when none answered) and judge it.
fn balance_views(
    b: &mut BalanceCtx,
    (state, fetched): (balance::BalanceState, Vec<balance::Fetched>),
    listing: &Listing,
    pods: &[Pod],
    now: u64,
) -> Vec<AccountView> {
    let burns = balance::burns_with(listing, pods, &state, &b.policy);
    let (state, _) = balance::commit(b.path.as_deref(), state, &fetched, &burns, balance::Mode::Inline, now);
    let views = balance::views(&state, &fetched, &burns, &b.policy, balance::Mode::Inline, now);
    b.memory = Some(state);
    b.last = Some(tokio::time::Instant::now());
    views
}

/// One refresh: list pods (per provider, so a provider that failed is known — not just
/// missing), probe them concurrently, build the core snapshot and publish it to `shared`.
///
/// API calls per refresh are at most one list per provider ([`list_all`]: none for a provider
/// whose quota is held — a 429's wait, a spent or low window). The details query
/// (`Provider::enrich`: RunPod's GraphQL for GPU, $/h and the host maintenance window) runs
/// alongside the probes only when `state::details_due` says so (every
/// [`state::DETAILS_EVERY`], or on `r` = `forced`), and its answer is re-applied to the
/// listings in between. The proxy config and the health cache are local file reads.
async fn refresh(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    opts: &ProbeOpts,
    shared: &Mutex<Shared>,
    details: &mut DetailsState,
    forced: bool,
) {
    // The spend ledger records the listing as of when it began (a stop the dashboard made
    // meanwhile is newer, and stands).
    let listed_at = snapshot::unix_now();
    let (fresh, listing) = list_all(provider, details).await;
    let now = snapshot::unix_now();
    // The prepaid balances on the details cadence (every DETAILS_EVERY, or `r` after the
    // minimum gap) — and FRESH keeps live reads to one per account per 5 min however often.
    let balance_due = details.balance.as_ref().is_some_and(|b| details_due(b.last.map(|t| t.elapsed()), forced));
    if !listing.any_ok() {
        // Keep the last known pods on screen; just report the error. The balance is still
        // observed and aged — with every burn unknown, from the provider's rate or the last
        // sample, else `?` — never frozen at its last value.
        let views = match details.balance.as_mut().filter(|_| balance_due) {
            Some(b) => {
                let seen = observe_balance(b, now).await;
                Some(balance_views(b, seen, &fresh, &[], now))
            }
            None => None,
        };
        let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        let held = details.throttle_note.as_ref().map(|n| format!(" · ⚠ {n}")).unwrap_or_default();
        let mut s = shared.lock().unwrap();
        s.status = RefreshStatus::list_error(format!("list error: {}{held}", errs.join("; ")));
        if let Some(v) = views {
            s.balance = v;
        }
        s.refreshing = false;
        return;
    }
    let partial: Vec<String> = listing.errors().iter().map(|(p, _)| p.to_string()).collect();
    let mut pods = listing.pods();
    // The first refresh shows the rows as soon as they're listed: the probes, the details
    // query and the balance reads below can take their full budgets (seconds, up to
    // ENRICH_TIMEOUT), and until now the screen said `loading…` for all of it.
    if shared.lock().unwrap().last_refresh.is_none() {
        publish_listing(cfg, shared, &pods, &partial, now);
    }
    // OpenRouter on the details cadence, in the background (never two reads at once) — and at
    // once after a run that changed keys (`Shared::ai_wanted`, taken only when a read can start:
    // one already going may have begun before the change).
    if let Some(a) = details.ai.as_mut() {
        let idle = a.reading.as_ref().is_none_or(|h| h.is_finished());
        let wanted = idle && std::mem::take(&mut shared.lock().unwrap().ai_wanted);
        if idle && (wanted || details_due(a.last.map(|t| t.elapsed()), forced)) {
            let done = if wanted { a.nudge.clone() } else { None };
            a.reading = Some(tokio::spawn(read_ai(a.api.clone(), a.view.clone(), done)));
            a.last = Some(tokio::time::Instant::now());
        }
    }
    // `SSH_CONFIG_FILE` (and staff's `SSH_ADMIN_CONFIG_FILE`) kept current, as `pods list`
    // keeps them: from a complete listing only (a provider that failed would drop its pods'
    // blocks — and a rate-limited one's carried rows aren't a listing), written only when an
    // endpoint moved; a failure is a status-line warning, never more.
    let ssh_warning = if fresh.errors().is_empty() {
        // No home under test: a `~/` value can't reach the developer's own ~/.ssh.
        let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
        arena_core::sshconfig::sync_all(cfg, &pods, home.as_deref()).into_iter().find_map(Result::err).map(|e| format!("ssh config not rewritten: {e}"))
    } else {
        None
    };
    let observing = async {
        match details.balance.as_ref().filter(|_| balance_due) {
            Some(b) => Some(observe_balance(b, now).await),
            None => None,
        }
    };
    // RTT: the endpoints due a timing ([`RTT_EVERY`]), timed alongside the SSH probes, all at
    // once, the whole timing bounded by [`RTT_WAIT`] — inside the probes' own budget when they
    // run; at worst a refresh with an endpoint that doesn't answer and nothing else to wait on
    // takes up to that second longer, once a minute.
    let rtt_due = details.rtt.as_ref().map(|r| r.due(&pods, Instant::now())).unwrap_or_default();
    let (metrics, observed, timed) = if details_due(details.last.map(|t| t.elapsed()), forced) {
        let mut enriched = pods.clone();
        let (metrics, failed, seen, timed) = tokio::join!(
            fetch_metrics(remote, &pods, cfg, opts),
            enrich_bounded(provider, &mut enriched),
            observing,
            time_endpoints(rtt_due)
        );
        // A query that failed part-way still shows what it filled (as `pods list` does),
        // over the last good details rather than instead of them (`state::merge_details`).
        merge_details(&mut details.details, capture_details(&enriched), failed.is_none());
        details.warning = failed;
        details.last = Some(tokio::time::Instant::now());
        (metrics, seen, timed)
    } else {
        tokio::join!(fetch_metrics(remote, &pods, cfg, opts), observing, time_endpoints(rtt_due))
    };
    if let Some(r) = details.rtt.as_mut() {
        r.record(timed, Instant::now());
    }
    overlay_details(&mut pods, &details.details);
    // What each machine costs, for `arena spend` — quietly, and an unchanged machine isn't
    // rewritten more than once a minute. Only the providers that listed this time count
    // (`fresh`): a rate-limited one's carried rows say nothing new.
    arena_core::spend::record_listing(spend_path(cfg).as_deref(), &fresh, &pods, &Naming::from_config(cfg), listed_at);
    // Who each machine is for, from the ledger just written (the OWNER column).
    let owners = spend_path(cfg).map(|p| owners_by_key(&arena_core::spend::load(&p).0, &pods)).unwrap_or_default();
    // The Claude Code accounts forget the machines this listing shows are gone, then are read for
    // the detail pane.
    arena_core::claude::record_listing(claude_path(cfg).as_deref(), &fresh, listed_at);
    let claude = claude_path(cfg).and_then(|p| arena_core::claude::load(&p).ok());
    // The balance's runway is judged against what is billing as listed (not the pending
    // placeholders, and after the details filled the prices), a provider that failed to
    // list — or was rate-limited, its rows carried — counting as unknown, never as $0/h.
    let balance_views = match (observed, details.balance.as_mut()) {
        (Some(seen), Some(b)) => Some(balance_views(b, seen, &fresh, &pods, now)),
        _ => None,
    };
    let (proxy, proxy_warning) = snapshot::local_proxy_text(cfg);
    let (file_health, health_warning) = load_health(cfg);

    let mut guard = shared.lock().unwrap();
    let s = &mut *guard;
    // Drop optimistic placeholders the provider now reports, then show the
    // real fleet plus any still-pending placeholders.
    s.pending.retain(|pp| !pods.iter().any(|r| r.name == pp.name));
    let mut display = pods;
    display.extend(s.pending.iter().cloned());
    let health = s.deep.layer(file_health);
    let snap = dashboard_snapshot(&display, &partial, proxy.as_deref(), &health, &Naming::from_config(cfg), now);

    for pod in &display {
        let m = metrics.get(&pod.name);
        let pct = |(u, t): (u32, u32)| if t > 0 { (u as u64 * 100 / t as u64) as u32 } else { 0 };
        // A CPU-only box's graphs are its CPU and RAM (there's no GPU to draw).
        let (util, mem_pct) = if pod.cpu_only() {
            (m.and_then(|m| m.cpu_pct), m.and_then(|m| m.host_mem_summary()).map(pct))
        } else {
            (m.and_then(|m| m.mean_util()), m.and_then(|m| m.mem_summary()).map(pct))
        };
        s.history.entry(pod.name.clone()).or_default().push(util, m.and_then(|m| m.max_temp()), mem_pct);
    }
    s.summary = summarize(&display, &metrics);
    if let Some(a) = &details.ai {
        let view = a.view.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
        let names: Vec<String> = display.iter().map(|p| p.name.clone()).collect();
        s.ai_cohort = Some(view.cohort(prefix, &names));
        s.ai = Some(view);
    }
    let warnings: Vec<&str> = [&details.throttle_note, &details.warning, &proxy_warning, &health_warning, &ssh_warning]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let (quota, rate) = quota_line(&listing);
    s.status = RefreshStatus::of(status_line(&s.summary, quota.as_deref(), &warnings), &warnings, rate);
    s.metrics = metrics;
    if let Some(r) = &details.rtt {
        s.rtts = r.clone();
    }
    if let Some(v) = balance_views {
        s.balance = v;
    }
    s.resets = backend_resets(provider, &display);
    s.refusals = backend_refusals(provider, &display);
    s.owners = owners;
    s.claude = claude;
    s.publish(snap);
    s.last_refresh = Some(Instant::now());
    s.refreshing = false;
}

/// This refresh's listing, every provider asked — each one's rate-limit waits bounded by
/// [`PROVIDER_WAIT`], so one whose API quota is held (a 429's wait, a spent window, a low
/// window's spacing: `arena_core::ratelimit`) is refused at once, **without a request**,
/// while the others list as usual. Returned as listed (`fresh`: for what records or acts on
/// it — the balance's burns, the SSH config) and as shown, a rate-limited provider's last
/// good rows kept ([`state::carry_over`]), with the footer's note on it
/// ([`state::listing_note`]).
async fn list_all(provider: &dyn Provider, details: &mut DetailsState) -> (Listing, Listing) {
    let results = arena_core::ratelimit::waits_within(PROVIDER_WAIT, provider.list_by_provider()).await;
    let throttled: Vec<String> = results
        .iter()
        .filter(|(_, r)| matches!(r, Err(e) if e.kind() == Some(arena_core::ProviderErrorKind::RateLimited)))
        .map(|(p, _)| p.clone())
        .collect();
    let fresh = Listing::from_results(results);
    let mut shown = fresh.clone();
    carry_over(details.listed.as_ref(), &mut shown, &throttled);
    let now = tokio::time::Instant::now();
    for pl in fresh.providers.iter().filter(|pl| pl.pods.is_ok()) {
        details.listed_at.insert(pl.provider.clone(), now);
    }
    let notes: Vec<Throttle> = throttled
        .iter()
        .map(|p| Throttle {
            provider: p.clone(),
            hold: arena_core::ratelimit::retry_in(p),
            rows_age: shown
                .providers
                .iter()
                .any(|pl| &pl.provider == p && pl.pods.is_ok())
                .then(|| details.listed_at.get(p).map(|t| now - *t))
                .flatten(),
        })
        .collect();
    details.throttle_note = listing_note(&notes);
    details.listed = Some(remember_good(details.listed.as_ref(), &fresh));
    (fresh, shown)
}

/// The footer's quota part: per provider listed, its API requests in the last minute and
/// what its quota has left this minute ([`arena_core::ratelimit::footer`]) — from this
/// process's own requests; nothing for a provider that sent none and reports nothing. With the
/// requests of the last minute across them (the footer's short status, `ok · API 13/min`).
fn quota_line(listing: &Listing) -> (Option<String>, Option<usize>) {
    let views = arena_core::ratelimit::observed();
    let mut seen: Vec<&str> = Vec::new();
    let listed: Vec<&str> = listing
        .providers
        .iter()
        .map(|pl| pl.provider.as_str())
        .filter(|p| {
            let new = !seen.contains(p);
            seen.push(p);
            new
        })
        .collect();
    let parts: Vec<String> =
        listed.iter().filter_map(|p| arena_core::ratelimit::footer(&views, p, &provider_title(p))).collect();
    let rate: usize = views.iter().filter(|v| listed.contains(&v.provider)).map(|v| v.last_minute).sum();
    ((!parts.is_empty()).then(|| parts.join(" · ")), (!parts.is_empty()).then_some(rate))
}

/// Show `pods` as listed, before anything about them is read: the rows, their proxy and last
/// deep check (local files) and the counts — no metrics yet. The first refresh's early paint.
fn publish_listing(cfg: &Config, shared: &Mutex<Shared>, pods: &[Pod], partial: &[String], now: u64) {
    let (proxy, _) = snapshot::local_proxy_text(cfg);
    let (file_health, _) = load_health(cfg);
    let mut s = shared.lock().unwrap();
    let health = s.deep.layer(file_health);
    let snap = dashboard_snapshot(pods, partial, proxy.as_deref(), &health, &Naming::from_config(cfg), now);
    s.summary = summarize(pods, &HashMap::new());
    s.status = RefreshStatus::listed(&count_label(pods.len()));
    s.publish(snap);
}

/// `1 pod`, `3 pods`.
fn count_label(n: usize) -> String {
    format!("{n} pod{}", if n == 1 { "" } else { "s" })
}

/// What a restart does to each of `pods`' container disks, by [`mark_key`]: each one's backend's
/// answer (`Provider::restart_resets_container` — pure, no request), for the menus' restart label.
pub(crate) fn backend_resets(provider: &dyn Provider, pods: &[Pod]) -> HashMap<String, ContainerReset> {
    pods.iter().map(|p| (mark_key(p), provider.restart_resets_container(p))).collect()
}

/// What each of `pods`' providers can't do of the menus' operations ([`Action::op`]), by
/// [`mark_key`]: the capability table's refusal (`Provider::supports` — pure, no request), for the
/// menus to grey those rows out with why.
pub(crate) fn backend_refusals(provider: &dyn Provider, pods: &[Pod]) -> HashMap<String, Vec<(Op, String)>> {
    let ops: Vec<Op> = Action::MENU.iter().filter_map(|(_, a)| a.op()).collect();
    pods.iter()
        .map(|p| (mark_key(p), ops.iter().filter_map(|&op| provider.supports(op, Some(p)).refusal().map(|why| (op, why))).collect()))
        .collect()
}

/// The provider's best-effort details query, bounded by [`ENRICH_TIMEOUT`]. `Some(why)`
/// when the details are incomplete (the pods keep whatever was filled before a failure).
async fn enrich_bounded(provider: &dyn Provider, pods: &mut [Pod]) -> Option<String> {
    // Its rate-limit waits as short as the listing's: a held quota is a warning, not a stall.
    match tokio::time::timeout(ENRICH_TIMEOUT, arena_core::ratelimit::waits_within(PROVIDER_WAIT, provider.enrich(pods))).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("pod details incomplete: {e}")),
        Err(_) => Some(format!("pod details timed out after {}s", ENRICH_TIMEOUT.as_secs())),
    }
}

/// Where the dashboard reads — and its deep checks write — the health cache: the CLI's
/// path ([`snapshot::health_cache_path_for`]), so `pods test --deep`'s verdicts show here
/// and the dashboard's show in `arena snapshot`. `None` in this crate's unit tests unless
/// the test's config sets `ARENA_STATE_DIR`: a test must never touch the developer's real
/// `~/.local/state`.
pub(crate) fn health_cache_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(snapshot::health_cache_path_for(cfg))
}

/// Where the balance state lives — the CLI's file ([`snapshot::BALANCE_FILE`] beside the
/// health cache), under the same test guard as [`health_cache_path`].
pub(crate) fn balance_state_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(snapshot::state_file_path_for(cfg, snapshot::BALANCE_FILE))
}

/// The health cache as it stands (an empty one when there is none or it can't be read),
/// plus the warning to show when it was unusable. Never fails: health is a convenience.
fn load_health(cfg: &Config) -> (HealthCache, Option<String>) {
    match health_cache_path(cfg) {
        None => (HealthCache::new(), None),
        Some(Err(e)) => (HealthCache::new(), Some(format!("no health cache: {e}"))),
        Some(Ok(path)) => HealthCache::load(&path),
    }
}

/// Time each endpoint's RTT ([`arena_core::rtt::connect_once`]: exactly one TCP connect, name
/// lookup included within [`RTT_WAIT`]), all at once. `None` for one that didn't answer in time.
async fn time_endpoints(due: Vec<Endpoint>) -> Vec<(Endpoint, Option<Duration>)> {
    let mut set = JoinSet::new();
    for ep in due {
        set.spawn(async move {
            let d = arena_core::rtt::connect_once((ep.0.as_str(), ep.1), RTT_WAIT).await;
            (ep, d)
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        out.extend(joined.ok());
    }
    out
}

/// Fan metric fetches out across the fleet concurrently over `remote` — each probe bounded
/// by core's `PROBE_TIMEOUT` and a short connect timeout, so a single unreachable pod
/// can't hold up the sweep.
async fn fetch_metrics(
    remote: &Arc<dyn Remote>,
    pods: &[Pod],
    cfg: &Config,
    opts: &ProbeOpts,
) -> HashMap<String, PodMetrics> {
    let mut set = JoinSet::new();
    for pod in pods {
        if let Ok(mut target) = SshTarget::from_pod(pod, cfg) {
            target.connect_timeout_secs = METRICS_CONNECT_TIMEOUT;
            let name = pod.name.clone();
            let opts = opts.clone();
            let remote = remote.clone();
            set.spawn(async move { (name, metrics::fetch_with(remote.as_ref(), &target, &opts).await) });
        }
    }
    let mut fresh = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((name, m)) = joined {
            fresh.insert(name, m);
        }
    }
    fresh
}

/// The footer's refresh status: liveness (the pod count lives in the summary bar), the
/// providers' API rate and quota ([`quota_line`]), then any warning from this refresh's reads
/// (a held listing, details query, proxy file, health cache).
fn status_line(s: &FleetSummary, quota: Option<&str>, warnings: &[&str]) -> String {
    let mut out = format!("{} reporting", s.reporting);
    if s.unreachable > 0 {
        out.push_str(&format!(" · {} unreachable", s.unreachable));
    }
    if let Some(q) = quota {
        out.push_str(&format!(" · {q}"));
    }
    for w in warnings {
        out.push_str(&format!(" · ⚠ {}", w.strip_prefix("warning: ").unwrap_or(w)));
    }
    out
}

/// The fleet's spend ledger (`arena spend`): its state file — none under test unless the
/// test's config sets `ARENA_STATE_DIR` (a test never touches the developer's own).
pub(crate) fn spend_path(cfg: &Config) -> Option<std::path::PathBuf> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    snapshot::state_file_path_for(cfg, arena_core::spend::SPEND_FILE).ok()
}

/// Each owned pod's owner by [`mark_key`] (per instance: provider + id, never the name a reused
/// name shares with someone else's instance). Pure.
pub(crate) fn owners_by_key(state: &arena_core::spend::SpendState, pods: &[Pod]) -> HashMap<String, String> {
    pods.iter().filter_map(|p| arena_core::spend::owner_of(state, &p.provider, &p.id).map(|o| (mark_key(p), o.to_string()))).collect()
}

/// The fleet's Claude Code accounts (`arena claude`): its state file — none under test unless the
/// test's config sets `ARENA_STATE_DIR`, as [`spend_path`].
pub(crate) fn claude_path(cfg: &Config) -> Option<std::path::PathBuf> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    arena_core::claude::state_path(cfg).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use arena_core::health::Status;
    use arena_core::remote::{FakeRemote, RemoteCall};
    use arena_core::{fleet, PodSpec};
    use crate::state::{self, health_cell, summary_text};
    use crate::tests::{cfg, live_views, pod, proxy_file, tmp, verdict, Down, FakeFleet};

    /// The first refresh's early paint: the listed rows, no metrics yet, and a status that
    /// says the rest is being read — never "loading…" while the probes run.
    #[test]
    fn the_listing_is_shown_before_the_pods_are_read() {
        let shared = Mutex::new(Shared { status: RefreshStatus::busy("loading…"), ..Default::default() });
        let pods = [pod("alpha", "runpod", "a", 1, Some(22)), pod("bravo", "hetzner", "b", 2, None)];
        publish_listing(&cfg(""), &shared, &pods, &[], 1_000);
        let s = shared.lock().unwrap();
        assert_eq!(s.pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha", "devtest-bravo"]);
        assert_eq!((s.snap.pods.len(), s.summary.pods, s.metrics.len()), (2, 2, 0));
        assert_eq!((s.status.full.as_str(), s.status.short.as_str()), ("listed 2 pods — reading them…", "reading 2 pods…"));
        assert!(s.last_refresh.is_none(), "still the first refresh: its full publish comes next");
    }

    /// A refresh times each pod's SSH endpoint alongside the probes — one TCP connect — and
    /// the next refreshes leave it alone until RTT_EVERY has passed (OpenSSH counts each such
    /// connect against the proxy box). A pod without an endpoint isn't timed.
    #[tokio::test]
    async fn refresh_times_each_endpoint_once_alongside_the_probes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut alpha = pod("alpha", "runpod", "rp1", 1, Some(port));
        alpha.ssh_ip = Some("127.0.0.1".into());
        let provider = FakeFleet {
            pods: vec![alpha.clone(), pod("bravo", "runpod", "rp2", 2, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState { rtt: Some(Rtts::default()), ..Default::default() };
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let got = shared.lock().unwrap().rtts.of(&alpha);
        assert!(got.is_some_and(|d| d < RTT_WAIT), "{got:?}");
        // Exactly one connect so far (it waits in the backlog), none from the next refresh.
        async fn waiting(l: &tokio::net::TcpListener) -> bool {
            tokio::time::timeout(Duration::from_millis(200), l.accept()).await.is_ok()
        }
        assert!(waiting(&listener).await, "timed once");
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        assert!(!waiting(&listener).await, "not again within RTT_EVERY");
        assert_eq!(shared.lock().unwrap().rtts.of(&alpha), got, "kept between rounds");
        // The dashboard's tests take no timings unless asked: no fixture endpoint is ever dialled.
        let mut quiet = DetailsState::default();
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &Mutex::new(Shared::default()), &mut quiet, false).await;
        assert!(!waiting(&listener).await);
    }

    /// The balance is observed in the details round (every DETAILS_EVERY, or `r` after the
    /// minimum gap) — never on each refresh — and a live read happens at most once per FRESH
    /// however often that is: the state file remembers the last attempt.
    #[tokio::test(start_paused = true)]
    async fn balance_is_observed_on_the_details_cadence() {
        use balance::{Account, FakeBalanceApi, Live};
        let dir = tmp("balance-cadence");
        let cfg = cfg("");
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let api = Arc::new(FakeBalanceApi::new().live_ok(
            Account::Runpod,
            Live { usd: 100.0, provider_burn_per_hr: None, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None },
        ));
        let ctx = BalanceCtx::new(api.clone(), Some(dir.0.join("devtest").join(snapshot::BALANCE_FILE)), BalancePolicy::default());
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState { balance: Some(ctx), ..Default::default() };
        let opts = ProbeOpts::default();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(api.live_calls(), 1, "the first round reads it");
        {
            let s = shared.lock().unwrap();
            assert_eq!((s.balance.len(), s.balance[0].balance), (1, Some(100.0)));
            // Judged against the same listing, after the details filled alpha's $0.17.
            assert_eq!(s.balance[0].listed_per_hr, Some(0.17));
        }
        // An ordinary refresh: no balance work at all.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(api.live_calls(), 1);
        // The next details round (and `r` after the gap) observe again — but the reading is
        // younger than FRESH, so no live call: still shown, from the state file.
        tokio::time::advance(state::DETAILS_EVERY).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        tokio::time::advance(state::DETAILS_MIN_GAP).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(api.live_calls(), 1, "at most one live read per FRESH");
        assert_eq!(shared.lock().unwrap().balance[0].balance, Some(100.0));
    }

    /// The dashboard keeps `SSH_CONFIG_FILE` current, as `pods list` does (spec
    /// ssh-config-include): a complete listing writes it, the same listing again leaves it alone,
    /// a listing a provider failed never rewrites it (its pods would vanish), and a file that
    /// isn't arena's stays as it is, with a warning on the status line.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_keeps_the_ssh_config_file_current() {
        type Listings = Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>;
        struct Scripted(Mutex<Listings>);
        #[async_trait::async_trait]
        impl Provider for Scripted {
            fn name(&self) -> &'static str {
                "fleet"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                panic!("the dashboard lists per provider")
            }
            async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
                self.0.lock().unwrap().iter().map(|(p, r)| (p.to_string(), r.clone().map_err(arena_core::Error::provider))).collect()
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                panic!("a refresh must never create a pod")
            }
            async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
                panic!("a refresh must never stop a pod")
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                panic!("a refresh must never restart a pod")
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                panic!("a refresh must never terminate a pod")
            }
        }
        use arena_core::sshconfig::{managed_text, render_manual};
        let dir = tmp("sshconfig");
        let path = dir.0.join("arena.conf");
        let cfg = cfg(&format!("SSH_CONFIG_FILE={}\n", path.display()));
        let want = |p: &Pod| managed_text(&render_manual("devtest", "root", "/nonexistent/devtest_key", std::slice::from_ref(p)));
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let provider = Scripted(Mutex::new(vec![("runpod", Ok(vec![alpha.clone()])), ("vast", Ok(vec![]))]));
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let read = || std::fs::read_to_string(&path).unwrap();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&alpha));
        // alpha moved while vast fails to list: not rewritten.
        let moved = pod("alpha", "runpod", "rp1", 2, Some(22002));
        *provider.0.lock().unwrap() = vec![("runpod", Ok(vec![moved.clone()])), ("vast", Err("vast list HTTP 429"))];
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&alpha));
        // Complete again: rewritten.
        *provider.0.lock().unwrap() = vec![("runpod", Ok(vec![moved.clone()])), ("vast", Ok(vec![]))];
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&moved));
        // Someone's own file: untouched, said on the status line.
        std::fs::write(&path, "Host mine\n").unwrap();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), "Host mine\n");
        let status = shared.lock().unwrap().status.full.clone();
        assert!(status.contains("ssh config not rewritten") && status.contains("isn't arena's"), "{status}");
    }

    fn rp_live(usd: f64, rate: f64) -> balance::Live {
        balance::Live { usd, provider_burn_per_hr: Some(rate), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    /// BS8: with every provider failing to list, the balance is still observed on its
    /// cadence and shown afresh — not the last views frozen on screen.
    #[tokio::test(start_paused = true)]
    async fn balance_is_observed_even_when_every_listing_fails() {
        use balance::{Account, FakeBalanceApi};
        let dir = tmp("balance-down");
        let api = Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)));
        let ctx = BalanceCtx::new(api.clone(), Some(dir.0.join("devtest").join(snapshot::BALANCE_FILE)), BalancePolicy::default());
        let mut details = DetailsState { balance: Some(ctx), ..Default::default() };
        let shared = Mutex::new(Shared { balance: live_views(&[(Account::Runpod, 100.0, 0.5)]), ..Default::default() });
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        refresh(&Down, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let s = shared.lock().unwrap();
        assert!(s.status.full.starts_with("list error"), "{}", s.status.full);
        assert_eq!(s.status.short, "list error");
        assert_eq!((s.balance.len(), s.balance[0].balance), (1, Some(40.0)), "observed afresh, not the old $100");
        // Burns unknown (nothing listed): the provider's own rate is the burn.
        assert_eq!(s.balance[0].burn.map(|b| b.per_hr), Some(0.5));
        assert_eq!(api.live_calls(), 1);
    }

    /// BS6 for the dashboard: with no state file at all, what it applied last keeps live
    /// reads to one per FRESH — and the reading stays on screen between them.
    #[tokio::test(start_paused = true)]
    async fn without_a_state_file_live_is_still_read_once_per_fresh() {
        use balance::{Account, FakeBalanceApi};
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let api = Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)));
        let mut details = DetailsState { balance: Some(BalanceCtx::new(api.clone(), None, BalancePolicy::default())), ..Default::default() };
        let shared = Mutex::new(Shared::default());
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        for _ in 0..3 {
            refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
            tokio::time::advance(state::DETAILS_EVERY).await;
        }
        assert_eq!(api.live_calls(), 1, "one live read in three rounds");
        assert_eq!(shared.lock().unwrap().balance[0].balance, Some(40.0));
    }

    /// One refresh = the core snapshot over the per-provider listing, the local proxy file
    /// and the health cache (a failed provider named, not silently missing); one list call
    /// per refresh as before, the details query only on its slower cadence or on `r` (and
    /// its answer kept on the rows in between); over SSH only the metrics probe.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_is_the_core_snapshot_with_details_on_a_slower_cadence() {
        let dir = tmp("refresh");
        let proxy_path = dir.0.join("proxy.conf");
        std::fs::write(&proxy_path, proxy_file()).unwrap();
        let cfg = cfg(&format!(
            "SSH_PROXY_HOST=proxy.example.com\nSSH_PROXY_NGINX_CONFIG_PATH={}\nARENA_STATE_DIR={}\n",
            proxy_path.display(),
            dir.0.join("state").display()
        ));
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        let cache = health_cache_path(&cfg).unwrap().unwrap();
        assert!(cache.starts_with(&dir.0), "never the real state dir: {}", cache.display());
        snapshot::record_health(&cache, &[verdict(&alpha, Status::Pass, None)], None, snapshot::unix_now() - 600).unwrap();

        let provider = FakeFleet {
            pods: vec![bravo, alpha],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let calls = || (provider.lists.load(Ordering::SeqCst), provider.details.load(Ordering::SeqCst));
        let alpha_row = |shared: &Mutex<Shared>| {
            let s = shared.lock().unwrap();
            let a = s.snap.pods.iter().find(|p| p.pod.id == "rp1").unwrap().clone();
            (fleet::price_label(&a.pod), state::has_maintenance(&a.pod))
        };

        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        {
            let s = shared.lock().unwrap();
            let names: Vec<&str> = s.pods.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, ["devtest-alpha", "devtest-bravo"]);
            assert_eq!(s.snap.pods.len(), 2, "rows and snapshot stay in step");
            let (a, b) = (&s.snap.pods[0], &s.snap.pods[1]);
            assert_eq!((a.proxy, a.proxy_port), (snapshot::ProxyState::Live, Some(9500)));
            assert_eq!((b.proxy, b.proxy_port), (snapshot::ProxyState::Stale, Some(9501)));
            assert_eq!(health_cell(a.health.as_ref(), false, s.snap.generated_at).0, "✓10m");
            assert!(b.health.is_none());
            assert_eq!(s.snap.partial, ["vast"]);
            let summary = summary_text(&s.summary, &s.snap.cost, &s.snap.partial, &[], None);
            assert!(summary.contains("fleet: $0.17/h across 2 billing pod(s) (1 unpriced)"), "{summary}");
            assert!(summary.starts_with(" ⚠ vast failed to list — its pods (and their cost) are missing"), "{summary}");
        }
        assert_eq!(alpha_row(&shared), ("$0.17".to_string(), true));
        assert_eq!(calls(), (1, 1), "the first refresh fills the details");
        // Over SSH: one metrics probe per pod, nothing else.
        let probes: Vec<String> = fake.calls().iter().map(|c| c.host().to_string()).collect();
        assert_eq!(probes.len(), 2);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout: Some(t), .. } if *t == arena_core::remote::PROBE_TIMEOUT)));

        // Right away again: one more list, no details query — the details stay on the row.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (2, 1));
        assert_eq!(alpha_row(&shared), ("$0.17".to_string(), true));
        // `r` within the minimum gap: still none; once it has passed: one.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(calls(), (3, 1));
        tokio::time::advance(state::DETAILS_MIN_GAP).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(calls(), (4, 2));
        // Unprompted, once a DETAILS_EVERY has gone by.
        tokio::time::advance(state::DETAILS_EVERY).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (5, 3));
    }

    /// RunPod's listing rate-limited on the given list calls (1-based); every other call
    /// lists `pods`. Counts list and details calls; panics on anything that would change
    /// the fleet.
    struct Throttled {
        pods: Vec<Pod>,
        on: &'static [usize],
        lists: AtomicUsize,
        details: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for Throttled {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            let n = self.lists.fetch_add(1, Ordering::SeqCst) + 1;
            if self.on.contains(&n) {
                return Err(arena_core::Error::Provider {
                    kind: arena_core::ProviderErrorKind::RateLimited,
                    message: "list pods HTTP 429 Too Many Requests".into(),
                });
            }
            Ok(self.pods.clone())
        }
        async fn enrich(&self, _pods: &mut [Pod]) -> arena_core::Result<()> {
            self.details.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
            panic!("a refresh must never create a pod")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("a refresh must never stop a pod")
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("a refresh must never restart a pod")
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("a refresh must never terminate a pod")
        }
    }

    /// A rate-limited listing: the rows stay (as last listed, not "failed to list"), the
    /// probes keep running, and the footer says when RunPod is listed again (its API's hold,
    /// as `arena_core::ratelimit` keeps it), why, and how old the rows are. A listing that
    /// works clears the note; the details query keeps its own cadence.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_listing_keeps_the_rows_and_says_when_it_lists_again() {
        let cfg = cfg("");
        let provider = Throttled {
            pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("bravo", "runpod", "rp2", 2, Some(22002))],
            on: &[2, 3],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
        };
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let calls = || (provider.lists.load(Ordering::SeqCst), provider.details.load(Ordering::SeqCst), fake.calls().len());
        let rows = |shared: &Mutex<Shared>| {
            let s = shared.lock().unwrap();
            (s.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), s.snap.partial.clone(), s.status.full.clone())
        };
        let step = |secs: u64| tokio::time::advance(Duration::from_secs(secs));

        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (1, 1, 2));
        // Throttled, RunPod's quota held for 12 s: the rows as last listed, nothing called
        // partial, the wait and the rows' age in the footer; the probes ran.
        step(5).await;
        arena_core::ratelimit::hold_for_test("runpod", "RunPod REST v2", Duration::from_secs(12), "the minute window is spent");
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        let (names, partial, status) = rows(&shared);
        assert_eq!(names, ["devtest-alpha", "devtest-bravo"]);
        assert!(partial.is_empty(), "{partial:?}");
        assert!(
            status.contains("⚠ RunPod rate limit: listing again in 12 s (the minute window is spent) · its rows are 5 s old"),
            "{status}"
        );
        assert_eq!(calls(), (2, 1, 4));
        assert!(details.listed_at.contains_key("runpod"));
        // Still throttled 5 s on: the same rows, older.
        step(5).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert!(rows(&shared).2.contains("its rows are 10 s old"), "{}", rows(&shared).2);
        assert_eq!(rows(&shared).0, ["devtest-alpha", "devtest-bravo"]);
        // Then a listing that works: no note.
        step(5).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls().0, 4);
        assert!(!rows(&shared).2.contains("rate limit"), "{}", rows(&shared).2);
        assert_eq!(details.throttle_note, None);
    }

    /// No rows to keep yet (the very first listing rate-limited): the error, with the note.
    #[tokio::test(start_paused = true)]
    async fn a_first_listing_rate_limited_says_so() {
        let provider = Throttled { pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22001))], on: &[1], lists: AtomicUsize::new(0), details: AtomicUsize::new(0) };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let status = shared.lock().unwrap().status.full.clone();
        assert!(status.starts_with("list error: runpod: provider error: list pods HTTP 429"), "{status}");
        assert!(status.contains("⚠ RunPod rate limit: listing again") && status.contains("no rows yet"), "{status}");
    }

    /// A details query that errs after filling some pods (GraphQL data + `errors`, one
    /// backend failing) still shows what it filled — as `pods list` and `arena snapshot`
    /// do, working on the pods in place — and the footer says the details are incomplete.
    #[tokio::test(start_paused = true)]
    async fn details_filled_before_a_query_error_still_show() {
        let cfg = cfg("");
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None), pod("bravo", "runpod", "rp2", 2, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: Some("pod details: some field errored"),
        };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        refresh(&provider, &remote, &cfg, &ProbeOpts::default(), &shared, &mut details, false).await;
        let s = shared.lock().unwrap();
        let alpha = &s.snap.pods.iter().find(|p| p.pod.id == "rp1").unwrap().pod;
        assert_eq!((fleet::price_label(alpha), state::has_maintenance(alpha)), ("$0.17".to_string(), true));
        assert_eq!(alpha.gpu_type.as_deref(), Some("RTX A4000"));
        assert!(s.status.full.contains("⚠ pod details incomplete: ") && s.status.full.contains("some field errored"), "{}", s.status.full);
        assert_eq!(s.status.short, "warn: pod details incomplete");
        // The fleet total counts the price it got.
        assert!(summary_text(&s.summary, &s.snap.cost, &s.snap.partial, &[], None).contains("$0.17/h"));
    }
}
