//! `pods up` after the create: one independent pipeline per pod (PLAN 2.B).
//!
//! The batch flow this replaces waited for *every* endpoint, then set the whole fleet up,
//! so one slow pod held up everyone — the ops playbook's "bring a whole fleet up as
//! independent pipelines, never as a batch". Here each pod advances the moment it is
//! individually ready, and says so:
//!
//!   endpoint → proxy sync → setup → [--check: deep check] → API keys → [--lock] → `[name] READY …`
//!
//! or `[name] FAILED <stage>: …` (left running). A pod up without a usable SSH endpoint for
//! `--endpoint-timeout` (RunPod: its container running behind RunPod's SSH proxy only) is a
//! host failure, with or without `--check` ([`EndpointWait::NoEndpoint`]): once a fresh look
//! confirms it, where it was is said, and it is terminated and its name recreated within the
//! name's placements (`--check-attempts`), never left billing. So is a RunPod pod still listed
//! RUNNING with no container when `--timeout` ends ([`NO_CONTAINER_LIMIT`]): its image never
//! started, a failed placement. With `--check` a pod that FAILs the deep
//! check on its host (`health::FailureKind::Host`: driver, CUDA, GPUs, peer copies, NCCL) is
//! terminated — and confirmed gone, so a name never has two pods — then its name is
//! recreated from the confirmed options and the new pod runs the pipeline again; one that
//! lands on a machine that already failed is rejected the same way (and terminated even with
//! no attempt left to replace it: it was never set up, and its machine is known bad). A host
//! FAIL with definite machine-level evidence (`health::condemns_machine`), on a machine its
//! provider names, is also remembered in the health cache for `snapshot::BAD_HOST_TTL_SECS`
//! (or until a check on it passes), so a later run (or another name's first pod) turns that
//! machine away before setup — Vast's creates skip it — unless `--retry-bad-hosts`; without
//! `--check` it is only warned about. Only a
//! check whose script actually ran can condemn a host: one that couldn't run (SSH dropped,
//! timed out) is retried once, then FAILs the name with the pod left running. A FAIL that is
//! only software/setup (python reporting that torch is missing or doesn't import, a CPU-only
//! torch, no zsh to run the check in on a bare VM before setup) is never replaced — the new
//! pod would boot the same image and fail the same way — so
//! the name FAILs at once, pod left running, its machine not counted as bad; python dying
//! before it could report (timeout, OOM, crash) is a host failure. Ctrl+C stops every
//! pipeline where it is and launches nothing new; nothing is terminated because of it.
//!
//! Concurrency is cooperative, on this one task: the pipelines borrow the fleet provider,
//! so they can't be spawned, and [`join_unordered`] drives them side by side instead —
//! next to one shared endpoint poller (one list call per `--interval` for the whole fleet,
//! as before) and the Ctrl+C watcher. Proxy writes go through one async mutex, so two
//! pipelines never write the proxy config at once. The decisions and the final report are
//! pure, in `arena_core::pipeline`.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use arena_core::fleet;
use arena_core::health::{self, HealthPolicy, PodHealth, Status, DEEP_CHECK_TIMEOUT};
use arena_core::pipeline::{self, Attempt, AttemptEnd, Stage, UpRow, Verdict};
use arena_core::placement::{self, End, OptionPlan, PlacementOption, PriceSource, Progress, Rounds};
use arena_core::provider::{bounded_list, LIST_TIMEOUT};
use arena_core::proxy::Listing;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::setup::{
    key_rejected, looks_unreachable, provision, provision_after_key_repair, provisioning_steps, BootRetry, SetupConfig,
    SetupTimeouts,
};
use arena_core::provider::runpod_v2;
use arena_core::snapshot::{self, BadHost};
use arena_core::ssh::SshTarget;
use arena_core::status;
use arena_core::{Config, Pod, PodSpec, Provider};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{
    copy_keys_command, enrich_best_effort, fleet_ssh_for, judge_deep_call, proxy_sync_now, record_health, repair_pod_keys,
    sync_line, KeySources, Made, ProxySync, COPY_KEYS_TIMEOUT, ENRICH_TIMEOUT,
};

/// How long a pod may be up with no direct SSH endpoint — reachable only through RunPod's
/// SSH proxy ([`Pod::proxy_only`]) — before it's a host failure: `up`'s `--endpoint-timeout`
/// default ([`pipeline::DEFAULT_ENDPOINT_TIMEOUT_MINS`]), and what the endpoint waits of
/// `replace`/`migrate`/`start`/`reimage` give up on such a pod after. Live (2026-10-08) such
/// community hosts never got a public endpoint (15+ minutes); a healthy pod has one within
/// seconds of its container starting.
pub(crate) const NO_PUBLIC_IP_GRACE: Duration = Duration::from_secs(pipeline::DEFAULT_ENDPOINT_TIMEOUT_MINS * 60);

/// The shortest `--timeout` at whose end a RunPod pod still listed RUNNING with no container
/// ([`no_container`]) is a failed placement — terminated and its name recreated, not left billing.
/// RunPod lists a pod RUNNING from its create while its image is pulled, and can't tell a pull
/// that will never finish from a slow one; live, healthy pulls took 1½ to 6⅔ minutes
/// (2026-10-28..30), and prod's arena9-lightning had none after the whole 600 s (2026-10-29). As
/// `--timeout`'s default, so a shorter `--timeout` leaves such a pod running, as before.
const NO_CONTAINER_LIMIT: Duration = Duration::from_secs(600);

/// What waiting for a pod's SSH endpoint came to.
enum EndpointWait {
    /// Listed with an endpoint.
    Ready(Pod),
    /// Up without a usable endpoint ([`status::up_without_endpoint`] — on RunPod, its container
    /// running behind RunPod's SSH proxy only) for `--endpoint-timeout` (past the endpoint
    /// deadline if need be: a whole one, always — unless that deadline is a failed placement,
    /// Shadeform's; its boot grace not counted, `Shared::grace_for`): a host failure once a fresh
    /// look confirms it
    /// (`Shared::no_endpoint`; the pod as last listed so) — by the latest listing that showed it
    /// so, or an older one (the latest lacked it, or couldn't look its endpoint up).
    NoEndpoint { pod: Pod, up_for: Duration },
    /// The deadline passed with no endpoint and the pod not up without one: still coming up
    /// (provisioning, loading; on RunPod, RUNNING with no container yet — an image still being
    /// pulled, or one that never will be: RunPod can't tell them apart, so past
    /// [`NO_CONTAINER_LIMIT`] it is a failed placement). `last`: as the latest listing that said
    /// anything about it had it.
    TimedOut { last: Option<Pod> },
    /// It died before it came up — listed `ERROR` on a backend whose creates can fail after
    /// they succeeded ([`Provider::ready_deadline`]: Shadeform): a failed placement. `why`, for
    /// the lines and the attempt log (`error: <status details>`).
    Failed { pod: Pod, why: String },
}

/// How long a pod we terminated (to replace it) may stay listed before we give up on the
/// replacement: creating the new pod while the old one is still listed would put two pods
/// under one name (the proxy merge, `still_needed` and every name lookup assume one).
const GONE_TIMEOUT: Duration = Duration::from_secs(300);

/// What waiting for a terminated pod to leave its provider's listing came to.
#[derive(Debug, PartialEq, Eq)]
enum Gone {
    /// Gone (or listed as terminated): the name may be created again.
    Yes,
    /// Still listed after [`GONE_TIMEOUT`].
    StillListed,
    /// Another live pod (this id) holds the name now: a replacement would be a second one.
    Taken(String),
}

/// How long the provider's pod details (the host maintenance window a deep check reports)
/// are reused across checks: RunPod's are one GraphQL query over the whole account, so a
/// fleet checked pod by pod shouldn't make one per pod.
const DETAILS_TTL: Duration = Duration::from_secs(60);

/// Which stream a line belongs on: progress to stdout, trouble to stderr (as everywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum To {
    Out,
    Err,
}

/// Where the run's lines go: the terminal ([`console`]), or a recorder in tests — which is
/// how the tests see *when* each pod reported, on the paused clock.
pub(crate) type Say<'a> = &'a (dyn Fn(To, &str) + Sync);

pub(crate) fn console(to: To, line: &str) {
    match to {
        To::Out => println!("{line}"),
        To::Err => eprintln!("{line}"),
    }
}

/// Provisioning, resolved before anything was created.
pub(crate) struct SetupStage {
    pub scfg: SetupConfig,
    pub timeouts: SetupTimeouts,
    /// The staged hetzner bare-VM script (`""` when no hetzner pod is involved).
    pub hetzner_script: String,
}

/// `--check`, resolved before anything was created.
pub(crate) struct CheckStage {
    pub policy: HealthPolicy,
    /// `--check-attempts`: placements per name, the first create included.
    pub attempts: u32,
    /// The deep-check command (the script inside the conda login wrap).
    pub cmd: String,
}

/// Everything the pipelines run with — all decided (and confirmed) before the create.
pub(crate) struct UpRun<'a> {
    pub provider: &'a dyn Provider,
    pub remote: Arc<dyn Remote>,
    pub cfg: &'a Config,
    /// The confirmed placement options (a one-option plan on the single-spec path) and the
    /// spec they're layered on: a replacement draws from exactly what was agreed to.
    pub options: OptionPlan,
    pub base: PodSpec,
    /// `--retry-mins`/`--retry-secs`, for a replacement's placement too.
    pub rounds: Rounds,
    /// Per attempt: how long to wait for the pod to come up — its SSH endpoint, then sshd
    /// answering on it (the boot race).
    pub timeout: Duration,
    /// Between endpoint polls (and "is it gone yet?" checks).
    pub interval: Duration,
    /// Whether there's a proxy to sync to — probed once, as the final sync would.
    pub proxy: std::result::Result<(), String>,
    pub setup: Option<SetupStage>,
    pub check: Option<CheckStage>,
    /// The per-host keys to hand out — only when setup runs and a keys CSV has rows.
    pub keys: Option<KeySources>,
    /// Each pod's own Claude Code account (one by spread if it has none) and its token, after the
    /// keys — when setup runs and there is a pool (`arena_core::claude::stage`).
    pub claude: bool,
    /// `--lock`: lock each pod as its last step (checked lockable before the create).
    pub lock: bool,
    /// `--retry-bad-hosts`: ignore the machines earlier runs remember as bad (a machine that
    /// fails again is remembered again; one that passes is forgotten).
    pub retry_bad_hosts: bool,
    /// Where a replacement's price is remembered for the balance, the moment its create
    /// returns — as the create phase does for the first pods (`OnMade`) — so a later listing
    /// with no price (RunPod v1's REST list) still prices it. `None`: not remembered.
    pub balance: Option<&'a crate::money::BalanceCtx>,
    /// `--expire-*`/`--idle-after` given: write each pod's reap notice once it's set up (RR11).
    pub reap_notice: bool,
    /// `--check-attempts`: placements per name, the first create included — what also bounds
    /// replacing a pod that never came up (listed `ERROR`, or not up by its backend's ready
    /// deadline) or never got a usable SSH endpoint, with or without `--check`: that is a failed
    /// placement, not a host to inspect.
    pub placements: u32,
    /// `--endpoint-timeout` (`ENDPOINT_TIMEOUT_MINS`): how long a pod may be up without a usable
    /// SSH endpoint ([`status::up_without_endpoint`]) before it's replaced.
    pub endpoint_timeout: Duration,
}

/// The per-pod part of `up`'s plan, for the dry-run and the confirm prompt. `endpoint`: the
/// `--endpoint-timeout` minutes and the placements per name that bound replacing a pod that
/// never gets an endpoint.
pub(crate) fn pipeline_text(setup: bool, check: Option<u32>, keys: bool, claude: bool, lock: bool, endpoint: (u64, u32)) -> String {
    let (mins, placements) = endpoint;
    let mut steps = vec![
        format!(
            "wait for its SSH endpoint — a pod up without one for {mins} min (RunPod: behind its SSH proxy \
             only), or a RunPod pod with no container at --timeout ({} min or more), is TERMINATED and its name \
             recreated (up to {placements} placement(s) per name)",
            NO_CONTAINER_LIMIT.as_secs() / 60
        ),
        "sync the proxy (if nginx is set up)".to_string(),
    ];
    if setup {
        steps.push("provision it".into());
    } else {
        steps.push("wait for SSH to answer".into());
    }
    if let Some(n) = check {
        steps.push(format!(
            "deep-check it — a pod that FAILs on its host is TERMINATED and its name recreated (up to {n} \
             placement(s) per name; options that haven't failed first; never on a machine that already \
             failed — this run, or an `up --check` in the last {}h); a software/setup FAIL (e.g. torch \
             missing) is left running, not replaced",
            snapshot::BAD_HOST_TTL_SECS / 3600
        ));
    }
    if setup && keys {
        steps.push("copy its API keys".into());
    }
    if setup && claude {
        steps.push("put it on a Claude Code account (the least-loaded, if it has none) and push that token".into());
    }
    if lock {
        steps.push("lock it (RunPod then refuses stop/restart/terminate until `arena pods unlock`)".into());
    }
    format!("per pod, as soon as it can: {} — then READY/FAILED per pod", steps.join(", "))
}

/// What one name's pipeline came to (it becomes an [`UpRow`]).
struct NameRun {
    name: String,
    /// When its first pod was created: READY AFTER counts from here.
    first_at: Instant,
    attempts: Vec<Attempt>,
    /// The name's pod now; `None` once we terminated it with no replacement made.
    pod: Option<Pod>,
    /// The option the current pod was created on (index into the confirmed plan).
    option: usize,
    /// The current pod's deep check, if it had one.
    health: Option<PodHealth>,
    /// The proxy port its pipeline's sync routed to it.
    port: Option<u16>,
    /// The fleet-SSH write its keys stage made (authorized keys + `~/.ssh/config` host map),
    /// so the report can tell whether a map rendered from a since-grown listing is stale.
    fleet_ssh: Option<String>,
    verdict: Verdict,
    ready_at: Option<Instant>,
    /// `--lock` locked its pod.
    locked: bool,
}

/// Counts a pipeline as waiting on the endpoint poller for as long as it's alive.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(waiters: &'a AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::SeqCst);
        Self(waiters)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the pipelines share.
struct Shared<'r, 'a> {
    run: &'r UpRun<'a>,
    say: Say<'r>,
    /// Flips to `true` on Ctrl+C.
    stop: watch::Receiver<bool>,
    /// The poller's latest fleet listing.
    listing: watch::Sender<Arc<Vec<Pod>>>,
    /// Pipelines currently waiting for an endpoint — the poller lists only while there are some.
    waiters: AtomicUsize,
    /// A pipeline started waiting: an idle poller lists at once instead of at its next tick
    /// (pipelines now start while the create phase runs, not all at the poller's first round).
    waiter_added: tokio::sync::Notify,
    /// One proxy writer at a time.
    proxy_gate: tokio::sync::Mutex<()>,
    /// Machines ([`health::host_key`]) that failed the deep check this run (any name's: a bad
    /// host breaks every pod on it).
    failed_hosts: Mutex<Vec<String>>,
    /// Machines remembered as bad ([`health::machine_key`] → when and why): an earlier `up
    /// --check`'s host failures still within their TTL (read once, at the start), plus this
    /// run's. Unlike `failed_hosts` it turns a pod away on ANY attempt, the first included.
    known_bad: Mutex<BTreeMap<String, BadHost>>,
    /// The provider's pod details for the deep checks, and when they were fetched.
    details: tokio::sync::Mutex<Option<(Instant, Vec<Pod>)>>,
}

/// Bring every pod in `made` up through its own pipeline, concurrently; returns one row per
/// name, in `made`'s order, after a final proxy sync. `interrupt` resolving (Ctrl+C in the
/// CLI) stops every pipeline where it is: no new attempt, no terminate, just the report.
#[cfg(test)]
pub(crate) async fn run_up<I>(run: &UpRun<'_>, made: Vec<Made>, interrupt: I, say: Say<'_>) -> Vec<UpRow>
where
    I: Future<Output = ()>,
{
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for m in made {
        let _ = tx.send(m);
    }
    drop(tx);
    run_up_streaming(run, rx, interrupt, say).await
}

/// [`run_up`] for pods that arrive while it runs: each pipeline starts the moment its pod
/// comes through `made` (the create phase sends each one as its create returns, and closes
/// the channel when it's done — finding #14: placed pods used to wait, billing, for a whole
/// `--retry-mins` window). Rows are in arrival order; the report waits for the channel to
/// close and every pipeline to end. Nothing arrived at all: no rows, and no report (nothing
/// was brought up, so there's no proxy sync to make).
pub(crate) async fn run_up_streaming<I>(
    run: &UpRun<'_>,
    mut made: tokio::sync::mpsc::UnboundedReceiver<Made>,
    interrupt: I,
    say: Say<'_>,
) -> Vec<UpRow>
where
    I: Future<Output = ()>,
{
    let (stop_tx, stop) = watch::channel(false);
    let (listing, _) = watch::channel(Arc::new(Vec::new()));
    let (known_bad, warning) = if run.retry_bad_hosts {
        (BTreeMap::new(), None)
    } else {
        remembered_bad_hosts(run.cfg, snapshot::unix_now())
    };
    if let Some(w) = warning {
        say(To::Err, &format!("{w} — no machine is remembered as bad this run"));
    }
    let shared = Shared {
        run,
        say,
        stop,
        listing,
        waiters: AtomicUsize::new(0),
        waiter_added: tokio::sync::Notify::new(),
        proxy_gate: tokio::sync::Mutex::new(()),
        failed_hosts: Mutex::new(Vec::new()),
        known_bad: Mutex::new(known_bad),
        details: tokio::sync::Mutex::new(None),
    };
    let shared = &shared;
    let pipelines = join_streamed(&mut made, |i, m| async move { (i, shared.pipeline(m).await) });
    let watcher = async {
        interrupt.await;
        say(To::Err, "interrupted — starting nothing new and terminating nothing; reporting what's ready");
        let _ = stop_tx.send(true);
        std::future::pending::<()>().await
    };
    // Polled in this order on every wake. The watcher first: in a terminal, Ctrl+C's SIGINT
    // also kills the in-flight ssh children, so a pipeline can see a check "fail" in the very
    // wake the interrupt arrives — the stop must already be up by then, or that pod would be
    // terminated for a replacement. Then the pipelines, so they're registered as waiting
    // before the poller's first round (else its first list would wait a whole interval).
    let mut names = tokio::select! {
        biased;
        () = watcher => unreachable!("the interrupt watcher never returns"),
        names = pipelines => names,
        () = shared.poll_endpoints() => unreachable!("the endpoint poller never returns"),
    };
    if names.is_empty() {
        return Vec::new();
    }
    names.sort_by_key(|(i, _)| *i);
    shared.report(names.into_iter().map(|(_, n)| n).collect()).await
}

/// [`join_unordered`] over futures that arrive on `rx`: `start(i, item)` makes the `i`-th
/// item's future the moment it arrives, and it is driven from then on beside the others.
/// Ends once `rx` is closed and every future has finished; outputs in finishing order.
async fn join_streamed<T, F, S>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>, mut start: S) -> Vec<F::Output>
where
    S: FnMut(usize, T) -> F,
    F: Future,
{
    let mut slots: Vec<Option<Pin<Box<F>>>> = Vec::new();
    let mut out = Vec::new();
    let mut closed = false;
    std::future::poll_fn(|cx| {
        // Take in everything that has arrived (registering for the next one), then poll
        // every unfinished future — a new one included, so its own wake-ups are set up.
        while !closed {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(item)) => {
                    let i = slots.len();
                    slots.push(Some(Box::pin(start(i, item))));
                }
                Poll::Ready(None) => closed = true,
                Poll::Pending => break,
            }
        }
        for slot in slots.iter_mut() {
            if let Some(f) = slot {
                if let Poll::Ready(v) = f.as_mut().poll(cx) {
                    out.push(v);
                    *slot = None;
                }
            }
        }
        if closed && slots.iter().all(Option::is_none) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    out
}

/// A row for each confirmed name the create phase made no pod for
/// ([`pipeline::not_created`]): `FAILED create`, or `STOPPED create` after a Ctrl+C — so the
/// summary covers every name the operator asked for, and [`conclude`] fails for it.
pub(crate) fn not_created_rows(confirmed: &[String], made: &[String], interrupted: bool) -> Vec<UpRow> {
    pipeline::not_created(confirmed, made)
        .into_iter()
        .map(|name| UpRow {
            name,
            gpu: "-".into(),
            price: "-".into(),
            proxy_port: None,
            health: None,
            health_notes: String::new(),
            verdict: if interrupted {
                Verdict::Stopped { stage: Stage::Create }
            } else {
                Verdict::Failed { stage: Stage::Create, reason: "no pod was created for it (the create output above says why)".into() }
            },
            ready_after: None,
            attempts: Vec::new(),
        })
        .collect()
}

/// The summary table, then `Err` (non-zero exit) unless every pod is READY.
pub(crate) fn conclude(rows: &[UpRow], say: Say<'_>) -> anyhow::Result<()> {
    say(To::Out, &format!("\n{}", pipeline::render_up_summary(rows).trim_end()));
    let not_ready: Vec<String> =
        rows.iter().filter(|r| !r.verdict.is_ready()).map(|r| format!("{} ({})", r.name, r.verdict.status_label())).collect();
    if !not_ready.is_empty() {
        anyhow::bail!("{} of {} pod(s) not ready: {}", not_ready.len(), rows.len(), not_ready.join(", "));
    }
    Ok(())
}

/// Drive `futs` side by side on this task and return their outputs in finishing order — for
/// futures that borrow (the pipelines share `&dyn Provider`), which a `JoinSet` can't spawn.
/// Each wake polls every unfinished future: fine for a fleet's worth of them.
async fn join_unordered<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut slots: Vec<Option<Pin<Box<F>>>> = futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut out = Vec::with_capacity(slots.len());
    std::future::poll_fn(|cx| {
        for slot in slots.iter_mut() {
            if let Some(f) = slot {
                if let Poll::Ready(v) = f.as_mut().poll(cx) {
                    out.push(v);
                    *slot = None;
                }
            }
        }
        if slots.iter().all(Option::is_none) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    out
}

/// `$0.17` / `~$0.17` (preset estimate) per pod for an option, `-` if unpriced.
fn option_price(o: &PlacementOption) -> String {
    match o.price_per_pod {
        Some(p) => {
            let tilde = if o.price_source == Some(PriceSource::Estimate) { "~" } else { "" };
            format!("{tilde}{}", fleet::fmt_money("$", p))
        }
        None => "-".to_string(),
    }
}

/// Why a replacement's placement didn't fill the name: the run's end, or — when a retry
/// round dropped the name (it appeared on the fleet meanwhile) — that.
fn unplaced(placed: &placement::Placement) -> String {
    if let Some(why) = placed.log.first().and_then(|l| l.skipped.as_deref()) {
        return format!("skipped: {why}");
    }
    match &placed.end {
        End::Filled => "nothing was created".into(),
        End::Exhausted => "no capacity on any option".into(),
        End::WindowElapsed => "the retry window elapsed".into(),
        End::Interrupted => "interrupted".into(),
        End::Aborted { error, .. } | End::Failed { error, .. } => error.to_string(),
    }
}

impl Shared<'_, '_> {
    fn option(&self, k: usize) -> Option<&PlacementOption> {
        self.run.options.options.get(k)
    }

    /// `fut`, unless Ctrl+C comes first (`None`) — or at the same time: a step cut short by
    /// the interrupt (its ssh child got the SIGINT too) is "stopped", never judged. Dropping
    /// an in-flight SSH call stops its local ssh child (see `remote`); every step it could cut
    /// short is idempotent.
    async fn or_stop<T>(&self, fut: impl Future<Output = T>) -> Option<T> {
        let mut stop = self.stop.clone();
        tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => None,
            v = fut => Some(v),
        }
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// One name, start to finish: its first pod's pipeline, then a replacement's, until one
    /// ends it (READY, FAILED, STOPPED).
    async fn pipeline(&self, made: Made) -> NameRun {
        let mut run = NameRun {
            name: made.pod.name.clone(),
            first_at: made.at,
            attempts: Vec::new(),
            pod: None,
            option: 0,
            health: None,
            port: None,
            fleet_ssh: None,
            verdict: Verdict::Stopped { stage: Stage::Endpoint },
            ready_at: None,
            locked: false,
        };
        // Options whose pods failed a check for this name — a replacement tries them last.
        let mut failed_options = Vec::new();
        let mut next = Some((made.pod, made.option.unwrap_or(0)));
        while let Some((pod, option)) = next.take() {
            next = self.attempt(&mut run, pod, option, &mut failed_options).await;
        }
        run
    }

    /// One pod's pipeline. `Some` = it was replaced: run this pod (and option) next.
    async fn attempt(
        &self,
        run: &mut NameRun,
        pod: Pod,
        option: usize,
        failed_options: &mut Vec<usize>,
    ) -> Option<(Pod, usize)> {
        let say = self.say;
        let name = run.name.clone();
        let n = run.attempts.len() + 1;
        run.attempts.push(Attempt {
            option: self.option(option).map(PlacementOption::describe).unwrap_or_else(|| "-".into()),
            pod_id: pod.id.clone(),
            host: None,
            end: AttemptEnd::Kept,
            terminated: false,
        });
        run.option = option;
        run.health = None;
        run.port = None;
        run.fleet_ssh = None;
        run.pod = Some(pod.clone());

        // 1. Its SSH endpoint, within its own deadline — or, on a backend whose creates can fail
        // after they succeeded (Shadeform: an instance goes to `error`, or sits pending, maybe
        // billing), within that backend's ready deadline: past it, or once listed ERROR, the
        // pod is a failed placement — terminated and the name tried on the next option, never
        // waited out or left billing.
        //
        // The wait ends at whichever bound comes first — the backend's ready deadline or
        // `--timeout` — and on such a backend (or any whose pods can't be stopped: they bill
        // until deleted) every expiry is a failed placement, whichever bound it was. Review SF1:
        // keeping the ready deadline only when it was under `--timeout` dropped it with the
        // shipped defaults (15 min vs 600 s), and a pod still pending at 600 s was "left
        // running" — billing.
        let started = Instant::now();
        let ready = self.run.provider.ready_deadline(&pod);
        let mut deadline = started + ready.map_or(self.run.timeout, |r| r.min(self.run.timeout));
        let fail_on_expiry = ready.is_some() || self.run.provider.stop_support(&pod).is_err();
        // Why such a backend's pod is a failed placement at its deadline.
        let expired = || {
            let bound = match ready {
                Some(r) if r <= self.run.timeout => "its provider's ready deadline",
                _ => "--timeout",
            };
            format!("not up after {} ({bound})", pipeline::fmt_elapsed(started.elapsed()))
        };
        // Whether the deadline was moved once already, for a pod first seen up without an
        // endpoint by the deadline's own look (below).
        let mut regraced = false;
        // A pod judged to have no endpoint is looked at once more first (`no_endpoint`); one that
        // turns out not to be up after all (restarting) or can't be read is waited for again,
        // its clock started over by the listings after that look, to the same deadline.
        // What a fresh look saw of a pod it didn't condemn, for the wait it resumes.
        let mut seen: Option<Pod> = None;
        let pod = loop {
            match self.or_stop(self.wait_endpoint(&pod, deadline, fail_on_expiry, seen.take())).await {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Endpoint }),
                Some(EndpointWait::Failed { pod, why }) => return self.dead(run, &pod, option, failed_options, why).await,
                Some(EndpointWait::TimedOut { .. }) if fail_on_expiry => {
                    // A last look at its own backend first (review of the nits): an endpoint that
                    // came between the last listing and the deadline goes on; one deleted
                    // elsewhere is said, nothing left; anything else is a failed placement, as
                    // this deadline always was — deleted, never left billing.
                    let Some(listings) = self.or_stop(self.run.provider.list_by_provider()).await else {
                        return self.end(run, Verdict::Stopped { stage: Stage::Endpoint });
                    };
                    match now_listed(&listings, &pod) {
                        Now::Reachable(found) => break found,
                        Now::Gone => {
                            run.pod = None;
                            let reason = format!(
                                "{}, and {} no longer lists it (terminated elsewhere?) — nothing left running",
                                expired(),
                                pod.provider
                            );
                            return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                        }
                        _ => return self.dead(run, &pod, option, failed_options, expired()).await,
                    }
                }
                Some(EndpointWait::TimedOut { last }) => {
                    // The wait never trusts one missing listing (a backend that failed to list
                    // drops its pods from the aggregate), but the verdict must say how things
                    // stand now: ask the owning backend once — a pod terminated elsewhere
                    // meanwhile is gone, not "left running" (live G: one deleted out of band read
                    // so for 10 minutes); one whose endpoint came with the deadline goes on.
                    //
                    // A RunPod pod still listed RUNNING with no container [`NO_CONTAINER_LIMIT`]
                    // after its create is a failed placement, with or without --check: its image
                    // never started (prod 2026-10-29, ops feedback #1: arena9-lightning, `FAILED
                    // endpoint: no SSH endpoint after 600s — left running`, billing until it was
                    // terminated by hand; its re-run was READY in a minute). Healthy pulls took
                    // 1½–7 minutes live. Judged on `--timeout` itself, not the time this look took:
                    // a short one's slow last look never turns into a terminate (review r6).
                    // Anything else still coming up — Vast `loading`, Hetzner
                    // `initializing`, a RunPod pod under a `--timeout` shorter than that limit, one
                    // that can't be read just now — is left running as before, and said to bill.
                    //
                    // One this look is the first to see up without an endpoint (it came up between
                    // the last listing and the deadline) gets its whole grace too, as every such
                    // pod does — once: the deadline moves that far, and the wait resumes (review
                    // of #39, nit C). Seen so again then, it is judged like any other.
                    let secs = if regraced { started.elapsed().as_secs() } else { self.run.timeout.as_secs() };
                    let now = self.or_stop(self.run.provider.list_by_provider()).await.map(|l| now_listed(&l, &pod));
                    match now {
                        Some(Now::Gone) => {
                            run.pod = None;
                            let reason = format!(
                                "no SSH endpoint after {secs}s, and {} no longer lists it (terminated elsewhere?) — \
                                 nothing left running",
                                pod.provider
                            );
                            return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                        }
                        Some(Now::Reachable(found)) => break found,
                        Some(Now::NoEndpoint(p)) if !regraced => {
                            regraced = true;
                            let grace = self.grace_for(&p);
                            deadline = Instant::now() + grace;
                            say(
                                To::Err,
                                &format!(
                                    "[{name}] no SSH endpoint after {secs}s, and only now up without one: giving it its {} \
                                     (--endpoint-timeout)",
                                    pipeline::fmt_elapsed(grace)
                                ),
                            );
                            seen = Some(p);
                        }
                        Some(Now::NotUp(p)) if no_container(&p) && self.run.timeout >= NO_CONTAINER_LIMIT => {
                            let why = format!(
                                "still listed RUNNING with no container {} after its create (its image never started)",
                                pipeline::fmt_elapsed(started.elapsed())
                            );
                            return self.dead(run, &p, option, failed_options, why).await;
                        }
                        now => {
                            let last = match now {
                                Some(Now::NotUp(p) | Now::NoEndpoint(p)) => Some(p),
                                _ => last,
                            };
                            let reason = format!("no SSH endpoint after {secs}s{}", left_running(&name, last.as_ref()));
                            return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                        }
                    }
                }
                Some(EndpointWait::NoEndpoint { pod: bare, up_for }) => {
                    match self.no_endpoint(run, bare, up_for, deadline, option, failed_options).await {
                        // It has its endpoint after all (a fresh look found it): on with the pipeline.
                        Ok(Look::Found(found)) => break found,
                        // Not up after all, or not readable: wait on, from what that look saw.
                        Ok(Look::WaitOn(was)) => seen = Some(was),
                        // So, past the deadline: where that is a failed placement (Shadeform: it
                        // bills until deleted), as at the deadline — terminated, never left
                        // billing (review of #39, nit A); elsewhere left running, said to bill.
                        Ok(Look::Expired { pod: was, .. }) if fail_on_expiry => {
                            return self.dead(run, &was, option, failed_options, expired()).await;
                        }
                        Ok(Look::Expired { pod: was, reason }) => {
                            run.pod = Some(was);
                            return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                        }
                        Err(next) => return next,
                    }
                }
                Some(EndpointWait::Ready(found)) => break found,
            }
        };
        // The machine it landed on, by the provider's machine id where the listing lacks one
        // (RunPod v2 reports none; several RunPod hosts share a public IP, so the IP alone
        // could reject a healthy replacement — finding #23).
        let pod = self.with_machine(&name, pod).await;
        let host = health::host_key(&pod);
        if let Some(a) = run.attempts.last_mut() {
            a.host = host.clone();
        }
        run.pod = Some(pod.clone());
        say(
            To::Out,
            &format!(
                "[{name}] endpoint {} (after {})",
                fleet::endpoint_label(&pod),
                pipeline::fmt_elapsed(started.elapsed())
            ),
        );

        // A replacement on a machine that already failed the check: "same machine, the
        // rebuild fixed nothing" — reject it before spending setup and a check on it. Only a
        // real machine identity counts (`host_key`: never a Vast IP, which several machines
        // can share).
        if n > 1 && pipeline::on_failed_host(host.as_deref(), &self.failed_hosts.lock().unwrap()) {
            let ip = host.unwrap_or_default();
            if let Some(a) = run.attempts.last_mut() {
                a.end = AttemptEnd::SameHost;
            }
            failed_options.push(option);
            say(To::Err, &format!("[{name}] landed on {ip}, a machine that already failed the deep check — rejecting it"));
            let why = format!("attempt {n} landed on {ip}, a machine that already failed the deep check");
            return self.replace(run, &pod, failed_options, why, Stage::Check).await;
        }

        // A machine remembered as bad — an `up --check` failed on it, earlier (within its TTL)
        // or in this run: on ANY attempt (live BPF S2: one broken host got 11 pods across runs,
        // each set up and checked before it was turned away). Under --check, rejected unseen
        // like the replacement above; without it nothing here terminates a pod nobody asked to
        // have checked — it is only said.
        let known = health::machine_key(&pod).and_then(|k| self.known_bad.lock().unwrap().get(&k).map(|b| (k, b.clone())));
        if let Some((machine, bad)) = known {
            let when = bad.describe(snapshot::unix_now());
            if self.run.check.is_none() {
                say(
                    To::Err,
                    &format!(
                        "[{name}] ⚠ landed on {machine}, which failed the deep check {when} — kept: without --check \
                         nothing is checked or replaced (`arena pods test --deep {name}`)"
                    ),
                );
            } else {
                if let Some(a) = run.attempts.last_mut() {
                    a.end = AttemptEnd::KnownBad(when.clone());
                }
                failed_options.push(option);
                say(
                    To::Err,
                    &format!(
                        "[{name}] landed on {machine}, which failed the deep check {when} — rejecting it \
                         (`--retry-bad-hosts` tries such machines anyway)"
                    ),
                );
                let why = format!("attempt {n} landed on {machine}, which failed the deep check {when}");
                return self.replace(run, &pod, failed_options, why, Stage::Check).await;
            }
        }

        // 2. The proxy (one writer at a time; a failed sync is reported, never fatal).
        if self.run.proxy.is_ok() {
            self.sync_proxy_for(run).await;
        }

        // sshd answers some time after the endpoint appears (the image boots first; the
        // playbook: "a problem only after ~10 minutes"), and this pod's setup starts the moment
        // its endpoint does — so connection refusals are ridden out for as long as the pod may
        // take to come up at all, not the short window `pods setup` uses on a running fleet.
        let boot = BootRetry { window: self.run.timeout, every: BootRetry::default().every };
        let target = match SshTarget::from_pod(&pod, self.run.cfg) {
            Ok(t) => t,
            Err(e) => return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason: format!("{e} — left running") }),
        };

        // 3. Setup — this pod only, through the same runner as `pods setup`.
        if let Some(setup) = &self.run.setup {
            let steps = provisioning_steps(&pod.provider, &setup.scfg, &name, false, &setup.hetzner_script, &setup.timeouts);
            let t = Instant::now();
            let mut outcome = self.or_stop(provision(self.run.remote.as_ref(), &target, &steps, boot)).await;
            // The pod refused our key: its provider may re-attach it (Vast's per-instance
            // attach); then setup runs again, waiting (bounded) for the attach to land.
            if outcome.as_ref().is_some_and(key_rejected) {
                match self.or_stop(repair_pod_keys(self.run.provider, self.run.cfg, &pod)).await {
                    None => return self.end(run, Verdict::Stopped { stage: Stage::Setup }),
                    Some(Ok(n)) => {
                        say(
                            To::Err,
                            &format!(
                                "[{name}] setup: the pod refused our SSH key — re-attached {n} key(s) through the {} API; \
                                 retrying setup",
                                pod.provider
                            ),
                        );
                        let remote = self.run.remote.as_ref();
                        outcome = self.or_stop(provision_after_key_repair(remote, &target, &steps, boot)).await;
                    }
                    Some(Err(Some(why))) => say(To::Err, &format!("[{name}] setup: the pod refused our SSH key; {why}")),
                    Some(Err(None)) => {}
                }
            }
            match outcome {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Setup }),
                Some(outcome) if !outcome.is_done() => {
                    let reason = format!("{} — left running", outcome.describe());
                    return self.end(run, Verdict::Failed { stage: Stage::Setup, reason });
                }
                Some(outcome) => {
                    // A bare VM's setup says what built its env (`install_uv.sh`, or `none (env
                    // already present)`): on the ✓ line, the one the operator reads.
                    let installer = outcome.installer().map(|i| format!(", installer: {i}")).unwrap_or_default();
                    // A VM from an arena image: which one, and how old (`HETZNER_IMAGE=arena`).
                    let image = outcome.image().map(|i| format!(", image: {i}")).unwrap_or_default();
                    say(To::Out, &format!("[{name}] setup ✓ ({}{installer}{image})", pipeline::fmt_elapsed(t.elapsed())));
                    // A best-effort step (the move onto the volume, the VS Code warm-up) that didn't
                    // work out: said, not fatal.
                    for w in outcome.warnings() {
                        say(To::Err, &format!("[{name}] setup warning: {w}"));
                    }
                }
            }
        }

        // Without setup nothing has waited out the boot race yet: a pod whose sshd is still
        // starting must not be judged by the check (unreachable = FAIL) and replaced for it —
        // nor called READY (live R: a Hetzner `up --no-setup` said READY after 1 s, while the
        // VM was still initializing). Bounded like setup's wait for it.
        if self.run.setup.is_none() {
            match self.or_stop(self.ssh_answers(&target, boot)).await {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Endpoint }),
                Some(Err(why)) => {
                    let reason = format!("SSH never answered ({why}) — left running");
                    return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                }
                Some(Ok(())) => {}
            }
        }

        // The participant's notice of the reap policy this pod was created with (RR11): after
        // setup, whose rc files the banner goes into. Best effort — said, never fatal.
        if self.run.reap_notice {
            match crate::reap::reassert_notice(self.run.remote.as_ref(), self.run.cfg, &pod, &target).await {
                Ok(true) => say(To::Out, &format!("[{name}] reap notice ✓")),
                Ok(false) => {}
                Err(why) => say(To::Err, &format!("[{name}] reap notice not written ({why}) — `arena pods reap set {name} …` writes it")),
            }
        }

        // 4. The deep check: a FAIL is a bad host — replace it while attempts remain.
        if let Some(check) = &self.run.check {
            let Some(mut h) = self.or_stop(self.deep_check(&name, &pod, &target, check)).await else {
                return self.end(run, Verdict::Stopped { stage: Stage::Check });
            };
            // Only a check whose script ran to the end can condemn the host (terminate it,
            // count its IP against every name). One that couldn't run — the SSH session
            // dropped, or the call timed out, moments after setup reached this very pod —
            // says nothing about the machine ("confirm before alarming"): once SSH answers
            // again it's run once more, and if that can't run either the name FAILs with the
            // pod left running for a look. A script that never started although SSH worked (no
            // zsh on a bare VM before setup, no `base64`) is no dropped session: the pod's
            // software, the same on a retry — judged below as software/setup.
            if !script_ran(&h) && !never_started(&h) {
                say(To::Err, &format!("[{name}] check: couldn't run it ({}) — once more when SSH answers", h.notes()));
                match self.or_stop(self.ssh_answers(&target, BootRetry::default())).await {
                    None => return self.end(run, Verdict::Stopped { stage: Stage::Check }),
                    Some(Err(why)) => {
                        let reason = format!(
                            "couldn't run the deep check ({}), then SSH didn't answer ({why}) — left running; its \
                             host isn't counted as bad",
                            h.notes()
                        );
                        run.health = Some(h);
                        return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
                    }
                    Some(Ok(())) => {}
                }
                let Some(again) = self.or_stop(self.deep_check(&name, &pod, &target, check)).await else {
                    return self.end(run, Verdict::Stopped { stage: Stage::Check });
                };
                if !script_ran(&again) {
                    let reason = format!(
                        "couldn't run the deep check twice ({}) — left running (`arena pods test --deep {name}`); \
                         its host isn't counted as bad",
                        again.notes()
                    );
                    run.health = Some(again);
                    return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
                }
                h = again;
            }
            let (status, notes, kind) = (h.status, h.notes(), h.failure_kind());
            run.health = Some(h);
            match status {
                // Every failure is about the software (torch missing, the check script not
                // running): a new pod boots the same image and runs the same setup, so it would
                // fail the same way — never replaced, and its machine isn't counted as bad.
                Status::Fail if kind == Some(health::FailureKind::Software) => {
                    say(To::Err, &format!("[{name}] check: FAIL (software/setup, not the host) — {notes}"));
                    let reason = format!(
                        "software/setup failure, not the host — {notes}. Not replaced: a new pod from the same image \
                         fails the same way. Left running — fix the setup/image, then `arena pods test --deep {name}`"
                    );
                    return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
                }
                Status::Fail => {
                    if let Some(ip) = &host {
                        self.failed_hosts.lock().unwrap().push(ip.clone());
                    }
                    if let Some(h) = &run.health {
                        self.remember_bad_host(&name, &pod, h);
                    }
                    failed_options.push(option);
                    if let Some(a) = run.attempts.last_mut() {
                        a.end = AttemptEnd::CheckFailed(notes.clone());
                    }
                    say(To::Err, &format!("[{name}] check: FAIL (host) — {notes}"));
                    return self.replace(run, &pod, failed_options, format!("deep check failed (host): {notes}"), Stage::Check).await;
                }
                Status::Warn => say(To::Out, &format!("[{name}] check: warn — {notes}")),
                _ => say(To::Out, &format!("[{name}] check: pass")),
            }
        }

        // 5. Its API keys — only ever to a pod that got this far.
        if let Some(keys) = &self.run.keys {
            match self.or_stop(self.copy_keys(&name, &pod, &target, keys)).await {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Keys }),
                Some(Err(why)) => {
                    let reason = format!("{why} — left running (retry: `arena pods copy-keys {name}`)");
                    return self.end(run, Verdict::Failed { stage: Stage::Keys, reason });
                }
                Some(Ok((to, line, fleet_ssh))) => {
                    say(to, &line);
                    run.fleet_ssh = fleet_ssh;
                }
            }
        }

        // 5b. Its Claude Code account: one by spread if it has none, and that account's token.
        // Best effort: a pod without it is still READY — said, with the retry.
        if self.run.claude {
            let one = [(pod.clone(), target.clone())];
            let staged = arena_core::claude::stage(self.run.remote.as_ref(), self.run.cfg, &one, None, snapshot::unix_now());
            match self.or_stop(staged).await {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Keys }),
                Some(Ok(None)) => {}
                Some(Ok(Some(outcomes))) => {
                    for o in outcomes {
                        match &o.result {
                            Ok(()) => say(To::Out, &format!("[{name}] claude ✓ {}", o.what())),
                            Err(e) => say(To::Err, &format!("[{name}] claude ✗ {} — {e} (retry: `arena claude push {name}`)", o.what())),
                        }
                    }
                }
                Some(Err(e)) => say(To::Err, &format!("[{name}] claude: not pushed — {e} (retry: `arena claude push {name}`)")),
            }
        }

        // 6. --lock: last, once nothing in this run will want to stop or replace the pod —
        // a lock refuses terminate, which a failed check's replacement needs. Idempotent, so a
        // throttled or transient failure is retried; a lock that doesn't take fails the name
        // (the operator asked for a protected pod), the pod left running, unlocked.
        if self.run.lock {
            let policy = arena_core::retry::RetryPolicy::default();
            match self.or_stop(arena_core::retry::retrying(&policy, || self.run.provider.set_locked(&pod.id, true))).await {
                None => return self.end(run, Verdict::Stopped { stage: Stage::Lock }),
                Some(Err(e)) => {
                    let reason = format!("{e} — left running, NOT locked (retry: `arena pods lock {name}`)");
                    return self.end(run, Verdict::Failed { stage: Stage::Lock, reason });
                }
                Some(Ok(())) => run.locked = true,
            }
        }

        run.ready_at = Some(Instant::now());
        self.end(run, Verdict::Ready)
    }

    /// A pod that never came up ([`EndpointWait::Failed`], past its backend's ready deadline, or
    /// on RunPod with no container at `--timeout`): a failed placement, not a host to inspect —
    /// nothing is on it yet, and it may bill. It is terminated and the name recreated, options
    /// that haven't failed first, within
    /// the name's placements (`--check-attempts`, with or without `--check`); with none left
    /// it is still terminated, and the name fails at `endpoint`.
    async fn dead(&self, run: &mut NameRun, pod: &Pod, option: usize, failed_options: &mut Vec<usize>, why: String) -> Option<(Pod, usize)> {
        (self.say)(To::Err, &format!("[{}] {why} — before it came up: a failed placement", run.name));
        if let Some(a) = run.attempts.last_mut() {
            a.end = AttemptEnd::Dead(why.clone());
        }
        failed_options.push(option);
        run.pod = Some(pod.clone());
        self.replace(run, pod, failed_options, why, Stage::Endpoint).await
    }

    /// A pod up without a usable SSH endpoint for `--endpoint-timeout` ([`EndpointWait::NoEndpoint`];
    /// ops feedback 2026-10-29 #1: a RUNNING 4090 with no public IP or ports for the whole wait,
    /// `FAILED endpoint … left running` — billing until someone terminated it and ran `up` again,
    /// READY in a minute). A host failure, like a failed deep check: nothing can be set up,
    /// backed up or forwarded through RunPod's proxy, and nothing is on it yet. Where it was —
    /// the machine and data centre its provider names — is said, so a host that keeps doing this
    /// can be told apart; then it is terminated and its name recreated on the confirmed options
    /// (others first), with or without `--check`, within the name's placements — and terminated
    /// even with none left: kept, it would only bill.
    ///
    /// Only on a fresh look ([`now_listed`]): the wait's verdict may rest on an older listing.
    /// Gone meanwhile (terminated elsewhere) is only said. [`Look::Found`]: it has an endpoint
    /// after all — on with the pipeline. [`Look::WaitOn`]: it isn't up any more (a restart:
    /// RunPod lists RUNNING with no container while it pulls its image again), or it can't be
    /// read just now — wait on, its clock started over by the listings after this look, to the
    /// same `deadline`. [`Look::Expired`]: such a pod past the deadline — the caller fails it as
    /// at the deadline (a failed placement where the deadline is one, else left running and said
    /// to bill). Nothing else is terminated on anything but a look that shows it up without an
    /// endpoint.
    async fn no_endpoint(
        &self,
        run: &mut NameRun,
        pod: Pod,
        up_for: Duration,
        deadline: Instant,
        option: usize,
        failed_options: &mut Vec<usize>,
    ) -> std::result::Result<Look, Option<(Pod, usize)>> {
        let name = run.name.clone();
        let (what, why) = if pod.proxy_only {
            (
                "no public SSH endpoint (RunPod ssh proxy only)".to_string(),
                format!(
                    "{} for {} (its host has no public IP — no scp/rsync/port-forwards)",
                    fleet::PROXY_ONLY,
                    pipeline::fmt_elapsed(up_for)
                ),
            )
        } else {
            (
                "running with no SSH endpoint".to_string(),
                format!("listed running for {} with no SSH endpoint (no public IP/port)", pipeline::fmt_elapsed(up_for)),
            )
        };
        let Some(listings) = self.or_stop(self.run.provider.list_by_provider()).await else {
            return Err(self.end(run, Verdict::Stopped { stage: Stage::Endpoint }));
        };
        let in_time = Instant::now() < deadline;
        let pod = match now_listed(&listings, &pod) {
            Now::Gone => {
                run.pod = None;
                let reason = format!(
                    "{why}, and {} no longer lists it (terminated elsewhere?) — nothing left running",
                    pod.provider
                );
                return Err(self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason }));
            }
            Now::Reachable(p) => {
                (self.say)(To::Out, &format!("[{name}] {why} — but it has an endpoint now; going on"));
                return Ok(Look::Found(p));
            }
            Now::NotUp(p) if in_time => {
                let now = p.status.trim();
                (self.say)(To::Err, &format!("[{name}] {why} — but it's listed {now}, not up, now (restarting?): waiting on"));
                return Ok(Look::WaitOn(p));
            }
            Now::NotUp(p) => {
                let reason = format!("{why}; now not up{}", left_running(&name, Some(&p)));
                return Ok(Look::Expired { pod: p, reason });
            }
            Now::Unknown(e) if in_time => {
                (self.say)(
                    To::Err,
                    &format!("[{name}] {why} — but it can't be looked at again just now ({e}): waiting on, nothing terminated"),
                );
                return Ok(Look::WaitOn(pod));
            }
            Now::Unknown(e) => {
                let reason = format!(
                    "{why} as last seen — its state couldn't be read just now ({e}), so it isn't replaced{}",
                    left_running(&name, Some(&pod))
                );
                return Ok(Look::Expired { pod, reason });
            }
            Now::NoEndpoint(p) => p,
        };
        // Which machine: RunPod's listing names none, its details do (as for a ready pod). Not
        // waited for past a Ctrl+C (`replace` then leaves the pod as it is).
        let pod = match self.or_stop(self.with_machine(&name, pod.clone())).await {
            Some(p) => p,
            None => pod,
        };
        let place = where_label(&pod);
        let on = match &place {
            Some(p) => format!(" — on {p}"),
            None => format!(" — {} names no machine or data centre for it", pod.provider),
        };
        (self.say)(To::Err, &format!("[{name}] {why}{on}: a host failure"));
        if let Some(a) = run.attempts.last_mut() {
            a.end = AttemptEnd::NoEndpoint(what);
            a.host = place;
        }
        failed_options.push(option);
        run.pod = Some(pod.clone());
        Err(self.replace(run, &pod, failed_options, format!("{why}{on}"), Stage::Endpoint).await)
    }

    /// Remember `pod`'s machine as bad after its check `h` FAILed — for the rest of this run
    /// (every name's next pod, first attempts included) and, in the health cache, for the next
    /// runs — when [`snapshot::bad_host_record`] says the verdict may condemn it (a host FAIL,
    /// on a machine its provider names). Saving is best effort: a warning, never a failure.
    fn remember_bad_host(&self, name: &str, pod: &Pod, h: &PodHealth) {
        let now = snapshot::unix_now();
        let Some((key, bad)) = snapshot::bad_host_record(pod, h, now) else {
            return;
        };
        self.known_bad.lock().unwrap().insert(key.clone(), bad.clone());
        if let Some(w) = save_bad_host(self.run.cfg, &key, &bad, now) {
            (self.say)(To::Err, &format!("[{name}] {w}"));
        }
    }

    /// Settle a name: print its READY / FAILED / STOPPED line now — the moment it's known —
    /// and record the verdict.
    fn end(&self, run: &mut NameRun, verdict: Verdict) -> Option<(Pod, usize)> {
        let name = &run.name;
        match &verdict {
            Verdict::Ready => {
                let mut what = vec![self.option(run.option).map(PlacementOption::describe).unwrap_or_default()];
                if let Some(port) = run.port {
                    what.push(format!("proxy port {port}"));
                }
                if let Some(h) = &run.health {
                    what.push(format!("check {}", h.status.label()));
                }
                if run.locked {
                    what.push("locked".into());
                }
                let after = run.ready_at.map(|t| pipeline::fmt_elapsed(t - run.first_at)).unwrap_or_default();
                what.retain(|w| !w.is_empty());
                (self.say)(To::Out, &format!("[{name}] READY after {after} — {}", what.join(", ")));
            }
            Verdict::Failed { stage, reason } => (self.say)(To::Err, &format!("[{name}] FAILED {}: {reason}", stage.label())),
            Verdict::Stopped { stage } => {
                (self.say)(To::Err, &format!("[{name}] STOPPED at {} (Ctrl+C) — left as it is", stage.label()))
            }
        }
        run.verdict = verdict;
        None
    }

    /// Replace the name's pod: terminate it, wait until it's gone (never two pods under one
    /// name), then recreate the name through the placement executor — options that haven't
    /// failed first. Only while attempts remain and Ctrl+C hasn't been pressed: otherwise a
    /// pod that failed its check is left running for a look and the name FAILs. A pod
    /// rejected for its machine, or one that never came up or never got an endpoint, is
    /// terminated even with no attempt left (unless Ctrl+C): it was never set up, so there's
    /// nothing to look at — kept, it would only bill and take the name's proxy port. Whatever
    /// the end, the reason says what is left of the name and whether it is still billing.
    ///
    /// `stage` is where the name fails if it isn't replaced: `check` for a failed deep check
    /// or a rejected machine, `endpoint` for a pod that never came up or got an endpoint.
    async fn replace(
        &self,
        run: &mut NameRun,
        pod: &Pod,
        failed_options: &[usize],
        why: String,
        stage: Stage,
    ) -> Option<(Pod, usize)> {
        let say = self.say;
        let name = run.name.clone();
        // A failed placement — a pod that never came up (`dead`) or never got a usable endpoint
        // (`no_endpoint`) — is replaced within the name's placements even without --check;
        // every other replacement is --check's.
        let misplaced = run.attempts.last().is_some_and(|a| matches!(a.end, AttemptEnd::Dead(_) | AttemptEnd::NoEndpoint(_)));
        let max = match &self.run.check {
            Some(c) => c.attempts,
            None if misplaced => self.run.placements.max(1),
            None => 1,
        } as usize;
        let n = run.attempts.len();
        // Rejected (terminated even with no attempt left): a machine known bad, or a pod that
        // never came up or got an endpoint — never set up, nothing to look at, and it may bill.
        let rejected = misplaced || run.attempts.last().is_some_and(|a| matches!(a.end, AttemptEnd::SameHost | AttemptEnd::KnownBad(_)));
        let last = n >= max;
        if last && !rejected {
            let reason = format!("{why} — no attempts left (--check-attempts {max}); left running");
            return self.end(run, Verdict::Failed { stage, reason });
        }
        if self.stopped() {
            let reason = format!("{why} — not replaced (interrupted); left running, still billing{}", price_of(pod));
            return self.end(run, Verdict::Failed { stage, reason });
        }
        if last {
            say(To::Err, &format!("[{name}] terminating {} — no attempt left to replace it (--check-attempts {max})", pod.id));
        } else {
            say(To::Err, &format!("[{name}] terminating {} to recreate {name} (attempt {} of {max})", pod.id, n + 1));
        }
        // On the backend that listed it, by name (as the reaper does): never routed by the id
        // alone, which another backend's pod may share (Vast and Hetzner ids are both numbers).
        if let Err(e) = self.run.provider.terminate_pod_on(&pod.provider, &pod.id).await {
            let reason = format!(
                "{why}; terminating it failed: {e} — left running, still billing{} (`arena pods terminate {name}`)",
                price_of(pod)
            );
            return self.end(run, Verdict::Failed { stage, reason });
        }
        if let Some(a) = run.attempts.last_mut() {
            a.terminated = true;
        }
        run.pod = None;
        run.port = None;
        let gone = self.or_stop(self.wait_gone(pod)).await;
        if last {
            // Waited for all the same, so the final proxy sync doesn't route the name to it.
            let tail = match &gone {
                Some(Gone::StillListed) => format!(
                    " — {name} has no pod now ({} still listed after {}s: `arena proxy apply` once it's gone)",
                    pod.id,
                    GONE_TIMEOUT.as_secs()
                ),
                Some(Gone::Taken(other)) => format!(" — another pod named {name} ({other}) is listed now, not made by this run"),
                _ => format!(" — {name} has no pod now, nothing of it left billing"),
            };
            let reason = format!("{why} — no attempts left (--check-attempts {max}); terminated it{tail}");
            return self.end(run, Verdict::Failed { stage, reason });
        }
        match gone {
            None => {
                let reason = format!("{why}; terminated it, then Ctrl+C before its replacement — {name} has no pod now");
                return self.end(run, Verdict::Failed { stage, reason });
            }
            Some(Gone::StillListed) => {
                let reason = format!(
                    "{why}; {} was terminated but is still listed after {}s — no replacement created (it would \
                     duplicate the name)",
                    pod.id,
                    GONE_TIMEOUT.as_secs()
                );
                return self.end(run, Verdict::Failed { stage, reason });
            }
            Some(Gone::Taken(other)) => {
                let reason = format!(
                    "{why}; terminated it, but another pod named {name} ({other}) is listed now — no replacement \
                     created (two pods would share the name; see `arena pods list`)"
                );
                return self.end(run, Verdict::Failed { stage, reason });
            }
            Some(Gone::Yes) => {}
        }
        if self.stopped() {
            let reason = format!("{why}; terminated it, then Ctrl+C before its replacement — {name} has no pod now");
            return self.end(run, Verdict::Failed { stage, reason });
        }
        let order = pipeline::replacement_order(self.run.options.options.len(), failed_options);
        // A marketplace (Vast) is told the machines known bad by now — this run's too — so the
        // replacement isn't rented right back on one (RunPod can't be told: there a pod placed
        // on one is rejected again, within --check-attempts).
        let mut avoid = self.run.base.avoid_machines.clone();
        for m in snapshot::avoided_machines(&self.known_bad.lock().unwrap(), "vast") {
            if !avoid.contains(&m) {
                avoid.push(m);
            }
        }
        let base = PodSpec { avoid_machines: avoid, ..self.run.base.clone() };
        let plan = OptionPlan {
            options: order.iter().map(|&k| self.run.options.options[k].clone()).collect(),
            dropped: Vec::new(),
            ..self.run.options.clone()
        };
        if let Some(first) = plan.options.first() {
            say(To::Out, &format!("[{name}] recreating it — trying {} first", first.describe()));
        }
        let stop = self.stop.clone();
        let interrupt = move || {
            let mut stop = stop.clone();
            async move {
                let _ = stop.wait_for(|s| *s).await;
            }
        };
        let mut show = |p: &Progress| say(if p.is_created() { To::Out } else { To::Err }, &p.line());
        let names = [name.clone()];
        let placed =
            placement::place(self.run.provider, &base, &names, &plan, self.run.rounds, None, interrupt, &mut show)
                .await;
        if let Some(bal) = self.run.balance {
            bal.remember_prices(&placed.created);
        }
        match (placed.created.first(), placed.log.first().and_then(|l| l.placed)) {
            (Some(new), Some(k)) => Some((new.clone(), order[k])),
            _ => {
                let reason = format!("{why}; its replacement wasn't placed ({}) — {name} has no pod now", unplaced(&placed));
                self.end(run, Verdict::Failed { stage, reason })
            }
        }
    }

    /// The fleet listing, every `--interval`, while any pipeline waits on an endpoint — one
    /// list call for everyone, however many pods are coming up. Never returns.
    async fn poll_endpoints(&self) {
        let every = self.run.interval.max(Duration::from_secs(1));
        let provider = self.run.provider;
        loop {
            let busy = self.waiters.load(Ordering::SeqCst) > 0;
            if busy {
                match bounded_list(provider.name(), provider.list_pods(), LIST_TIMEOUT).await {
                    Ok(pods) => {
                        self.listing.send_replace(Arc::new(pods));
                    }
                    Err(e) => (self.say)(To::Err, &format!("  endpoint poll failed ({e}); retrying in {}s", every.as_secs())),
                }
                tokio::time::sleep(every).await;
            } else {
                // Idle: no list calls until a pipeline waits (or the next tick).
                tokio::select! {
                    () = tokio::time::sleep(every) => {}
                    () = self.waiter_added.notified() => {}
                }
            }
        }
    }

    /// Whether sshd answers on `target`, riding out the boot race like setup's first copy
    /// does: a refused/unreachable connection is retried every `boot.every` until
    /// `boot.window`; anything else (a key rejected, …) is an answer — and an error.
    async fn ssh_answers(&self, target: &SshTarget, boot: BootRetry) -> std::result::Result<(), String> {
        let deadline = Instant::now() + boot.window;
        loop {
            let why = match self.run.remote.exec(target, "true", Some(PROBE_TIMEOUT)).await {
                Ok(o) if o.success => return Ok(()),
                Ok(o) => format!("{}: {}", o.exit_label(), o.stderr.trim()),
                Err(e) => describe_error(&e),
            };
            if !looks_unreachable(&why) {
                return Err(why);
            }
            if Instant::now() + boot.every > deadline {
                return Err(format!("{why}; still unreachable after {}s", boot.window.as_secs()));
            }
            tokio::time::sleep(boot.every).await;
        }
    }

    /// How long `pod` may be up without an SSH endpoint before it's judged: `--endpoint-timeout`,
    /// plus whatever its backend holds its endpoint back for on purpose
    /// ([`status::endpoint_held_back`]: Shadeform's 60 s boot grace) — never counted against it
    /// (review of #39, nit B: `--endpoint-timeout 1` ended just as a healthy instance's IP was
    /// due).
    fn grace_for(&self, pod: &Pod) -> Duration {
        self.run.endpoint_timeout + status::endpoint_held_back(pod)
    }

    /// The pod `made` once the listing shows it with an SSH endpoint — or, once it has been up
    /// without one ([`status::up_without_endpoint`]: on RunPod, behind RunPod's SSH proxy only)
    /// for `--endpoint-timeout` — a whole one, past the deadline if need be — that; or at the
    /// deadline [`EndpointWait::TimedOut`] (still coming up — RunPod lists a pod RUNNING from its
    /// create, while its image is pulled). `seen`: a fresh look's view of it, for a wait resumed
    /// after one (the listings before it no longer count).
    ///
    /// It is found by its backend and id together, never the id alone: Vast's and Hetzner's ids
    /// are both numbers, and another backend's pod under the same id — up without an endpoint —
    /// would otherwise start this one's clock and be the pod condemned (review r4).
    ///
    /// The clock starts over only when a listing shows the pod not up (provisioning, loading,
    /// restarting; on RunPod, RUNNING with no container — a restart pulling its image again). A
    /// listing it is missing from proves nothing: the poller's aggregate listing skips a backend
    /// whose list failed, so one RunPod 5xx/429 drops every RunPod pod from it — and used to
    /// restart every proxy-only pod's grace, turning a replaceable host into `FAILED endpoint`,
    /// left billing. Nor does one whose endpoint lookup failed ([`runpod_v2::endpoint_lookup_failed`]:
    /// listed without its endpoint or proxy-only flag whatever it has). Either way a verdict may
    /// rest on an older listing: `Shared::no_endpoint` looks again before acting on it.
    ///
    /// `dead_on_error`: a backend where the deadline is a failed placement (a ready deadline, or
    /// pods that can't be stopped — Shadeform). A pod listed `ERROR` meanwhile is
    /// [`EndpointWait::Failed`] (elsewhere ERROR isn't known to be final, and the wait runs on as
    /// before), and the deadline is the end, grace or not: [`EndpointWait::TimedOut`], as before
    /// `--endpoint-timeout` existed — such a pod bills until deleted, so its grace never runs past
    /// it (review of #39, nit A).
    async fn wait_endpoint(&self, made: &Pod, deadline: Instant, dead_on_error: bool, seen: Option<Pod>) -> EndpointWait {
        let _waiting = Waiting::new(&self.waiters);
        self.waiter_added.notify_one();
        let mut rx = self.listing.subscribe();
        let grace = self.grace_for(made);
        // When it was first seen up without an endpoint (this stretch), and as last seen so.
        let mut bare: Option<(Instant, Pod)> = None;
        // A wait resumed after a fresh look starts from what that look saw, and from the
        // listings after it only — the latest one may be older than the look (review r3: a
        // restart the look saw was read over by the proxy-only listing before it).
        let mut skip_current = seen.is_some();
        // As the latest listing that said anything about it had it (for `--timeout`'s verdict).
        let mut last: Option<Pod> = seen;
        let at_deadline = |bare: Option<(Instant, Pod)>, last: Option<Pod>| match bare {
            Some((since, pod)) if !dead_on_error => EndpointWait::NoEndpoint { pod, up_for: since.elapsed() },
            _ => EndpointWait::TimedOut { last },
        };
        loop {
            let listed = rx.borrow_and_update().iter().find(|p| p.id == made.id && p.provider == made.provider).cloned();
            let listed = if std::mem::take(&mut skip_current) { None } else { listed };
            match listed {
                Some(p) if dead_on_error && p.status.trim().eq_ignore_ascii_case("ERROR") => {
                    let why = format!("error: {}", p.status_detail.as_deref().unwrap_or("listed ERROR, no details given"));
                    return EndpointWait::Failed { pod: p, why };
                }
                Some(p) if p.ssh_ip.is_some() && p.ssh_port.is_some() => return EndpointWait::Ready(p),
                // Its endpoint couldn't be looked up this time: no news.
                Some(p) if runpod_v2::endpoint_lookup_failed(&p) => {}
                Some(p) if status::up_without_endpoint(&p) => {
                    let since = bare.as_ref().map_or_else(Instant::now, |(t, _)| *t);
                    if since.elapsed() >= grace {
                        return EndpointWait::NoEndpoint { pod: p, up_for: since.elapsed() };
                    }
                    bare = Some((since, p.clone()));
                    last = Some(p);
                }
                // Listed, looked up, and not up (provisioning, loading, restarting; on RunPod,
                // RUNNING with no container — its image being pulled, again after a restart):
                // the clock starts over when it's up again.
                Some(p) => (bare, last) = (None, Some(p)),
                // Not in this listing: no news — the clock runs on.
                None => {}
            }
            // When the wait ends: the deadline — or, for a pod up without an endpoint, the end of
            // its clock when that's later: it is judged only after a whole `--endpoint-timeout`
            // of it (review r3), and one whose container came up late (a slow pull, then behind
            // the proxy only — live 2026-10-30, 6m39s) is replaced, not left running at the
            // deadline. Wake for the next listing, that end, or the end of a clock still running
            // (one already over waits for a listing that shows the pod — or the end). Not past a
            // deadline that is a failed placement (`dead_on_error`).
            let now = Instant::now();
            let grace_end = bare.as_ref().map(|(t, _)| *t + grace);
            let end = if dead_on_error { deadline } else { grace_end.map_or(deadline, |g| deadline.max(g)) };
            let wake = grace_end.filter(|g| *g > now).map_or(end, |g| g.min(end));
            // A listing first: at the end, one that arrived with it is looked at before the
            // verdict (it may hold the endpoint).
            tokio::select! {
                biased;
                changed = rx.changed() => if changed.is_err() { return at_deadline(bare, last) },
                () = tokio::time::sleep_until(wake) => {
                    if Instant::now() >= end {
                        return at_deadline(bare, last);
                    }
                }
            }
        }
    }

    /// Whether the terminated `pod` has left its provider's listing (or is listed as
    /// terminated) within [`GONE_TIMEOUT`]. Asked of the backend that owns it, not the fleet
    /// aggregate: that skips a backend that fails to list, and "not in RunPod's listing"
    /// read off a listing RunPod isn't in would create the replacement next to the old pod.
    /// So a failed (or absent) owner listing proves nothing — it just counts as "not yet".
    ///
    /// The same listings also show whether *another* pod has taken the name meanwhile (made
    /// outside this pipeline — another terminal, or a create loop that read the name as free
    /// while the old pod was going), on any backend that answered: then no replacement may be
    /// created ([`Gone::Taken`]).
    async fn wait_gone(&self, pod: &Pod) -> Gone {
        let every = self.run.interval.max(Duration::from_secs(1));
        let deadline = Instant::now() + GONE_TIMEOUT;
        let live = |p: &Pod| !p.status.eq_ignore_ascii_case("TERMINATED");
        loop {
            // Per backend, each list bounded (`list_by_provider`'s contract).
            let listings = self.run.provider.list_by_provider().await;
            let mut answered = listings.iter().filter_map(|(_, l)| l.as_ref().ok()).flatten();
            if let Some(twin) = answered.find(|p| p.name == pod.name && p.id != pod.id && live(p)) {
                return Gone::Taken(twin.id.clone());
            }
            if gone_from_owner(&listings, pod) {
                return Gone::Yes;
            }
            if Instant::now() + every > deadline {
                return Gone::StillListed;
            }
            tokio::time::sleep(every).await;
        }
    }

    /// Merge this pod's endpoint into the proxy — holding the gate, so pipelines never write
    /// the config concurrently — and say which port it got.
    async fn sync_proxy_for(&self, run: &mut NameRun) {
        let outcome = {
            let _one_writer = self.proxy_gate.lock().await;
            proxy_sync_now(self.run.cfg, self.run.provider).await
        };
        let name = &run.name;
        match &outcome {
            ProxySync::Synced { routed, .. } => match routed.iter().find(|f| &f.name == name) {
                Some(f) => {
                    run.port = Some(f.public_port);
                    (self.say)(To::Out, &format!("[{name}] proxy: port {} → {}", f.public_port, f.target()));
                }
                None => (self.say)(
                    To::Err,
                    &format!("[{name}] proxy: synced, but no forward for it — not on MACHINE_NAME_LIST? (`arena proxy plan` says why)"),
                ),
            },
            ProxySync::Failed(e) => (self.say)(
                To::Err,
                &format!("[{name}] proxy NOT synced — {} (the final sync retries; or `arena proxy apply`)", one_line(e)),
            ),
            ProxySync::Skipped(why) => (self.say)(To::Out, &format!("[{name}] proxy skipped — {why}")),
        }
    }

    /// The deep check on one pod (one bounded exec, judged as `pods test --deep` judges it),
    /// with the provider's best-effort details looked up alongside ([`Self::details_of`]):
    /// the host's maintenance window, and the GPU's display name — so a MIG slice is labelled
    /// as `pods list` labels it (`B300 MIG 34GB`), not by the catalog id REST v2 lists (live
    /// BPF S6). Never fails a check: no details, no window, the listed name.
    async fn deep_check(&self, name: &str, pod: &Pod, target: &SshTarget, check: &CheckStage) -> PodHealth {
        let call = self.run.remote.exec(target, &check.cmd, Some(DEEP_CHECK_TIMEOUT));
        let (details, call) = tokio::join!(self.details_of(name, pod), call);
        let pod = Pod {
            maintenance: details.as_ref().and_then(|d| d.maintenance.clone()),
            gpu_type: details.and_then(|d| d.gpu_type).or_else(|| pod.gpu_type.clone()),
            ..pod.clone()
        };
        judge_deep_call(&pod, call.map_err(|e| describe_error(&e)), &check.policy)
    }

    /// `pod` with the machine id from the provider's best-effort details when its listing
    /// has none (RunPod: REST v2 reports no machine; GraphQL's `machineId` does). Shares the
    /// details fetch with [`Self::deep_check`]. Without details it stays as it was — and
    /// the same-host check falls back to the IP, as before.
    async fn with_machine(&self, name: &str, pod: Pod) -> Pod {
        if pod.machine_id.is_some() || pod.provider != "runpod" {
            return pod;
        }
        let machine_id = self.details_of(name, &pod).await.and_then(|p| p.machine_id);
        Pod { machine_id, ..pod }
    }

    /// `pod` as the provider's best-effort details have it: one fetch for the whole listing,
    /// reused for [`DETAILS_TTL`] (refetched sooner only for a pod it doesn't cover, e.g. a
    /// replacement). `None` when the details don't cover it.
    async fn details_of(&self, name: &str, pod: &Pod) -> Option<Pod> {
        let mut cache = self.details.lock().await;
        let fresh = cache.as_ref().is_some_and(|(at, pods)| at.elapsed() < DETAILS_TTL && pods.iter().any(|p| p.id == pod.id));
        if !fresh {
            let mut pods: Vec<Pod> = self.listing.borrow().iter().cloned().collect();
            if !pods.iter().any(|p| p.id == pod.id) {
                pods.push(pod.clone());
            }
            if let Some(w) = enrich_best_effort(self.run.provider, &mut pods, ENRICH_TIMEOUT).await {
                (self.say)(To::Err, &format!("[{name}] {w}"));
            }
            *cache = Some((Instant::now(), pods));
        }
        cache.as_ref().and_then(|(_, pods)| pods.iter().find(|p| p.id == pod.id)).cloned()
    }

    /// This pod's API keys (its per-host CSV keys + the broadcast tokens) and the fleet SSH
    /// map, in one bounded exec — `pods copy-keys` for exactly this pod. `Ok` carries the
    /// line to print and the fleet-SSH write it made (none if there was nothing to send). With
    /// a backup archive, the pod gets its access even with no key for it (A-ckl9).
    async fn copy_keys(
        &self,
        name: &str,
        pod: &Pod,
        target: &SshTarget,
        keys: &KeySources,
    ) -> std::result::Result<(To, String, Option<String>), String> {
        let (vars, matched) = keys.vars_for(&pod.name);
        let archive = super::archive::pod_archive(self.run.cfg);
        let none_for_it = format!("keys: none for it (no per-host key matched {name}, no broadcast token set)");
        if vars.is_empty() && archive.ready().is_none() {
            return Ok((To::Err, format!("[{name}] {none_for_it}"), None));
        }
        let fleet_pods = self.listing.borrow().clone();
        let (fleet_ssh, _) = fleet_ssh_for(self.run.cfg, &fleet_pods);
        let written = self.run.remote.exec(target, &copy_keys_command(&vars, &fleet_ssh), Some(COPY_KEYS_TIMEOUT)).await;
        // The backup archive's read key (when this user has an archive), by scp — as `copy-keys`.
        let archive_note = match (&written, archive) {
            (Ok(o), super::archive::PodArchive::Ready(a)) if o.success => {
                super::archive::install_read_key(self.run.remote.as_ref(), target, &a, COPY_KEYS_TIMEOUT).await.err()
            }
            (Ok(o), unreadable @ super::archive::PodArchive::Unreadable(_)) if o.success => unreadable.warning(),
            _ => None,
        };
        if let Some(e) = archive_note {
            let done = if vars.is_empty() { "fleet ssh" } else { "keys" };
            return Ok((To::Err, format!("[{name}] {done} ✓ — backup archive: {e}"), Some(fleet_ssh)));
        }
        match written {
            // No key for it, only the archive's access: said as such — a warning only when a key
            // file exists that didn't name it.
            Ok(o) if o.success && vars.is_empty() => {
                let to = if keys.per_host.is_empty() { To::Out } else { To::Err };
                let why = if keys.per_host.is_empty() { "keys: none generated".to_string() } else { none_for_it };
                Ok((to, format!("[{name}] backup archive access ✓ ({why})"), Some(fleet_ssh)))
            }
            Ok(o) if o.success && (matched || keys.per_host.is_empty()) => Ok((To::Out, format!("[{name}] keys ✓"), Some(fleet_ssh))),
            Ok(o) if o.success => Ok((
                To::Err,
                format!("[{name}] keys ✓ — broadcast tokens only: no per-host key in keys/*_api_keys.csv matched {name}"),
                Some(fleet_ssh),
            )),
            Ok(o) => Err(format!("copying its keys: {}: {}", o.exit_label(), o.stderr.trim())),
            Err(e) => Err(format!("copying its keys: {}", describe_error(&e))),
        }
    }

    /// A READY pod's keys stage wrote its `~/.ssh/config` fleet map from the listing of that
    /// moment. With a proxy layout that map is the stable port list and never changes; without
    /// one it names only the pods that had an endpoint by then — so a pod READY early would
    /// never learn of one whose endpoint came later (or of a replacement), where the batch flow
    /// wrote every map after the last endpoint. So: re-render it from the final listing and
    /// rewrite it (the fleet-SSH half only; one bounded exec each, side by side) on every READY
    /// pod whose map that changes. Best effort — a failure is reported, the pod stays READY.
    async fn refresh_fleet_ssh(&self, names: &[NameRun], pods: &[Pod]) {
        let (fresh, _) = fleet_ssh_for(self.run.cfg, pods);
        let stale: Vec<(&str, SshTarget)> = names
            .iter()
            .filter(|r| r.verdict.is_ready() && r.fleet_ssh.as_ref().is_some_and(|wrote| *wrote != fresh))
            .filter_map(|r| {
                let pod = r.pod.as_ref()?;
                let now = pods.iter().find(|p| p.id == pod.id).unwrap_or(pod);
                Some((r.name.as_str(), SshTarget::from_pod(now, self.run.cfg).ok()?))
            })
            .collect();
        let fresh = &fresh;
        let writes = stale
            .iter()
            .map(|(name, target)| async move { (*name, self.run.remote.exec(target, fresh, Some(COPY_KEYS_TIMEOUT)).await) })
            .collect();
        for (name, written) in join_unordered(writes).await {
            match written {
                Ok(o) if o.success => {
                    (self.say)(To::Out, &format!("[{name}] fleet SSH map updated with the pods that came up after it"))
                }
                Ok(o) => (self.say)(
                    To::Err,
                    &format!(
                        "[{name}] fleet SSH map NOT updated ({}: {}) — `arena pods copy-keys {name}`",
                        o.exit_label(),
                        one_line(&o.stderr)
                    ),
                ),
                Err(e) => (self.say)(
                    To::Err,
                    &format!("[{name}] fleet SSH map NOT updated ({}) — `arena pods copy-keys {name}`", describe_error(&e)),
                ),
            }
        }
    }

    /// After the pipelines: one more proxy merge from a fresh listing (an endpoint can move
    /// while setup runs; a replaced pod's forward follows its name), the fleet SSH maps that
    /// listing outdates, then the rows — GPU and $/h from one listing with the provider's
    /// best-effort details.
    async fn report(&self, names: Vec<NameRun>) -> Vec<UpRow> {
        let say = self.say;
        let synced = match &self.run.proxy {
            Err(why) => ProxySync::Skipped(why.clone()),
            Ok(()) => proxy_sync_now(self.run.cfg, self.run.provider).await,
        };
        say(if matches!(synced, ProxySync::Failed(_)) { To::Err } else { To::Out }, &sync_line("up", &synced));
        let routed: HashMap<String, u16> = match &synced {
            ProxySync::Synced { routed, .. } => routed.iter().map(|f| (f.name.clone(), f.public_port)).collect(),
            _ => HashMap::new(),
        };
        let provider = self.run.provider;
        // Per backend (each list bounded): the rows take whatever answered; a fleet map is
        // only rewritten from a listing every backend answered — one missing a backend would
        // drop that backend's pods from maps that had them.
        let listing = Listing::from_results(provider.list_by_provider().await);
        let mut pods: Vec<Pod> = listing.pods();
        if listing.all_ok() && !self.stopped() {
            self.refresh_fleet_ssh(&names, &pods).await;
        }
        // `SSH_CONFIG_FILE` (and staff's `SSH_ADMIN_CONFIG_FILE`) from the same listing: the endpoints as the pods ended up — the
        // pipelines' own syncs are the proxy's only (`proxy_sync_now`, not the lifecycle hook),
        // and a create rarely has an endpoint yet. Never from a listing a backend didn't answer.
        if listing.all_ok() {
            // No home under test: a `~/` value can't reach the developer's own ~/.ssh.
            let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
            let synced = arena_core::sshconfig::sync_all(self.run.cfg, &pods, home.as_deref());
            for line in crate::ssh_config_lines("up", synced) {
                say(if line.starts_with("warning:") { To::Err } else { To::Out }, &line);
            }
        }
        // The standing pods' checks, for `arena snapshot` (a pod terminated for its FAIL is
        // gone — the listing prunes any record of it).
        if self.run.check.is_some() {
            let checked: Vec<PodHealth> =
                names.iter().filter(|r| r.pod.is_some()).filter_map(|r| r.health.clone()).collect();
            if !checked.is_empty() {
                if let Some(warning) = record_health(self.run.cfg, &checked, Some(&listing)) {
                    say(To::Err, &warning);
                }
            }
        }
        if let Some(w) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
            say(To::Err, &w);
        }
        names
            .into_iter()
            .map(|r| {
                // GPU, $/h and port describe the name's pod now — none once we terminated it
                // without a replacement (HEALTH keeps the check that got it terminated).
                let listed = r.pod.as_ref().and_then(|p| pods.iter().find(|l| l.id == p.id));
                let option = r.pod.as_ref().and_then(|_| self.option(r.option));
                let known = |s: String| (s != "-").then_some(s);
                let gpu = r
                    .health
                    .as_ref()
                    .filter(|_| r.pod.is_some())
                    .and_then(|h| h.facts.as_ref())
                    .map(|f| f.gpu_label())
                    .and_then(known)
                    .or_else(|| listed.map(fleet::gpu_label).and_then(known))
                    .or_else(|| option.map(|o| o.label.clone()))
                    .unwrap_or_else(|| "-".into());
                let price = listed
                    .map(fleet::price_label)
                    .and_then(known)
                    .or_else(|| option.map(option_price))
                    .unwrap_or_else(|| "-".into());
                let proxy_port = r.pod.as_ref().and_then(|_| routed.get(&r.name).copied().or(r.port));
                UpRow {
                    gpu,
                    price,
                    proxy_port,
                    health: r.health.as_ref().map(|h| h.status),
                    health_notes: r.health.as_ref().map(PodHealth::notes).unwrap_or_default(),
                    ready_after: r.ready_at.map(|t| t - r.first_at),
                    name: r.name,
                    verdict: r.verdict,
                    attempts: r.attempts,
                }
            })
            .collect()
    }
}

/// ` ($0.17/h)` — what a pod left running costs, as its listing priced it; empty when unpriced.
/// Pure.
fn price_of(pod: &Pod) -> String {
    match fleet::price_label(pod) {
        p if p == "-" => String::new(),
        p => format!(" ({p}/h)"),
    }
}

/// The end of the reason for a pod `up` gives up on without terminating it: what it was last
/// listed as, that it's left running, whether it's billing, and what to do — `, still listed
/// RUNNING (no container yet — its image may still be pulling) — left running, still billing
/// ($0.17/h): `arena proxy apply` once it has an endpoint, or `arena pods terminate apple``.
/// Pure.
fn left_running(name: &str, last: Option<&Pod>) -> String {
    let state = match last {
        Some(p) => {
            let pulling = if no_container(p) { " (no container yet — its image may still be pulling)" } else { "" };
            format!(", still listed {}{pulling}", p.status.trim())
        }
        None => String::new(),
    };
    let billing = match last {
        Some(p) if status::bills_hourly(&p.provider, &p.status) => format!(", still billing{}", price_of(p)),
        _ => String::new(),
    };
    format!("{state} — left running{billing}: `arena proxy apply` once it has an endpoint, or `arena pods terminate {name}`")
}

/// Whether `pod` is a RunPod pod listed RUNNING with no container yet: no endpoint, not behind
/// RunPod's SSH proxy (that is a container up), and its endpoint looked up (a failed lookup lists
/// any pod so). RunPod's RUNNING is the pod's desired status, listed from the create while its
/// image is pulled — or while one that never starts isn't. Pure.
fn no_container(pod: &Pod) -> bool {
    pod.provider == "runpod"
        && pod.status.trim().eq_ignore_ascii_case("RUNNING")
        && !pod.proxy_only
        && (pod.ssh_ip.is_none() || pod.ssh_port.is_none())
        && !runpod_v2::endpoint_lookup_failed(pod)
}

/// Where a pod is, as far as its provider says: `machine 4x2k9, data centre EU-RO-1`, either
/// half alone, or `None` when it names neither (RunPod COMMUNITY pods have no data centre; v2's
/// listing names no machine — `Shared::with_machine` fills it from the details). What `up`
/// says about a pod that never got an endpoint, so a host that keeps doing it can be avoided.
/// Pure.
fn where_label(pod: &Pod) -> Option<String> {
    let given = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let parts: Vec<String> = [given(&pod.machine_id).map(|m| format!("machine {m}")), given(&pod.location).map(|dc| format!("data centre {dc}"))]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Error text onto one line (it often embeds a command's multi-line stderr).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `pod`'s own backend answered in `listings` (per backend, `list_by_provider`) and
/// lists it no more — or only as terminated. A backend that failed to list, or none that
/// owns it, proves nothing (`false`): the aggregate listing skips a failed backend, and "not
/// in a listing RunPod isn't in" must never read as gone. Pure.
fn gone_from_owner(listings: &[(String, arena_core::Result<Vec<Pod>>)], pod: &Pod) -> bool {
    now_listed(listings, pod) == Now::Gone
}

/// What `Shared::no_endpoint` makes of a pod it didn't condemn or give up on.
enum Look {
    /// It has an endpoint after all: the pipeline goes on with it.
    Found(Pod),
    /// Wait on (its clock started over), the pod as this look saw it — or, unread, as last seen.
    WaitOn(Pod),
    /// Past the deadline, not up now or not readable: the pod as this look saw it (or as last
    /// seen), and the reason it is left running with — where expiry isn't a failed placement.
    Expired { pod: Pod, reason: String },
}

/// What a fresh look at a pod up found ([`now_listed`]).
#[derive(Debug, PartialEq)]
enum Now {
    /// Not listed by its own backend (or only as terminated).
    Gone,
    /// Listed with an SSH endpoint.
    Reachable(Pod),
    /// Listed up without an endpoint ([`status::up_without_endpoint`]), its endpoint looked up.
    NoEndpoint(Pod),
    /// Listed, but not up (restarting, provisioning, stopped; on RunPod RUNNING with no
    /// container — its image being pulled).
    NotUp(Pod),
    /// Its backend didn't answer, its answer left the pod's endpoint unknown (a failed RunPod
    /// lookup), or no backend owns it: why.
    Unknown(String),
}

/// `pod` as its own backend lists it now, in `listings` (per backend, `list_by_provider`): what
/// `up` may act on before terminating a pod for its endpoint. A backend that failed to list, or
/// none that owns the pod, is [`Now::Unknown`] — never "gone", never "still none". Pure.
fn now_listed(listings: &[(String, arena_core::Result<Vec<Pod>>)], pod: &Pod) -> Now {
    let owner = match listings.iter().find(|(p, _)| *p == pod.provider) {
        Some(l) => Some(l),
        None => match listings {
            [only] => Some(only),
            _ => None,
        },
    };
    let pods = match owner {
        None => return Now::Unknown(format!("no {} listing", pod.provider)),
        Some((_, Err(e))) => return Now::Unknown(one_line(&e.to_string())),
        Some((_, Ok(pods))) => pods,
    };
    let Some(p) = pods.iter().find(|p| p.id == pod.id && !p.status.eq_ignore_ascii_case("TERMINATED")) else {
        return Now::Gone;
    };
    if p.ssh_ip.is_some() && p.ssh_port.is_some() {
        Now::Reachable(p.clone())
    } else if runpod_v2::endpoint_lookup_failed(p) {
        Now::Unknown(p.status_detail.clone().unwrap_or_default())
    } else if status::up_without_endpoint(p) {
        Now::NoEndpoint(p.clone())
    } else {
        Now::NotUp(p.clone())
    }
}

/// Whether a deep check's script ran start to end — the only kind of check that says
/// anything about the machine. No facts (ssh failed, the call timed out), no header (it
/// never started), or no trailer (the session was cut mid-run) is a check that couldn't run.
fn script_ran(h: &PodHealth) -> bool {
    h.facts.as_ref().is_some_and(|f| f.started && f.complete)
}

/// The machines an earlier `up --check` found bad and still remembered at `now`, from this
/// fleet's health cache (`MACHINE_NAME_PREFIX`-scoped, like the verdicts beside them) — plus
/// a warning when the cache couldn't be read: then none are. A cache that can't be read only
/// costs the memory, never a create.
pub(crate) fn remembered_bad_hosts(cfg: &Config, now: u64) -> (BTreeMap<String, BadHost>, Option<String>) {
    match super::health_cache_path(cfg) {
        None => (BTreeMap::new(), None),
        Some(Err(e)) => (BTreeMap::new(), Some(format!("warning: no health cache to remember bad machines in: {e}"))),
        Some(Ok(path)) => {
            let (cache, warning) = snapshot::HealthCache::load(&path);
            (cache.live_bad_hosts(now), warning)
        }
    }
}

/// Save a machine found bad to the health cache ([`snapshot::record_bad_host`]); a warning
/// line when that didn't work, or when the old file had to be ignored.
fn save_bad_host(cfg: &Config, key: &str, bad: &BadHost, now: u64) -> Option<String> {
    let path = match super::health_cache_path(cfg)? {
        Ok(p) => p,
        Err(e) => return Some(format!("warning: {key} not remembered as bad for later runs: {e}")),
    };
    match snapshot::record_bad_host(&path, key, bad, now) {
        Ok(warning) => warning,
        Err(e) => Some(format!("warning: {key} not remembered as bad for later runs ({}: {e})", path.display())),
    }
}

/// Whether SSH worked but the check script never started ([`health::judge_deep_call`] keeps
/// facts only when the session did): the pod lacks what runs it — zsh before setup, `base64`.
fn never_started(h: &PodHealth) -> bool {
    h.facts.as_ref().is_some_and(|f| !f.started)
}

/// The pipelines end to end over a scripted fleet (`Fleet`) and `FakeRemote`, on a paused
/// clock: every endpoint delay, budget and poll interval elapses instantly and exactly, so
/// "A was READY before B's endpoint existed" is a statement about tokio time.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_tests::{DEEP_CUINIT_999, DEEP_HEALTHY};
    use arena_core::placement::{plan_options, Order, PriceBook, Request};
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use arena_core::{Error, Result as CoreResult};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};

    const A4000: &str = "NVIDIA RTX A4000";
    const R3090: &str = "NVIDIA GeForce RTX 3090";
    /// How the attempt log names a proxy-only host.
    const PROXY_ONLY_END: &str = "no public SSH endpoint (RunPod ssh proxy only)";

    /// A RunPod-like fleet: a create makes a pod with no endpoint, which gets the next
    /// endpoint scripted for its name once that endpoint's delay has passed; a terminated pod
    /// leaves the listing after `linger` (at once by default). Records creates/terminates in
    /// order, counts list calls, and how many `list_by_provider` calls overlap (the proxy
    /// test makes each take `sync_delay`, so overlapping proxy writers would show).
    #[derive(Default)]
    struct Fleet {
        endpoints: Mutex<Endpoints>,
        pods: Mutex<Vec<Slot>>,
        events: Mutex<Vec<String>>,
        next_id: AtomicUsize,
        lists: AtomicUsize,
        syncing: AtomicUsize,
        max_syncing: AtomicUsize,
        /// `enrich` calls (RunPod: one GraphQL query each); it puts a maintenance window on
        /// `devtest-cloud`.
        enriched: AtomicUsize,
        sync_delay: Duration,
        /// How long a terminated pod stays listed.
        linger: Duration,
        /// After a terminate, RunPod's own listing fails for this long while a second
        /// (empty) backend answers — a multi-provider fleet, whose aggregate `list_pods`
        /// skips the failed backend. Non-zero also makes `list_by_provider` list both.
        down_after_terminate: Duration,
        down_until: Mutex<Option<Instant>>,
        /// After a terminate, creating that name hits "no capacity" for this long.
        dry_after_terminate: Duration,
        dry_until: Mutex<Option<(String, Instant)>>,
        /// Can re-attach SSH keys through its API (like Vast); records `authorize <id> <n>`.
        attaches: bool,
        /// Vast-like machines: while any are left, each create takes the next machine id and
        /// its pod is tagged `vast` (so only that id names its machine, never its IP).
        machines: Mutex<VecDeque<String>>,
        /// Pod ids whose lock fails (a 500 the retry gives up on would look the same).
        lock_refused: Mutex<Vec<String>>,
        /// RunPod-like machine ids, by SSH port: the listing (REST v2) has none, the details
        /// (`enrich`, GraphQL `machineId`) fill them in.
        details_machines: Mutex<HashMap<u16, String>>,
        /// RunPod-like GPU display names, by SSH port: the details (GraphQL `gpuDisplayName`)
        /// name the GPU the listing gives by its catalog id (REST v2).
        details_gpus: Mutex<HashMap<u16, String>>,
        /// Answers `lock_support` (RunPod v2 can lock); off, it's the default refusal.
        lockable: bool,
        /// A terminate is followed at once by another pod under the same name (`twin`, never
        /// up) — made outside the pipeline, as another terminal could — listed by this backend
        /// (`runpod`, with the rest; `hetzner`, a second backend listing only it).
        twin_after_terminate: Option<&'static str>,
        /// What the second backend (`hetzner`) lists, when it is listed.
        hetzner: Mutex<Vec<Pod>>,
        /// Other backends' pods (by their `provider` tag), listed before this one's — in the
        /// aggregate listing and in their own backend's: ids from two backends may collide.
        foreign: Mutex<Vec<Pod>>,
        /// Each create's `avoid_machines`, in order.
        avoids: Mutex<Vec<Vec<String>>>,
        /// A Shadeform-like backend: its new pods get this ready deadline
        /// (`Provider::ready_deadline`). An endpoint scripted as `error` lists its pod `ERROR`
        /// ("provider failed to launch") once its delay has passed — on any fake.
        ready: Option<Duration>,
        /// What a pod is listed as before its scripted time. `None`: `RUNNING`, as RunPod lists a
        /// pod from its create while its image pulls (live 2026-10-30, v1 and v2 alike); Vast
        /// `LOADING`, Hetzner `initializing`, Shadeform `PENDING` otherwise.
        pending: Option<&'static str>,
        /// The provider new pods are tagged with (`None`: `runpod`, or `vast` with `machines`).
        tag: Option<&'static str>,
        /// The data centre new pods are listed in.
        dc: Option<&'static str>,
        /// RunPod-like machine ids by pod id: the details (GraphQL `machineId`) name a pod's
        /// machine whether or not it has an endpoint.
        machines_by_id: Mutex<HashMap<String, String>>,
        /// Per name: what its next create is listed as later, phase by phase — `(ip, port, after)`
        /// from the create, as in the script — so a pod can first be up without an endpoint
        /// (`proxy`, `bare`), restart (`bare` on RunPod: RUNNING with no container; `pending`:
        /// listed PENDING), and get one.
        later: Mutex<Phases>,
        /// Until then RunPod's endpoint lookups fail: a pod without an endpoint is listed with
        /// `status_detail` saying so and no proxy-only flag, as `runpod_v2::fill_endpoints` lists it.
        lookups_fail_until: Mutex<Option<Instant>>,
    }

    /// A pod in the fake: its endpoint from `from` (and each of `later`'s from its time);
    /// listed until `gone_at`.
    struct Slot {
        pod: Pod,
        from: Instant,
        ip: String,
        port: u16,
        gone_at: Option<Instant>,
        later: Vec<(Instant, String, u16)>,
    }

    /// Per pod name: the endpoint each of its creates gets, in order — `(ip, port, after)`.
    type Endpoints = HashMap<String, VecDeque<(String, u16, Duration)>>;

    /// Per pod name: what its next create is listed as later, phase by phase — `(ip, port, after)`.
    type Phases = HashMap<String, Vec<(String, u16, Duration)>>;

    /// One name's script: `(name, [(ip, port, endpoint after N s)])`.
    type Script<'a> = (&'a str, &'a [(&'a str, u16, u64)]);

    impl Fleet {
        /// `(name, [(ip, port, endpoint after N s)])` — one endpoint per create of that name.
        fn new(script: &[Script<'_>]) -> Self {
            let fleet = Fleet::default();
            for (name, eps) in script {
                let q = eps.iter().map(|(ip, port, s)| (ip.to_string(), *port, Duration::from_secs(*s))).collect();
                fleet.endpoints.lock().unwrap().insert(format!("devtest-{name}"), q);
            }
            fleet
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
        fn listed(&self) -> Vec<Pod> {
            let now = Instant::now();
            let pods = self.pods.lock().unwrap();
            let lookups_fail = self.lookups_fail_until.lock().unwrap().is_some_and(|t| now < t);
            pods.iter()
                .filter(|s| s.gone_at.is_none_or(|g| now < g))
                .map(|s| {
                    let mut p = s.pod.clone();
                    let phase = s.later.iter().rev().find(|(at, _, _)| now >= *at);
                    let (ip, port) = match phase {
                        Some((_, ip, port)) => (ip.as_str(), *port),
                        None => (s.ip.as_str(), s.port),
                    };
                    if now >= s.from || phase.is_some() {
                        // `proxy` = a host with no public IP: up, behind RunPod's proxy only.
                        // `bare` = up (listed running) with no endpoint and no proxy either.
                        if ip == "proxy" {
                            p.proxy_only = true;
                        } else if ip == "error" {
                            p.status = "ERROR".into();
                            p.status_detail = Some("provider failed to launch".into());
                        } else if ip == "pending" {
                            // Not up (again): listed PENDING, no endpoint.
                            p.status = "PENDING".into();
                        } else if ip != "bare" {
                            (p.ssh_ip, p.ssh_port) = (Some(ip.to_string()), Some(port));
                        }
                    } else if let Some(pending) = self.pending {
                        p.status = pending.into();
                    }
                    if lookups_fail && p.provider == "runpod" && p.ssh_ip.is_none() {
                        p.proxy_only = false;
                        p.status_detail = Some("SSH endpoint lookup failed: endpoint lookup for it timed out after 5s".into());
                    }
                    p
                })
                .collect()
        }
        fn runpod_down(&self) -> bool {
            self.down_until.lock().unwrap().is_some_and(|t| Instant::now() < t)
        }
    }

    #[async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            // The aggregate skips a backend that failed: RunPod down → just the empty other one.
            let mut out = self.foreign.lock().unwrap().clone();
            out.extend(if self.runpod_down() { Vec::new() } else { self.listed() });
            Ok(out)
        }
        async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
            let now = self.syncing.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_syncing.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.sync_delay).await;
            self.syncing.fetch_sub(1, Ordering::SeqCst);
            let runpod = if self.runpod_down() { Err(Error::provider("runpod: HTTP 502 Bad Gateway")) } else { Ok(self.listed()) };
            let mut out = Vec::new();
            let foreign = self.foreign.lock().unwrap().clone();
            let mut backends: Vec<String> = foreign.iter().map(|p| p.provider.clone()).collect();
            backends.dedup();
            for b in backends {
                let pods = foreign.iter().filter(|p| p.provider == b).cloned().collect();
                out.push((b, Ok(pods)));
            }
            out.push(("runpod".to_string(), runpod));
            let hetzner = self.hetzner.lock().unwrap().clone();
            if !self.down_after_terminate.is_zero() || !hetzner.is_empty() {
                out.push(("hetzner".into(), Ok(hetzner)));
            }
            out
        }
        async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
            self.events.lock().unwrap().push(format!("create {} {}", spec.name, spec.gpu_type));
            self.avoids.lock().unwrap().push(spec.avoid_machines.clone());
            // The invariant `--check` must keep: a pod it terminated is gone from the listing
            // before the name is created again — never two pods under one name.
            assert!(!self.listed().iter().any(|p| p.name == spec.name), "a second pod named {} while one is listed", spec.name);
            if self.dry_until.lock().unwrap().as_ref().is_some_and(|(n, t)| *n == spec.name && Instant::now() < *t) {
                return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
            }
            let mut pods = self.pods.lock().unwrap();
            let Some((ip, port, after)) = self.endpoints.lock().unwrap().get_mut(&spec.name).and_then(VecDeque::pop_front) else {
                return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
            };
            let mut pod = Pod {
                id: format!("id{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1),
                name: spec.name.clone(),
                provider: "runpod".into(),
                status: "RUNNING".into(),
                gpu_type: Some(spec.gpu_type.clone()),
                gpu_count: Some(spec.gpu_count),
                cost_per_hr: Some(0.17),
                ..Default::default()
            };
            if let Some(machine) = self.machines.lock().unwrap().pop_front() {
                (pod.provider, pod.machine_id) = ("vast".into(), Some(machine));
            }
            if let Some(tag) = self.tag {
                pod.provider = tag.into();
            }
            pod.location = self.dc.map(String::from);
            let later = self.later.lock().unwrap().remove(&spec.name).unwrap_or_default();
            let later = later.into_iter().map(|(ip, port, after)| (Instant::now() + after, ip, port)).collect();
            pods.push(Slot { pod: pod.clone(), from: Instant::now() + after, ip, port, gone_at: None, later });
            Ok(pod)
        }
        async fn enrich(&self, pods: &mut [Pod]) -> CoreResult<()> {
            self.enriched.fetch_add(1, Ordering::SeqCst);
            let machines = self.details_machines.lock().unwrap().clone();
            let by_id = self.machines_by_id.lock().unwrap().clone();
            let gpus = self.details_gpus.lock().unwrap().clone();
            for p in pods.iter_mut().filter(|p| p.provider == "runpod") {
                if let Some(m) = p.ssh_port.and_then(|port| machines.get(&port)).or_else(|| by_id.get(&p.id)) {
                    p.machine_id = Some(m.clone());
                }
                if let Some(g) = p.ssh_port.and_then(|port| gpus.get(&port)) {
                    p.gpu_type = Some(g.clone());
                }
            }
            for p in pods.iter_mut().filter(|p| p.name == "devtest-cloud") {
                p.maintenance = Some(arena_core::pod::Maintenance {
                    start: Some("2026-10-09T02:00:00Z".into()),
                    end: Some("2026-10-09T06:00:00Z".into()),
                    note: None,
                });
            }
            Ok(())
        }
        async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn authorize_ssh_keys(&self, pod: &Pod, keys: &[String]) -> CoreResult<()> {
            if !self.attaches {
                return Err(Error::NotImplemented("no key API".into()));
            }
            self.events.lock().unwrap().push(format!("authorize {} {}", pod.id, keys.len()));
            Ok(())
        }
        fn ready_deadline(&self, _pod: &Pod) -> Option<Duration> {
            self.ready
        }
        fn lock_support(&self, _pod: &Pod) -> CoreResult<()> {
            if self.lockable {
                Ok(())
            } else {
                Err(Error::NotImplemented("no lock".into()))
            }
        }
        async fn set_locked(&self, id: &str, locked: bool) -> CoreResult<()> {
            if self.lock_refused.lock().unwrap().iter().any(|r| r == id) {
                return Err(Error::provider("lock pod HTTP 404 Not Found: pod not found"));
            }
            self.events.lock().unwrap().push(format!("lock {id} {locked}"));
            Ok(())
        }
        async fn terminate_pod(&self, id: &str) -> CoreResult<()> {
            self.events.lock().unwrap().push(format!("terminate {id}"));
            let now = Instant::now();
            let mut pods = self.pods.lock().unwrap();
            if let Some(s) = pods.iter_mut().find(|s| s.pod.id == id) {
                s.gone_at = Some(now + self.linger);
                if !self.dry_after_terminate.is_zero() {
                    *self.dry_until.lock().unwrap() = Some((s.pod.name.clone(), now + self.dry_after_terminate));
                }
            }
            if let Some(backend) = self.twin_after_terminate {
                let name = pods.iter().find(|s| s.pod.id == id).map(|s| s.pod.name.clone()).unwrap_or_default();
                let pod = Pod { id: "twin".into(), name, provider: backend.into(), status: "RUNNING".into(), ..Default::default() };
                if backend == "runpod" {
                    let never = now + Duration::from_secs(1 << 30);
                    pods.push(Slot { pod, from: never, ip: String::new(), port: 0, gone_at: None, later: Vec::new() });
                } else {
                    self.hetzner.lock().unwrap().push(pod);
                }
            }
            if !self.down_after_terminate.is_zero() {
                *self.down_until.lock().unwrap() = Some(now + self.down_after_terminate);
            }
            Ok(())
        }
        /// By the backend's name, as the fleet provider routes it: this fake's own pods by their
        /// tag; one named for a backend that doesn't list that id here never reaches them.
        async fn terminate_pod_on(&self, provider: &str, id: &str) -> CoreResult<()> {
            let ours = self.pods.lock().unwrap().iter().any(|s| s.pod.id == id && s.pod.provider == provider);
            if ours {
                return self.terminate_pod(id).await;
            }
            self.events.lock().unwrap().push(format!("terminate {provider} {id}"));
            Ok(())
        }
    }

    /// Every line, with when (since the test began) it was said.
    struct Lines {
        start: Instant,
        lines: Mutex<Vec<(Duration, String)>>,
    }

    impl Lines {
        fn new() -> Self {
            Lines { start: Instant::now(), lines: Mutex::new(Vec::new()) }
        }
        fn say(&self, _to: To, line: &str) {
            self.lines.lock().unwrap().push((self.start.elapsed(), line.to_string()));
        }
        fn all(&self) -> Vec<String> {
            self.lines.lock().unwrap().iter().map(|(_, l)| l.clone()).collect()
        }
        /// (index, time) of the first line starting with `prefix`.
        fn find(&self, prefix: &str) -> (usize, Duration) {
            let lines = self.lines.lock().unwrap();
            let i = lines.iter().position(|(_, l)| l.starts_with(prefix)).unwrap_or_else(|| panic!("no `{prefix}…` in {lines:#?}"));
            (i, lines[i].0)
        }
    }

    /// A temp dir of its own (the proxy lock is on the config's directory), removed on drop.
    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp_dir(tag: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!("arena-up-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    /// Setup + check settings, and (with `proxy`) a write-only proxy config — tests never
    /// reload nginx.
    fn cfg(proxy: Option<&Path>) -> Config {
        Config::parse(&cfg_text(proxy))
    }

    fn cfg_text(proxy: Option<&Path>) -> String {
        let mut text = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n  \"cloud\"\n)\n\
                        ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
                        SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nALLOWED_CUDA_VERSIONS=\"13.0\"\n\
                        GPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nVSCODE_PREINSTALL=0\nREPO_ON_VOLUME=0\n"
            .to_string();
        if let Some(p) = proxy {
            text.push_str(&format!(
                "SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\nSSH_PROXY_STARTING_PORT=9500\n",
                p.display()
            ));
        }
        text
    }

    /// A4000 then 3090, community, as listed (prices: the preset estimates).
    fn options() -> OptionPlan {
        let req = Request {
            gpus: vec![A4000.into(), R3090.into()],
            clouds: vec!["COMMUNITY".into()],
            gpu_count: 1,
            max_price: None,
            order: Order::Listed,
            near: None,
        };
        plan_options(&req, "runpod", &PriceBook::runpod(Vec::new()))
    }

    /// A keys dir with an OpenAI key for every test name.
    fn keys_dir(tag: &str) -> TmpDir {
        let d = tmp_dir(&format!("keys-{tag}"));
        std::fs::write(d.0.join("openai_api_keys.csv"), "host,key\ndevtest-apple,sk-a\ndevtest-bloom,sk-b\ndevtest-cloud,sk-c\n")
            .unwrap();
        d
    }

    async fn up_run<'a>(
        fleet: &'a Fleet,
        remote: Arc<dyn Remote>,
        cfg: &'a Config,
        setup: bool,
        check: Option<u32>,
        keys: Option<&Path>,
    ) -> UpRun<'a> {
        UpRun {
            provider: fleet,
            remote,
            cfg,
            options: options(),
            base: PodSpec::from_config(cfg),
            rounds: Rounds { window: Duration::ZERO, every: Duration::from_secs(60) },
            timeout: Duration::from_secs(600),
            interval: Duration::from_secs(10),
            proxy: crate::proxy_deployable(cfg).await,
            setup: setup.then(|| SetupStage {
                scfg: SetupConfig::from_config(cfg).unwrap(),
                timeouts: SetupTimeouts::default(),
                hetzner_script: String::new(),
            }),
            check: check.map(|attempts| CheckStage {
                policy: HealthPolicy::from_config(cfg).unwrap(),
                attempts,
                cmd: health::deep_check_command(Some("arena-env")),
            }),
            keys: keys.and_then(|d| crate::up_keys_stage(cfg, &d.to_string_lossy(), true)),
            claude: false,
            lock: false,
            retry_bad_hosts: false,
            balance: None,
            reap_notice: false,
            placements: check.unwrap_or(2),
            endpoint_timeout: NO_PUBLIC_IP_GRACE,
        }
    }

    /// The create phase: each name on the first option.
    async fn make(fleet: &Fleet, names: &[&str]) -> Vec<Made> {
        let plan = options();
        let mut made = Vec::new();
        for n in names {
            let spec = placement::spec_for(&PodSpec::from_config(&cfg(None)), &format!("devtest-{n}"), &plan.options[0]);
            let pod = fleet.create_pod(&spec).await.unwrap();
            made.push(Made { pod, at: Instant::now(), option: Some(0) });
        }
        made
    }

    fn host(ip: &str, port: u16) -> String {
        format!("{ip}:{port}")
    }

    /// Execs to `host` whose command contains `needle`.
    fn execs_with(fake: &FakeRemote, host: &str, needle: &str) -> usize {
        fake.calls_to(host).iter().filter(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains(needle))).count()
    }

    fn healthy() -> FakeReply {
        FakeReply::stdout(DEEP_HEALTHY)
    }

    fn bad_host() -> FakeReply {
        FakeReply::stdout(DEEP_CUINIT_999)
    }

    #[tokio::test(start_paused = true)]
    async fn each_pod_is_ready_on_its_own_and_a_late_endpoint_holds_no_one_up() {
        // apple's endpoint is there at once, bloom's only after 300s.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 300)])]);
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        let (apple_ready, apple_at) = lines.find("[devtest-apple] READY after ");
        let (bloom_endpoint, bloom_at) = lines.find("[devtest-bloom] endpoint 10.0.0.2:22002");
        assert!(apple_ready < bloom_endpoint, "apple's READY comes first: {:#?}", lines.all());
        assert!(apple_at < Duration::from_secs(300), "apple was ready at {apple_at:?}, before bloom had an endpoint");
        assert_eq!(bloom_at, Duration::from_secs(300));
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{rows:#?}");
        assert_eq!((rows[0].name.as_str(), rows[1].name.as_str()), ("devtest-apple", "devtest-bloom"), "rows keep the create order");
        assert!(rows[1].ready_after.unwrap() >= Duration::from_secs(300));
        assert_eq!(rows[0].gpu, "1×RTX A4000");
        assert_eq!(rows[0].price, "$0.17");
        // Each got its own setup (copy key + config), nothing else (no check, no keys).
        for h in [host("10.0.0.1", 22001), host("10.0.0.2", 22002)] {
            assert_eq!(fake.calls_to(&h).len(), 2, "{h}");
        }
        // One list per poll for the whole fleet — not one per waiting pod (that'd be ~60).
        let lists = fleet.lists.load(Ordering::SeqCst);
        assert!(lists <= 33, "{lists} list calls for 300s of polling every 10s");
        assert!(conclude(&rows, &|_, _| {}).is_ok());
    }

    /// `SSH_CONFIG_FILE` after `up`: written from the final listing, with each pod's endpoint as
    /// it came up — a pod whose endpoint came late included — and said once.
    #[tokio::test(start_paused = true)]
    async fn up_writes_the_ssh_config_file_with_the_endpoints_the_pods_came_up_on() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 300)])]);
        let fake = Arc::new(FakeRemote::new());
        let dir = tmp_dir("sshconfig");
        let path = dir.0.join("arena.conf");
        let cfg = Config::parse(&format!("{}SSH_CONFIG_FILE={}\n", cfg_text(None), path.display()));
        let run = up_run(&fleet, fake.clone(), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{rows:#?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# managed by arena"), "{text}");
        for block in ["Host devtest-apple\n    HostName 10.0.0.1\n    Port 22001\n", "Host devtest-bloom\n    HostName 10.0.0.2\n    Port 22002\n"] {
            assert!(text.contains(block), "{block}: {text}");
        }
        let said: Vec<String> = lines.all().into_iter().filter(|l| l.contains("[ssh-config]")).collect();
        assert_eq!(said, [format!("[ssh-config] after up: rewrote {}", path.display())]);
    }

    /// With the VS Code warm-up on (the default outside these tests): it runs after the
    /// required setup steps and before the check; a warm-up that fails is a warning line,
    /// and the pod is checked and READY as usual.
    #[tokio::test(start_paused = true)]
    async fn a_failed_vscode_warm_up_is_a_warning_and_the_pod_is_still_ready() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(
            &host("10.0.0.1", 22001),
            [
                FakeReply::ok(),
                FakeReply::ok(),
                FakeReply::exit(3, "vscode warm-up incomplete: update API for server-linux-x64: curl: (6) Could not resolve host"),
                healthy(),
            ],
        );
        let cfg = Config::parse(&cfg_text(None).replace("VSCODE_PREINSTALL=0\n", ""));
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        let (ok, _) = lines.find("[devtest-apple] setup ✓");
        let (warned, _) = lines.find(
            "[devtest-apple] setup warning: vscode warm-up: exit 3: vscode warm-up incomplete: update API for \
             server-linux-x64: curl: (6) Could not resolve host",
        );
        assert!(ok < warned, "{:#?}", lines.all());
        assert_eq!(execs_with(&fake, &host("10.0.0.1", 22001), "arena-vscode-warmup"), 1);
        assert_eq!(fleet.events().len(), 1, "never replaced: {:?}", fleet.events());
    }

    /// Live (hz F4): `up` on a Hetzner VM said only `setup ✓ (5m44s)` — which installer built the
    /// env was in the script's output alone. Now what the setup says built it (its
    /// `arena-installer:` line, from whichever step prints it) is on the ✓ line; a setup that
    /// says nothing keeps the line as it was.
    #[tokio::test(start_paused = true)]
    async fn the_setup_line_names_the_installer_the_setup_said() {
        for (said, want) in [
            ("### 4/6 ARENA repo + Python env\narena-installer: install_uv.sh\n### done — Python 3.13.16\n", "(0s, installer: install_uv.sh)"),
            ("arena-installer: none (env already present)\r\n", "(0s, installer: none (env already present))"),
            ("### done — nothing said\n", "(0s)"),
            (
                "arena-image: devtest-image-261029-1556, built 2026-10-29 (0 h ago), commit 3eed962b9bd8; requirements unchanged, env kept\n\
                 arena-installer: none (env already present)\n",
                "(0s, installer: none (env already present), image: devtest-image-261029-1556, built 2026-10-29 (0 h ago), commit \
                 3eed962b9bd8; requirements unchanged, env kept)",
            ),
        ] {
            let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::stdout(said), healthy()]);
            let cfg = cfg(None);
            let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
            let made = make(&fleet, &["apple"]).await;
            let lines = Lines::new();
            let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
            assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
            let (i, _) = lines.find("[devtest-apple] setup ✓");
            assert_eq!(lines.all()[i], format!("[devtest-apple] setup ✓ {want}"));
        }
    }

    /// [`cfg`] with a readable cohort key (its `.pub` beside it) and a deploy key that has
    /// none — so exactly one public key is there to re-attach.
    fn cfg_with_pubkey(dir: &Path) -> Config {
        std::fs::write(dir.join("devtest_key"), "not a real key").unwrap();
        std::fs::write(dir.join("devtest_key.pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbc devtest\n").unwrap();
        let text = cfg_text(None)
            .replace("/nonexistent/devtest_key", &dir.join("devtest_key").to_string_lossy())
            .replace("/nonexistent/devtest_deploy_key", &dir.join("no-such-deploy-key-for-up-tests").to_string_lossy());
        Config::parse(&text)
    }

    const DENIED: &str = "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n";

    /// A pod that refuses our key (a Vast instance whose key attach hadn't landed): its
    /// provider re-attaches the cohort key, and setup runs again until the pod takes it.
    #[tokio::test(start_paused = true)]
    async fn a_pod_refusing_our_key_gets_it_reattached_and_setup_reruns() {
        let fleet = Fleet { attaches: true, ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]) };
        let fake = Arc::new(FakeRemote::new());
        // Refused at first, and still 20s after the re-attach; accepted at 40s.
        fake.script(&host("10.0.0.1", 22001), [FakeReply::exit(255, DENIED), FakeReply::exit(255, DENIED)]);
        let dir = tmp_dir("key-repair");
        let cfg = cfg_with_pubkey(&dir.0);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert!(fleet.events().contains(&"authorize id1 1".to_string()), "{:?}", fleet.events());
        let (said, at) = lines.find("[devtest-apple] setup: the pod refused our SSH key — re-attached 1 key(s)");
        assert_eq!(at, Duration::ZERO);
        let (ready, ready_at) = lines.find("[devtest-apple] READY after ");
        assert!(said < ready && ready_at >= Duration::from_secs(40), "{:#?}", lines.all());
        // Refused copy, refused copy (20s), then copy + config (40s).
        assert_eq!(fake.calls_to(&host("10.0.0.1", 22001)).len(), 4);
    }

    /// A provider without a key API (RunPod): the refusal is the setup failure, as before —
    /// reported at once, not retried.
    #[tokio::test(start_paused = true)]
    async fn a_refused_key_without_a_key_api_fails_setup_as_before() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::exit(255, DENIED)]);
        let dir = tmp_dir("key-repair-none");
        let cfg = cfg_with_pubkey(&dir.0);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Setup, reason } if reason.contains("Permission denied (publickey)")), "{:?}", rows[0].verdict);
        assert!(lines.all().iter().all(|l| !l.contains("re-attach")), "{:#?}", lines.all());
        assert_eq!(fake.calls_to(&host("10.0.0.1", 22001)).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_timeout_fails_that_pod_alone_and_keys_go_only_to_ready_pods() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.1", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22002), [FakeReply::ok(), FakeReply::hang()]); // config step wedges
        let keys = keys_dir("setup-timeout");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, Some(&keys.0)).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        let (apple_ready, apple_at) = lines.find("[devtest-apple] READY after ");
        let (bloom_failed, bloom_at) =
            lines.find("[devtest-bloom] FAILED setup: timed out at repo + keys config after 300s — left running");
        assert!(apple_ready < bloom_failed && apple_at < bloom_at, "{:#?}", lines.all());
        assert_eq!(bloom_at, Duration::from_secs(300), "bloom fails at its step budget");
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert!(matches!(&rows[1].verdict, Verdict::Failed { stage: Stage::Setup, .. }), "{:?}", rows[1].verdict);
        // Keys: apple (READY) got its write, bounded; bloom (failed) was never contacted again.
        let apple = fake.calls_to(&host("10.0.0.1", 22001));
        assert!(
            matches!(&apple[..], [RemoteCall::Copy { .. }, RemoteCall::Exec { .. }, RemoteCall::Exec { cmd, timeout, .. }]
                if cmd.contains("OPENAI_API_KEY") && *timeout == Some(COPY_KEYS_TIMEOUT)),
            "{apple:?}"
        );
        assert_eq!(fake.calls_to(&host("10.0.0.1", 22002)).len(), 2, "copy + the wedged config step, no keys");
        assert!(fleet.events().iter().all(|e| !e.starts_with("terminate")), "a failed pod is left running");
        let err = conclude(&rows, &|_, _| {}).unwrap_err().to_string();
        assert_eq!(err, "1 of 2 pod(s) not ready: devtest-bloom (FAILED setup)");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_deep_check_is_terminated_and_recreated_on_the_next_option() {
        // apple lands on a cuInit-999 host; its replacement (30s to an endpoint) is healthy.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 30)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let keys = keys_dir("replace");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), Some(&keys.0)).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        // Terminated (and gone) before the replacement exists; the replacement tries the next option.
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        let (fail, _) = lines.find("[devtest-apple] check: FAIL (host) — cuda: RuntimeError: Unexpected error from cudaGetDeviceCount()");
        let (term, _) = lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        let (created, _) = lines.find("[created] devtest-apple id=id2 on 1×RTX 3090 COMMUNITY (~$0.22/h)");
        let (ready, _) = lines.find("[devtest-apple] READY after 30s — 1×RTX 3090 COMMUNITY, check pass");
        assert!(fail < term && term < created && created < ready, "{:#?}", lines.all());
        let row = &rows[0];
        assert_eq!(row.verdict, Verdict::Ready);
        assert_eq!(row.health, Some(Status::Pass));
        assert_eq!(row.attempts.len(), 2);
        assert!(matches!(&row.attempts[0].end, AttemptEnd::CheckFailed(n) if n.contains("Error 999")), "{:?}", row.attempts[0]);
        assert!(row.attempts[0].terminated);
        assert_eq!((row.attempts[1].option.as_str(), row.attempts[1].end.clone()), ("1×RTX 3090 COMMUNITY", AttemptEnd::Kept));
        // The bad pod never got keys; the good one did, after its own setup + check.
        assert_eq!(execs_with(&fake, &host("10.0.0.1", 22001), "OPENAI_API_KEY"), 0);
        assert_eq!(execs_with(&fake, &host("10.0.0.2", 22002), "OPENAI_API_KEY"), 1);
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("devtest-apple: #1 1×RTX A4000 COMMUNITY (id1, 10.0.0.1): check FAIL — cuda: "), "{summary}");
        assert!(summary.contains("; terminated → #2 1×RTX 3090 COMMUNITY (id2, 10.0.0.2): ready"), "{summary}");
    }

    /// A FAIL that is all software (here torch doesn't import — live: a Hetzner VM whose
    /// install had failed) is never replaced: a new pod boots the same image and runs the same
    /// setup. The name FAILs at once, saying so; the pod is left running, never terminated (and
    /// its machine isn't counted as bad: another name's replacement may land there).
    #[tokio::test(start_paused = true)]
    async fn a_software_failure_is_not_replaced_and_says_why() {
        let torch_missing = DEEP_HEALTHY
            .lines()
            .filter(|l| !["torch_", "cuda_", "device_count", "tensor.", "peer.", "nccl"].iter().any(|p| l.starts_with(p)))
            .map(|l| if l == "torch=ok" { "torch=error: ModuleNotFoundError: No module named 'torch'".to_string() } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n");
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 30)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(&torch_missing)]);
        let keys = keys_dir("software");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), Some(&keys.0)).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "never terminated nor recreated");
        lines.find("[devtest-apple] check: FAIL (software/setup, not the host) — torch: ModuleNotFoundError: No module named 'torch'");
        assert!(!lines.all().iter().any(|l| l.contains("terminating")), "{:#?}", lines.all());
        let row = &rows[0];
        match &row.verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.starts_with("software/setup failure, not the host — torch: ModuleNotFoundError")
                    && reason.contains("Not replaced: a new pod from the same image fails the same way")
                    && reason.contains("Left running"),
                "{reason}"
            ),
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert_eq!(row.attempts.len(), 1);
        assert_eq!((row.attempts[0].end.clone(), row.attempts[0].terminated), (AttemptEnd::Kept, false));
        // No replacement was ever placed (nothing ran on the second endpoint), and the pod got
        // no keys (it isn't ready).
        assert!(fake.calls_to(&host("10.0.0.2", 22002)).is_empty());
        assert_eq!(execs_with(&fake, &host("10.0.0.1", 22001), "OPENAI_API_KEY"), 0);
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("devtest-apple: FAILED check — software/setup failure, not the host — torch: "), "{summary}");
        assert!(conclude(&rows, &|_, _| {}).is_err());
    }

    /// python killed by its 75s timeout before `import torch` could report (a hung or overloaded
    /// host as likely as the image; OOM kills and SIGILL on an old CPU look the same): a host
    /// failure, so the pod is replaced — it used to count as software (torch FAILed) and was left
    /// running, billed, with "replacing it won't help".
    #[tokio::test(start_paused = true)]
    async fn python_dying_before_torch_reports_is_a_host_failure_and_replaced() {
        let timed_out = DEEP_HEALTHY
            .lines()
            .filter(|l| !["torch", "cuda_", "device_count", "tensor.", "peer.", "nccl", "py_"].iter().any(|p| l.starts_with(p)))
            .map(|l| if l == "deep_check_end=1" { "py_exit=124\ndeep_check_end=1".to_string() } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n");
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 30)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(&timed_out)]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let keys = keys_dir("pydied");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), Some(&keys.0)).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        lines.find("[devtest-apple] check: FAIL (host) — torch: python checks timed out");
        assert!(!lines.all().iter().any(|l| l.contains("software/setup")), "{:#?}", lines.all());
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!(rows[0].attempts.len(), 2);
    }

    /// `up --check` records the standing pod's check for `arena snapshot`; the pod it
    /// terminated for its FAIL leaves no record behind.
    #[tokio::test(start_paused = true)]
    async fn up_check_records_the_standing_pods_health_for_snapshot() {
        use arena_core::snapshot::HealthCache;
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 30)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let state = tmp_dir("health-cache");
        let mut cfg = cfg(None);
        cfg.values.insert("ARENA_STATE_DIR".into(), state.0.display().to_string());
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);

        let (cache, warning) = HealthCache::load(&state.0.join("devtest").join("health.json"));
        assert_eq!(warning, None);
        let ids: Vec<&str> = cache.pods.values().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["id2"], "{cache:#?}");
        assert_eq!(cache.pods.values().next().unwrap().status, Status::Pass);
        assert!(!lines.all().iter().any(|l| l.contains("health cache")), "{:#?}", lines.all());
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_on_the_failed_machine_is_rejected_and_retried() {
        // #1 fails on 10.0.0.1; #2 lands on 10.0.0.1 again (another port, same machine);
        // #3 lands elsewhere and is healthy.
        let fleet =
            Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.1", 22002, 0), ("10.0.0.3", 22003, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"), // the option that hasn't failed
                "terminate id2".to_string(),             // same machine: rejected unseen
                format!("create devtest-apple {A4000}"), // both failed → back in the confirmed order
            ]
        );
        lines.find("[devtest-apple] landed on 10.0.0.1, a machine that already failed the deep check — rejecting it");
        assert!(fake.calls_to(&host("10.0.0.1", 22002)).is_empty(), "the rejected pod is never set up or checked");
        let row = &rows[0];
        assert_eq!(row.verdict, Verdict::Ready);
        let ends: Vec<(AttemptEnd, bool)> = row.attempts.iter().map(|a| (a.end.clone(), a.terminated)).collect();
        assert!(matches!(&ends[..], [(AttemptEnd::CheckFailed(_), true), (AttemptEnd::SameHost, true), (AttemptEnd::Kept, false)]), "{ends:?}");
        assert_eq!(row.attempts[1].host.as_deref(), Some("10.0.0.1"));
    }

    /// Vast: the machine id, not the IP, says which machine a replacement landed on. A host
    /// often runs several machines behind one public IP — a replacement there on another
    /// machine is kept and checked; one back on the failed machine, whatever its IP, is not.
    #[tokio::test(start_paused = true)]
    async fn on_vast_the_machine_id_not_the_ip_names_the_failed_machine() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.1", 22002, 0)])]);
        fleet.machines.lock().unwrap().extend(["100".to_string(), "200".to_string()]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.1", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")],
            "the same IP on another machine is no reason to reject"
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        let hosts: Vec<Option<&str>> = rows[0].attempts.iter().map(|a| a.host.as_deref()).collect();
        assert_eq!(hosts, [Some("vast machine 100"), Some("vast machine 200")]);
        assert!(!fake.calls_to(&host("10.0.0.1", 22002)).is_empty(), "set up and checked");

        // Back on machine 100 behind another IP: rejected unseen.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.9", 22009, 0), ("10.0.0.3", 22003, 0)])]);
        fleet.machines.lock().unwrap().extend(["100", "100", "300"].map(String::from));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(fleet.events()[3], "terminate id2");
        lines.find("[devtest-apple] landed on vast machine 100, a machine that already failed the deep check — rejecting it");
        assert!(fake.calls_to(&host("10.0.0.9", 22009)).is_empty(), "the rejected pod is never set up or checked");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// Finding #23: several RunPod hosts share one public IP (live: alpha and bravo on
    /// 87.197.146.56 with different drivers), so the IP alone rejected a healthy replacement.
    /// The provider's machine id — from the details where the listing has none (REST v2) —
    /// names the machine first.
    #[tokio::test(start_paused = true)]
    async fn on_runpod_the_machine_id_from_the_details_names_the_failed_machine() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.1", 22002, 0)])]);
        fleet.details_machines.lock().unwrap().extend([(22001, "m1".to_string()), (22002, "m2".to_string())]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.1", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")],
            "the same IP on another RunPod machine is no reason to reject"
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        let hosts: Vec<Option<&str>> = rows[0].attempts.iter().map(|a| a.host.as_deref()).collect();
        assert_eq!(hosts, [Some("runpod machine m1"), Some("runpod machine m2")]);

        // Back on machine m1 behind another IP: rejected unseen.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.9", 22009, 0), ("10.0.0.3", 22003, 0)])]);
        fleet.details_machines.lock().unwrap().extend([(22001, "m1"), (22009, "m1"), (22003, "m3")].map(|(p, m)| (p, m.to_string())));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        lines.find("[devtest-apple] landed on runpod machine m1, a machine that already failed the deep check — rejecting it");
        assert!(fake.calls_to(&host("10.0.0.9", 22009)).is_empty(), "the rejected pod is never set up or checked");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// Live BPF S6: `up`'s summary called a B300 MIG slice `1×B300 SXM6 AC MIG 1g.34gb` (REST
    /// v2 lists the catalog id) where `pods list` says `1×B300 MIG 34GB` (the details' display
    /// name). The check now labels the slice by the details' name too.
    #[tokio::test(start_paused = true)]
    async fn a_mig_slice_is_labelled_in_the_summary_as_pods_list_labels_it() {
        const MIG: &str = "NVIDIA B300 SXM6 AC MIG 1g.34gb";
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        fleet.details_gpus.lock().unwrap().insert(22001, "B300 MIG 34GB".into());
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let spec = PodSpec { name: "devtest-apple".into(), gpu_type: MIG.into(), ..PodSpec::from_config(&cfg) };
        let pod = fleet.create_pod(&spec).await.unwrap();
        let made = vec![Made { pod, at: Instant::now(), option: Some(0) }];
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);
        // (The scripted check sees two devices.)
        assert_eq!(rows[0].gpu, "2×B300 MIG 34GB");
    }

    /// A state dir for the health cache, `cfg` pointed at it, and the cache's path.
    fn with_state(tag: &str, cfg: &mut Config) -> (TmpDir, PathBuf) {
        let state = tmp_dir(tag);
        cfg.values.insert("ARENA_STATE_DIR".into(), state.0.display().to_string());
        let path = state.0.join("devtest").join("health.json");
        (state, path)
    }

    /// Seed the health cache: machine `m` failed the deep check `ago` seconds before now.
    fn remember(path: &Path, m: &str, ago: u64) {
        let bad = BadHost {
            failed_at: snapshot::unix_now() - ago,
            pod: "devtest-bravo".into(),
            reason: "cuda: RuntimeError: CUDA unknown error".into(),
        };
        snapshot::record_bad_host(path, &format!("runpod machine {m}"), &bad, snapshot::unix_now()).unwrap();
    }

    /// Item 13 (live BPF S2): a machine an earlier `up --check` failed on its host (RunPod
    /// `ue6mmk7dp8qi`: 11 pods across runs, each set up and checked first) is turned away on
    /// the FIRST attempt too — before setup, through the same replace path as a replacement on
    /// a failed machine — saying when and why it was found bad.
    #[tokio::test(start_paused = true)]
    async fn a_machine_remembered_as_bad_is_rejected_on_the_first_attempt_before_setup() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])]);
        fleet.details_machines.lock().unwrap().extend([(22001, "mbad".to_string()), (22002, "mgood".to_string())]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let mut cfg = cfg(None);
        let (_state, path) = with_state("known-bad", &mut cfg);
        remember(&path, "mbad", 5 * 3600);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert!(fake.calls_to(&host("10.0.0.1", 22001)).is_empty(), "the rejected pod is never set up or checked");
        lines.find(
            "[devtest-apple] landed on runpod machine mbad, which failed the deep check 5h ago (20", // …the UTC time
        );
        let line = lines.all().into_iter().find(|l| l.contains("landed on runpod machine mbad")).unwrap();
        assert!(
            line.ends_with(", devtest-bravo: cuda: RuntimeError: CUDA unknown error) — rejecting it (`--retry-bad-hosts` tries such machines anyway)"),
            "{line}"
        );
        let row = &rows[0];
        assert_eq!(row.verdict, Verdict::Ready, "{:#?}", lines.all());
        let ends: Vec<(AttemptEnd, bool)> = row.attempts.iter().map(|a| (a.end.clone(), a.terminated)).collect();
        assert!(matches!(&ends[..], [(AttemptEnd::KnownBad(w), true), (AttemptEnd::Kept, false)] if w.starts_with("5h ago")), "{ends:?}");
        assert!(pipeline::render_up_summary(&rows).contains("on a machine that failed a deep check 5h ago"));

        // With one attempt only, it's still terminated (never set up; its machine known bad):
        // like a replacement rejected on its last attempt.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        fleet.details_machines.lock().unwrap().insert(22001, "mbad".into());
        let fake = Arc::new(FakeRemote::new());
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(1), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()]);
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.starts_with("attempt 1 landed on runpod machine mbad, which failed the deep check 5h ago")
                    && reason.contains("terminated it — devtest-apple has no pod now"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Without --check the remembered machine is only said: nothing terminates a pod nobody
    /// asked to have checked (as a replacement — which only --check makes — is the only pod
    /// today's in-run rejection ever turns away). Expired, it isn't even said; a cache that
    /// can't be read is a warning and remembers nothing — never a reason to fail a create.
    #[tokio::test(start_paused = true)]
    async fn without_check_a_remembered_machine_is_a_warning_and_expired_or_unreadable_ones_are_nothing() {
        let run_one = |cfg: Config, check: Option<u32>| async move {
            let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
            fleet.details_machines.lock().unwrap().insert(22001, "mbad".into());
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
            let run = up_run(&fleet, fake.clone(), &cfg, true, check, None).await;
            let made = make(&fleet, &["apple"]).await;
            let lines = Lines::new();
            let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
            (fleet.events(), rows[0].verdict.clone(), lines.all())
        };
        let mut cfg = cfg(None);
        let (_state, path) = with_state("known-bad-nocheck", &mut cfg);
        remember(&path, "mbad", 3600);
        let (events, verdict, lines) = run_one(cfg.clone(), None).await;
        assert_eq!(events, [format!("create devtest-apple {A4000}")], "nothing terminated");
        assert_eq!(verdict, Verdict::Ready);
        let warned: Vec<&String> = lines.iter().filter(|l| l.contains("landed on runpod machine mbad")).collect();
        assert!(
            matches!(&warned[..], [l] if l.starts_with("[devtest-apple] ⚠ landed on runpod machine mbad, which failed the deep check 1h ago")
                && l.ends_with("— kept: without --check nothing is checked or replaced (`arena pods test --deep devtest-apple`)")),
            "{lines:#?}"
        );

        // Expired (past the TTL): forgotten — checked and READY on that machine, under --check.
        std::fs::remove_file(&path).unwrap();
        remember(&path, "mbad", snapshot::BAD_HOST_TTL_SECS + 60);
        let (events, verdict, lines) = run_one(cfg.clone(), Some(2)).await;
        assert_eq!((events.len(), verdict), (1, Verdict::Ready), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("mbad")), "{lines:#?}");

        // Corrupt: one warning, nothing remembered, the run goes on.
        std::fs::write(&path, "{not json").unwrap();
        let (events, verdict, lines) = run_one(cfg, Some(2)).await;
        assert_eq!((events.len(), verdict), (1, Verdict::Ready), "{lines:#?}");
        assert!(
            lines[0].starts_with("warning: ignoring the health cache") && lines[0].ends_with("— no machine is remembered as bad this run"),
            "{lines:#?}"
        );
    }

    /// A host FAIL is remembered at once — for the rest of the run (another name's FIRST pod
    /// on that machine is turned away; it used to be set up and checked) and in the health
    /// cache for the next runs, by machine id with when/why. A FAIL on a machine only an IP
    /// names is never written down (IPs are shared, or reassigned).
    #[tokio::test(start_paused = true)]
    async fn a_host_fail_is_remembered_for_this_run_and_the_next() {
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)]),
            // bloom's first pod lands on apple's bad machine, after apple's check.
            ("bloom", &[("10.0.0.1", 22003, 120), ("10.0.0.4", 22004, 0)]),
            // cloud fails on a machine nothing but its IP names.
            ("cloud", &[("10.0.0.5", 22005, 0), ("10.0.0.6", 22006, 0)]),
        ]);
        fleet.details_machines.lock().unwrap().extend(
            [(22001, "mbad"), (22002, "m2"), (22003, "mbad"), (22004, "m4")].map(|(p, m)| (p, m.to_string())),
        );
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.5", 22005), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        for (ip, port) in [("10.0.0.2", 22002), ("10.0.0.4", 22004), ("10.0.0.6", 22006)] {
            fake.script(&host(ip, port), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        }
        let mut cfg = cfg(None);
        let (_state, path) = with_state("remembers", &mut cfg);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple", "bloom", "cloud"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{:#?}", lines.all());
        let (i, _) = lines.find("[devtest-bloom] landed on runpod machine mbad, which failed the deep check ");
        assert!(lines.all()[i].contains(", devtest-apple: cuda: ") && lines.all()[i].contains("— rejecting it"), "{:#?}", lines.all());
        assert!(fake.calls_to(&host("10.0.0.1", 22003)).is_empty(), "bloom's pod on the bad machine is never set up");
        assert!(matches!(rows[1].attempts[0].end, AttemptEnd::KnownBad(_)), "{:?}", rows[1].attempts);

        let (cache, warning) = snapshot::HealthCache::load(&path);
        assert_eq!(warning, None);
        let keys: Vec<&str> = cache.bad_hosts.keys().map(String::as_str).collect();
        assert_eq!(keys, ["runpod machine mbad"], "cloud's IP-only machine isn't remembered: {cache:#?}");
        let bad = &cache.bad_hosts["runpod machine mbad"];
        assert_eq!(bad.pod, "devtest-apple");
        assert!(bad.reason.starts_with("cuda: ") && bad.live(snapshot::unix_now()), "{bad:?}");
        assert_eq!(cache.pods.len(), 3, "the standing pods' verdicts are kept beside it");
    }

    /// Merge of wave 7 balance onto lane G: a replacement — for a pod that FAILed its check
    /// on its host, or one turned away on a machine remembered as bad — remembers the price
    /// its create answered with in the balance state, the moment it's made (the first pods
    /// are the create phase's, through `OnMade`). The two state files share one directory and
    /// its lock: G's bad-host and verdict writes and the balance's price writes interleave
    /// here, and neither file loses the other's records.
    #[tokio::test(start_paused = true)]
    async fn replacements_remember_their_price_beside_the_remembered_bad_host() {
        use arena_core::balance::{self, Account, FakeBalanceApi};
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)]),
            // bloom's first pod lands on apple's bad machine, after apple's check.
            ("bloom", &[("10.0.0.1", 22003, 120), ("10.0.0.4", 22004, 0)]),
        ]);
        fleet.details_machines.lock().unwrap().extend([(22001, "mbad"), (22002, "m2"), (22003, "mbad"), (22004, "m4")].map(|(p, m)| (p, m.to_string())));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        for (ip, port) in [("10.0.0.2", 22002), ("10.0.0.4", 22004)] {
            fake.script(&host(ip, port), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        }
        let mut cfg = cfg(None);
        let (state, health) = with_state("w7-prices", &mut cfg);
        let prices = state.0.join("devtest").join(snapshot::BALANCE_FILE);
        let bal = crate::money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().with(Account::Runpod)), path: Some(Ok(prices.clone())) };
        let run = UpRun { balance: Some(&bal), ..up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await };
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{:#?}", lines.all());
        assert!(matches!(rows[1].attempts[0].end, AttemptEnd::KnownBad(_)), "{:?}", rows[1].attempts);

        // The pods standing now are the two replacements: each remembered at its create's price.
        let mut standing: Vec<String> = fleet.listed().into_iter().map(|p| p.id).collect();
        standing.sort();
        assert_eq!(standing.len(), 2);
        let (saved, warning) = balance::load(&prices);
        assert_eq!(warning, None);
        let remembered: Vec<(&String, f64)> = saved.accounts[&Account::Runpod].prices.iter().map(|(id, (usd, _))| (id, *usd)).collect();
        assert_eq!(remembered, standing.iter().map(|id| (id, 0.17)).collect::<Vec<_>>());
        // …and G's records beside them: the bad machine, and each name's verdict.
        let (cache, warning) = snapshot::HealthCache::load(&health);
        assert_eq!(warning, None);
        assert_eq!(cache.bad_hosts.keys().collect::<Vec<_>>(), ["runpod machine mbad"]);
        assert_eq!(cache.pods.len(), 2);
    }

    /// Review G1: a check that timed out (python's 75 s here) is a host FAIL — the pod is
    /// replaced in the run that saw it — but it is no evidence against the machine: not
    /// remembered for later runs, nor used to turn another name's first pod away.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_check_replaces_the_pod_but_is_not_remembered() {
        let timed_out = DEEP_HEALTHY
            .lines()
            .filter(|l| !["torch", "cuda_", "device_count", "tensor.", "peer.", "nccl", "py_"].iter().any(|p| l.starts_with(p)))
            .map(|l| if l == "deep_check_end=1" { "py_exit=124\ndeep_check_end=1".to_string() } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n");
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)]),
            ("bloom", &[("10.0.0.1", 22003, 120)]), // later, on the machine apple timed out on
        ]);
        fleet.details_machines.lock().unwrap().extend([(22001, "mslow"), (22002, "m2"), (22003, "mslow")].map(|(p, m)| (p, m.to_string())));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(&timed_out)]);
        for (ip, port) in [("10.0.0.2", 22002), ("10.0.0.1", 22003)] {
            fake.script(&host(ip, port), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        }
        let mut cfg = cfg(None);
        let (_state, path) = with_state("timeout-not-remembered", &mut cfg);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{:#?}", lines.all());
        assert_eq!(rows[0].attempts.len(), 2, "apple's pod was replaced");
        assert_eq!(rows[1].attempts.len(), 1, "bloom's first pod on that machine was checked, not rejected");
        let (cache, _) = snapshot::HealthCache::load(&path);
        assert!(cache.bad_hosts.is_empty(), "{cache:#?}");
    }

    /// Review G1: `--retry-bad-hosts` ignores the memory for the run — the pod on a
    /// remembered machine is set up and checked — and a check that passes there forgets it.
    #[tokio::test(start_paused = true)]
    async fn retry_bad_hosts_checks_a_remembered_machine_and_a_pass_forgets_it() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        fleet.details_machines.lock().unwrap().insert(22001, "mbad".into());
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let mut cfg = cfg(None);
        let (_state, path) = with_state("retry-bad-hosts", &mut cfg);
        remember(&path, "mbad", 3600);
        remember(&path, "mother", 3600);
        let run = UpRun { retry_bad_hosts: true, ..up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await };
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "not rejected");
        assert!(!lines.all().iter().any(|l| l.contains("mbad")), "{:#?}", lines.all());
        let keys: Vec<String> = snapshot::HealthCache::load(&path).0.bad_hosts.into_keys().collect();
        assert_eq!(keys, ["runpod machine mother"], "the machine that passed is forgotten, the other kept");
    }

    /// Review G2: on Vast a create is told the machines known bad — so a replacement isn't
    /// rented right back on the one that just failed (Vast rents the cheapest offer, and the
    /// bad machine is often that). The first create gets the remembered ones (`up` puts them
    /// on the spec it creates from).
    #[tokio::test(start_paused = true)]
    async fn on_vast_a_replacement_is_told_to_avoid_the_machines_known_bad() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])]);
        fleet.machines.lock().unwrap().extend(["100".to_string(), "200".to_string()]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let mut run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        // What `up` puts on its base spec from the remembered machines (here: 7).
        run.base = crate::spec_with_overrides(&cfg, &crate::SpecOverrides { avoid_machines: vec!["7".into()], ..Default::default() });
        assert_eq!(run.base.avoid_machines, ["7"]);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;

        assert_eq!(rows[0].verdict, Verdict::Ready);
        let avoids = fleet.avoids.lock().unwrap().clone();
        assert_eq!(avoids, [vec![], vec!["7".to_string(), "100".to_string()]], "the replacement avoids the remembered and the just-failed machine");
    }

    /// Live (wave 6, and again 2026-10-30): community hosts came up RUNNING with a runtime but no
    /// direct SSH — only RunPod's proxy — for 15+ minutes. After `--endpoint-timeout` that's a
    /// host failure, replaced with or without --check (ops feedback 2026-10-29 #1: without it the
    /// pod was left billing until someone terminated it by hand), within the name's placements.
    #[tokio::test(start_paused = true)]
    async fn a_pod_up_behind_the_ssh_proxy_only_is_a_host_failure() {
        // Without --check: replaced once the 5 minutes are over — not at the 600s endpoint
        // timeout — on another option first; the new pod is READY.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s");
        assert!(at >= NO_PUBLIC_IP_GRACE && at < Duration::from_secs(330), "{at:?}");
        lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        assert_eq!((rows[0].attempts[0].end.clone(), rows[0].attempts[0].terminated), (AttemptEnd::NoEndpoint(PROXY_ONLY_END.into()), true));
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("no public SSH endpoint (RunPod ssh proxy only); terminated"), "{summary}");
        assert!(fake.calls_to(&host("10.0.0.2", 22002)).len() == 2, "only the replacement was set up");

        // With --check: the same, and the new pod checked.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!((rows[0].verdict.clone(), rows[0].health), (Verdict::Ready, Some(Status::Pass)));

        // No attempt left (--check-attempts 1): terminated all the same — kept, it would only
        // bill — and the name fails at `endpoint` saying nothing of it is left.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, Some(1), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()]);
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert!(
                reason.ends_with(
                    "— no attempts left (--check-attempts 1); terminated it — devtest-apple has no pod now, nothing of it \
                     left billing"
                ),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(fleet.listed().is_empty(), "nothing of apple's left");
    }

    /// Ops feedback #1 end to end, plain `up`: a pod that never gets an endpoint and its
    /// replacement that doesn't either — both terminated (the last with no attempt left), the
    /// name FAILED at `endpoint` saying nothing of it is left billing, both attempts in the
    /// summary, a non-zero exit. The bound without --check is --check-attempts' default (2).
    #[tokio::test(start_paused = true)]
    async fn a_replacement_without_an_endpoint_either_leaves_nothing_billing() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("proxy", 0, 5)])]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        assert_eq!(run.placements, 2);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                "terminate id2".to_string(),
            ]
        );
        assert!(fleet.listed().is_empty(), "nothing of apple's left");
        lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        lines.find("[devtest-apple] terminating id2 — no attempt left to replace it (--check-attempts 2)");
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert!(
                reason.starts_with("no public endpoint (RunPod ssh proxy only) for 5m00s")
                    && reason.ends_with(
                        "— no attempts left (--check-attempts 2); terminated it — devtest-apple has no pod now, nothing \
                         of it left billing"
                    ),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        let summary = pipeline::render_up_summary(&rows);
        assert!(
            summary.contains(
                "devtest-apple: #1 1×RTX A4000 COMMUNITY (id1): no public SSH endpoint (RunPod ssh proxy only); terminated \
                 → #2 1×RTX 3090 COMMUNITY (id2): no public SSH endpoint (RunPod ssh proxy only); terminated"
            ),
            "{summary}"
        );
        assert!(conclude(&rows, &|_, _| {}).is_err());
    }

    /// With --check, `--check-attempts` bounds it: three placements, none with an endpoint, each
    /// terminated — the options that haven't failed first (A4000, 3090, then A4000 again).
    #[tokio::test(start_paused = true)]
    async fn with_check_the_attempts_bound_how_often_a_pod_without_an_endpoint_is_replaced() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("proxy", 0, 5), ("proxy", 0, 5), ("10.0.0.4", 22004, 0)])]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, Some(3), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                "terminate id2".to_string(),
                format!("create devtest-apple {A4000}"),
                "terminate id3".to_string(),
            ]
        );
        assert_eq!(rows[0].attempts.len(), 3);
        assert!(rows[0].attempts.iter().all(|a| a.terminated && a.end == AttemptEnd::NoEndpoint(PROXY_ONLY_END.into())));
        assert!(
            matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason }
                if reason.ends_with("— no attempts left (--check-attempts 3); terminated it — devtest-apple has no pod now, nothing of it left billing")),
            "{:?}",
            rows[0].verdict
        );
    }

    /// Up without an endpoint for less than `--endpoint-timeout` is waited for: a RunPod pod
    /// behind the proxy only from minute 1 that gets its endpoint at minute 3, and a Vast
    /// instance listed running without one until minute 3 — each READY, nothing terminated.
    #[tokio::test(start_paused = true)]
    async fn an_endpoint_at_minute_3_is_waited_for() {
        let cfg = cfg(None);
        for (case, ip, tag) in [("runpod proxy only", "proxy", None), ("vast running", "bare", Some("vast"))] {
            let mut fleet = Fleet::new(&[("apple", &[(ip, 0, 60)])]);
            (fleet.tag, fleet.pending) = (tag, tag.map(|_| "LOADING"));
            fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("10.0.0.1".into(), 22001, Duration::from_secs(180))]);
            let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
            let made = make(&fleet, &["apple"]).await;
            let lines = Lines::new();
            let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
            assert_eq!(rows[0].verdict, Verdict::Ready, "{case}: {:#?}", lines.all());
            assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "{case}: nothing terminated");
            let (_, at) = lines.find("[devtest-apple] endpoint 10.0.0.1:22001");
            assert!(at >= Duration::from_secs(180) && at < Duration::from_secs(200), "{case}: {at:?}");
        }
    }

    /// Why RunPod's clock starts only when its container is up (live 2026-10-30): RunPod lists a
    /// pod RUNNING from its create while its image pulls — 6½ minutes with `runtime: null`, live —
    /// so a pull longer than `--endpoint-timeout` is no host failure. Endpoint at minute 7: READY,
    /// never replaced. Only `--timeout` bounds the pull.
    #[tokio::test(start_paused = true)]
    async fn a_runpod_image_pull_longer_than_the_endpoint_timeout_is_waited_out() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 420)])]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "never replaced");
        lines.find("[devtest-apple] endpoint 10.0.0.1:22001 (after 7m");
    }

    /// …but one still listed RUNNING with no container when `--timeout` ends, 10 minutes after
    /// its create ([`NO_CONTAINER_LIMIT`]), never started its image: a failed placement, with or
    /// without --check — prod 2026-10-29 (ops feedback #1): arena9-lightning, `FAILED endpoint: no
    /// SSH endpoint after 600s — left running`, billing until it was terminated by hand; its
    /// re-run was READY in a minute. Terminated and recreated, the other option first: READY.
    /// A replacement that never starts either is terminated with no attempt left, and nothing of
    /// the name is left billing.
    #[tokio::test(start_paused = true)]
    async fn a_runpod_pod_with_no_container_by_the_timeout_is_replaced() {
        const WHY: &str = "still listed RUNNING with no container 10m00s after its create (its image never started)";
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000), ("10.0.0.2", 22002, 60)])]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        let (_, at) = lines.find(&format!("[devtest-apple] {WHY} — before it came up: a failed placement"));
        assert_eq!(at, NO_CONTAINER_LIMIT);
        lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        assert_eq!((rows[0].attempts[0].end.clone(), rows[0].attempts[0].terminated), (AttemptEnd::Dead(WHY.into()), true));

        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000), ("10.0.0.2", 22002, 100_000)])]);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                "terminate id2".to_string(),
            ]
        );
        assert!(fleet.listed().is_empty(), "nothing of apple's left");
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert_eq!(
                reason,
                &format!(
                    "{WHY} — no attempts left (--check-attempts 2); terminated it — devtest-apple has no pod now, nothing of \
                     it left billing"
                )
            ),
            other => panic!("{other:?}"),
        }
    }

    /// A `--timeout` shorter than [`NO_CONTAINER_LIMIT`] is no verdict on the image: a RunPod pod
    /// with no container yet then is left running, as before — the reason says it bills, and how
    /// to stop it (review, codex r2: a healthy slow pull must not be killed by a short wait). Even
    /// when the deadline's look itself takes it past 10 minutes (review r6: `--timeout 590`, a
    /// 15 s listing).
    #[tokio::test(start_paused = true)]
    async fn a_runpod_pod_with_no_container_by_a_short_timeout_is_left_running_and_said_to_bill() {
        let cfg = cfg(None);
        for (timeout, look) in [(300, 0), (590, 15)] {
            let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)])]);
            fleet.sync_delay = Duration::from_secs(look);
            let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
            run.timeout = Duration::from_secs(timeout);
            let made = make(&fleet, &["apple"]).await;
            let started = Instant::now();
            let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
            assert!(started.elapsed() >= Duration::from_secs(timeout + look), "{timeout}: {:?}", started.elapsed());
            assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "{timeout}: nothing terminated");
            match &rows[0].verdict {
                Verdict::Failed { stage: Stage::Endpoint, reason } => assert_eq!(
                    reason,
                    &format!(
                        "no SSH endpoint after {timeout}s, still listed RUNNING (no container yet — its image may still be \
                         pulling) — left running, still billing ($0.17/h): `arena proxy apply` once it has an endpoint, or \
                         `arena pods terminate devtest-apple`"
                    )
                ),
                other => panic!("{other:?}"),
            }
        }
    }

    /// An endpoint that comes between the last listing and the deadline (listings every 7 s: 595,
    /// 602; the endpoint at 598) is found by the deadline's fresh look: on with the pipeline —
    /// READY, nothing terminated — not "left running" with an endpoint it has.
    #[tokio::test(start_paused = true)]
    async fn an_endpoint_only_the_deadlines_look_sees_goes_on() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 598)])]);
        let cfg = cfg(None);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.interval = Duration::from_secs(7);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "nothing terminated");
        let (_, at) = lines.find("[devtest-apple] endpoint 10.0.0.1:22001 (after 10m00s)");
        assert_eq!(at, Duration::from_secs(600));
    }

    /// Review of #39, nit C: a pod the deadline's own look is the first to see up without an
    /// endpoint — RunPod: its container came up behind the proxy only between the last listing
    /// (595 s, listings every 7 s) and the deadline (600 s) — gets its whole grace too, once: the
    /// wait resumes, and after it the pod is replaced (the replacement READY). One whose endpoint
    /// comes during that grace is READY, nothing terminated.
    #[tokio::test(start_paused = true)]
    async fn a_pod_first_seen_up_without_an_endpoint_at_the_deadline_gets_its_grace() {
        let cfg = cfg(None);
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 598), ("10.0.0.2", 22002, 0)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.interval = Duration::from_secs(7);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        let (_, at) = lines.find(
            "[devtest-apple] no SSH endpoint after 600s, and only now up without one: giving it its 5m00s (--endpoint-timeout)",
        );
        assert_eq!(at, Duration::from_secs(600));
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s");
        assert!(at >= Duration::from_secs(900) && at < Duration::from_secs(910), "{at:?}");
        assert_eq!(rows[0].attempts[0].end, AttemptEnd::NoEndpoint(PROXY_ONLY_END.into()));

        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 598)])]);
        fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("10.0.0.1".into(), 22001, Duration::from_secs(700))]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.interval = Duration::from_secs(7);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "nothing terminated");
        let (_, at) = lines.find("[devtest-apple] endpoint 10.0.0.1:22001");
        assert!(at >= Duration::from_secs(700) && at < Duration::from_secs(710), "{at:?}");
    }

    /// Review of #39, nit A: on a backend whose deadline is a failed placement (Shadeform: it
    /// can't be stopped, and bills until deleted) a pod is never "left running, still billing"
    /// at it, as before `--endpoint-timeout`. Judged without an endpoint at 590 s (active with no
    /// IP from 230 s: 5 minutes plus its 60 s boot grace), its fresh look lands past the 600 s
    /// deadline (a 15 s listing) and can't condemn it — unreadable (a 5xx), or not up — so it is
    /// a failed placement as at the deadline: deleted. And its grace never runs past the
    /// deadline: one active with no IP from 500 s is deleted at 600 s, not judged at 860 s.
    #[tokio::test(start_paused = true)]
    async fn on_shadeform_the_deadline_still_deletes_a_pod_without_an_endpoint() {
        let cfg = cfg(None);
        for case in ["unreadable", "not up"] {
            let mut fleet = Fleet::new(&[("apple", &[("bare", 0, 230)])]);
            (fleet.tag, fleet.pending, fleet.ready) = (Some("shadeform"), Some("PENDING"), Some(Duration::from_secs(15 * 60)));
            fleet.sync_delay = Duration::from_secs(15);
            if case == "not up" {
                fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("pending".into(), 0, Duration::from_secs(595))]);
            }
            let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
            let made = make(&fleet, &["apple"]).await;
            let outage = async {
                if case == "unreadable" {
                    tokio::time::sleep(Duration::from_secs(595)).await;
                    *fleet.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(100_000));
                }
            };
            let lines = Lines::new();
            let say = |to, l: &str| lines.say(to, l);
            let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &say), outage);
            assert_eq!(fleet.events()[..2], [format!("create devtest-apple {A4000}"), "terminate id1".to_string()], "{case}");
            let (_, at) = lines.find("[devtest-apple] not up after 10m05s (--timeout) — before it came up: a failed placement");
            assert_eq!(at, Duration::from_secs(605), "{case}");
            match &rows[0].verdict {
                Verdict::Failed { stage: Stage::Endpoint, reason } => assert!(!reason.contains("left running"), "{case}: {reason}"),
                other => panic!("{case}: {other:?}"),
            }
        }

        let mut fleet = Fleet::new(&[("apple", &[("bare", 0, 500)])]);
        (fleet.tag, fleet.pending, fleet.ready) = (Some("shadeform"), Some("PENDING"), Some(Duration::from_secs(15 * 60)));
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(fleet.events()[..2], [format!("create devtest-apple {A4000}"), "terminate id1".to_string()]);
        let (_, at) = lines.find("[devtest-apple] not up after 10m00s (--timeout) — before it came up: a failed placement");
        assert_eq!(at, Duration::from_secs(600));
    }

    /// Shadeform's deadline looks once more before it deletes (review of the nits): an instance
    /// whose IP came between the last listing (595 s, every 7 s) and the 600 s deadline goes on,
    /// READY, nothing deleted; one deleted elsewhere meanwhile is said to be gone — not deleted
    /// again, nothing recreated.
    #[tokio::test(start_paused = true)]
    async fn shadeforms_deadline_looks_once_more_before_it_deletes() {
        let cfg = cfg(None);
        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 598)])]);
        (fleet.tag, fleet.pending, fleet.ready) = (Some("shadeform"), Some("PENDING"), Some(Duration::from_secs(15 * 60)));
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.interval = Duration::from_secs(7);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "nothing deleted");
        let (_, at) = lines.find("[devtest-apple] endpoint 10.0.0.1:22001 (after 10m00s)");
        assert_eq!(at, Duration::from_secs(600));

        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)])]);
        (fleet.tag, fleet.pending, fleet.ready) = (Some("shadeform"), Some("PENDING"), Some(Duration::from_secs(15 * 60)));
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let elsewhere = async {
            tokio::time::sleep(Duration::from_secs(595)).await;
            fleet.terminate_pod("id1").await.unwrap();
        };
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &|_, _| {}), elsewhere);
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()], "only the one elsewhere");
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert_eq!(
                reason,
                "not up after 10m00s (--timeout), and shadeform no longer lists it (terminated elsewhere?) — nothing left running"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Review of #39, nit B: Shadeform hides an `active` instance's IP for its 60 s boot grace
    /// (`shadeform::BOOT_GRACE`), so "active, no IP" is expected then — not counted towards
    /// `--endpoint-timeout`. With `--endpoint-timeout 1`, an instance active from 30 s whose IP
    /// shows at 95 s (a clock a few seconds behind Shadeform's) is READY, not terminated at 90 s.
    /// One that never gets an IP is still replaced, after both: 2 minutes.
    #[tokio::test(start_paused = true)]
    async fn shadeforms_boot_grace_is_not_counted_against_the_endpoint_timeout() {
        let cfg = cfg(None);
        let shadeform = |script: &[Script<'_>]| {
            let mut fleet = Fleet::new(script);
            (fleet.tag, fleet.pending, fleet.ready) = (Some("shadeform"), Some("PENDING"), Some(Duration::from_secs(15 * 60)));
            fleet
        };
        let fleet = shadeform(&[("apple", &[("bare", 0, 30)])]);
        fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("10.0.0.1".into(), 22001, Duration::from_secs(95))]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.endpoint_timeout = Duration::from_secs(60);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "nothing terminated");

        let fleet = shadeform(&[("apple", &[("bare", 0, 30), ("10.0.0.2", 22002, 0)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.endpoint_timeout = Duration::from_secs(60);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        let (_, at) = lines.find("[devtest-apple] listed running for 2m00s with no SSH endpoint");
        assert_eq!(at, Duration::from_secs(150));
    }

    /// Hetzner, Vast and Shadeform pods still coming up (`initializing`, `LOADING`, `PENDING`)
    /// aren't up without an endpoint: waited for as before — READY when it comes, Vast's at
    /// minute 8 (past the endpoint timeout) — and a Vast instance still loading at `--timeout` is
    /// left running as before. A Vast instance listed *running* without one is in RunPod's
    /// proxy-only state, and replaced the same way.
    #[tokio::test(start_paused = true)]
    async fn hetzner_vast_and_shadeform_still_coming_up_are_waited_for_as_before() {
        let cfg = cfg(None);
        for (tag, pending, after) in [("hetzner", "initializing", 40), ("vast", "LOADING", 480), ("shadeform", "PENDING", 400)] {
            let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, after)])]);
            (fleet.tag, fleet.pending) = (Some(tag), Some(pending));
            if tag == "shadeform" {
                fleet.ready = Some(Duration::from_secs(15 * 60));
            }
            let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
            let made = make(&fleet, &["apple"]).await;
            let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
            assert_eq!(rows[0].verdict, Verdict::Ready, "{tag}");
            assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "{tag}: nothing terminated");
        }

        // Still loading at --timeout: left running, as before.
        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)])]);
        (fleet.tag, fleet.pending) = (Some("vast"), Some("LOADING"));
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(
            matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason }
                if reason == "no SSH endpoint after 600s, still listed LOADING — left running, still billing ($0.17/h): \
                              `arena proxy apply` once it has an endpoint, or `arena pods terminate devtest-apple`"),
            "{:?}",
            rows[0].verdict
        );
        assert_eq!(fleet.events().len(), 1, "nothing terminated");

        // Listed running with no endpoint: replaced at the endpoint timeout.
        let mut fleet = Fleet::new(&[("apple", &[("bare", 0, 30), ("10.0.0.2", 22002, 0)])]);
        (fleet.tag, fleet.pending) = (Some("vast"), Some("LOADING"));
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        lines.find("[devtest-apple] listed running for 5m00s with no SSH endpoint (no public IP/port) — vast names no machine or data centre for it: a host failure");
        assert_eq!(rows[0].attempts[0].end, AttemptEnd::NoEndpoint("running with no SSH endpoint".into()));
    }

    /// Where it was is said — the machine (RunPod's details) and the data centre (its listing) —
    /// so a host that keeps doing this can be avoided; in the attempt log too. One its provider
    /// names neither for is said to be that.
    #[tokio::test(start_paused = true)]
    async fn the_machine_and_data_centre_of_a_pod_without_an_endpoint_are_said() {
        let mut fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        fleet.dc = Some("EU-RO-1");
        fleet.machines_by_id.lock().unwrap().insert("id1".into(), "ff3mcwkwnb9v".into());
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        lines.find(
            "[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no \
             scp/rsync/port-forwards) — on machine ff3mcwkwnb9v, data centre EU-RO-1: a host failure",
        );
        assert_eq!(rows[0].attempts[0].host.as_deref(), Some("machine ff3mcwkwnb9v, data centre EU-RO-1"));
        let summary = pipeline::render_up_summary(&rows);
        assert!(
            summary.contains(
                "#1 1×RTX A4000 COMMUNITY (id1, machine ff3mcwkwnb9v, data centre EU-RO-1): no public SSH endpoint \
                 (RunPod ssh proxy only); terminated"
            ),
            "{summary}"
        );

        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        lines.find(
            "[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no \
             scp/rsync/port-forwards) — runpod names no machine or data centre for it: a host failure",
        );
    }

    /// Never a pod this run didn't create: another name's pod, behind the proxy only all along
    /// (made before the run — by hand, or another `up`), is left alone while this run's pod
    /// takes its time.
    #[tokio::test(start_paused = true)]
    async fn a_pod_up_did_not_create_is_never_terminated() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 400)]), ("bloom", &[("proxy", 0, 0)])]);
        let theirs = make(&fleet, &["bloom"]).await;
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!((rows.len(), rows[0].verdict.clone()), (1, Verdict::Ready));
        assert!(!fleet.events().iter().any(|e| e.starts_with("terminate")), "{:?}", fleet.events());
        assert!(fleet.listed().iter().any(|p| p.id == theirs[0].pod.id && p.proxy_only), "still there, untouched");
    }

    /// Review (codex r4, high): a pod is this run's by its backend and id together. Another
    /// backend's server under the same id (Vast's and Hetzner's ids are both numbers), running
    /// with no endpoint and listed first, starts no clock and is never condemned: this run's
    /// pod, whose endpoint comes after the grace, is READY, and nothing is terminated anywhere.
    #[tokio::test(start_paused = true)]
    async fn another_backends_pod_under_the_same_id_is_never_judged_or_terminated() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 400)])]);
        let theirs =
            Pod { id: "id1".into(), name: "devtest-bloom".into(), provider: "hetzner".into(), status: "running".into(), ..Default::default() };
        assert!(status::up_without_endpoint(&theirs), "theirs would be condemned if it were ours");
        fleet.foreign.lock().unwrap().push(theirs);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        assert_eq!((made[0].pod.id.as_str(), made[0].pod.provider.as_str()), ("id1", "runpod"), "the same id, another backend");
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert!(!fleet.events().iter().any(|e| e.starts_with("terminate")), "{:?}", fleet.events());
        let (_, at) = lines.find("[devtest-apple] endpoint ");
        assert_eq!(at, Duration::from_secs(400), "its own endpoint, not judged at 5 minutes on theirs");
    }

    /// Review (codex r1, high): nothing is terminated on an old look. The clock ends between two
    /// listings (every 7 s: 301, 308; the clock from 7 s ends at 307): its own backend is asked
    /// once more, and a pod that has got its endpoint since (at 305) goes on with the pipeline —
    /// READY, nothing terminated. And one whose listings go missing (RunPod's list failing) is
    /// judged only on a look that can be read: none can, so after its grace it is left running,
    /// said to bill, not replaced.
    #[tokio::test(start_paused = true)]
    async fn a_pod_is_judged_on_a_fresh_look_never_on_an_old_one() {
        let cfg = cfg(None);
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("10.0.0.1".into(), 22001, Duration::from_secs(305))]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.interval = Duration::from_secs(7);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events().len(), 1, "nothing terminated");
        let (_, at) = lines.find(
            "[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no \
             scp/rsync/port-forwards) — but it has an endpoint now; going on",
        );
        assert_eq!(at, Duration::from_secs(307));

        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.timeout = Duration::from_secs(195);
        let made = make(&fleet, &["apple"]).await;
        let outage = async {
            tokio::time::sleep(Duration::from_secs(100)).await;
            *fleet.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(100_000));
        };
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &|_, _| {}), outage);
        assert!(!fleet.events().iter().any(|e| e.starts_with("terminate")), "{:?}", fleet.events());
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert!(
                reason.starts_with(
                    "no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no \
                     scp/rsync/port-forwards) as last seen — its state couldn't be read just now ("
                ) && reason.ends_with(
                    "so it isn't replaced, still listed RUNNING — left running, still billing ($0.17/h): `arena proxy \
                     apply` once it has an endpoint, or `arena pods terminate devtest-apple`"
                ),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Review (codex r1, medium): a container restart starts the clock over. Behind the proxy
    /// only for 3 minutes, then RUNNING with no container for 3 more (a restart pulling its
    /// image again — RunPod says nothing else), then behind the proxy again and its endpoint a
    /// minute later: READY, never replaced — the first 3 minutes don't count against the second
    /// stretch.
    #[tokio::test(start_paused = true)]
    async fn a_container_restart_starts_the_clock_over() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        fleet.later.lock().unwrap().insert(
            "devtest-apple".into(),
            vec![
                ("bare".into(), 0, Duration::from_secs(185)),
                ("proxy".into(), 0, Duration::from_secs(365)),
                ("10.0.0.1".into(), 22001, Duration::from_secs(425)),
            ],
        );
        let cfg = cfg(None);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.timeout = Duration::from_secs(900);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "never replaced");
        lines.find("[devtest-apple] endpoint 10.0.0.1:22001");
    }

    /// Review (codex r2): the fresh look itself may be the first to see a restart (RunPod:
    /// RUNNING, no container, its image pulled again) — then it's no host failure: the wait goes
    /// on, its clock started over, to the same deadline (READY once it has its endpoint). And a
    /// fresh look that can't be read (the list failing) condemns nothing either: the wait goes
    /// on, and the host is replaced only once a later look shows it behind the proxy still.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_look_that_shows_a_restart_or_shows_nothing_waits_on() {
        let cfg = cfg(None);
        // Listings every 7 s: the proxy-only clock (from 7 s) ends at 307 s, between the
        // listings at 301 and 308; the restart (bare) starts at 304 — only the fresh look sees it.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        fleet.later.lock().unwrap().insert(
            "devtest-apple".into(),
            vec![
                ("bare".into(), 0, Duration::from_secs(304)),
                ("proxy".into(), 0, Duration::from_secs(400)),
                ("10.0.0.1".into(), 22001, Duration::from_secs(450)),
            ],
        );
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        (run.interval, run.timeout) = (Duration::from_secs(7), Duration::from_secs(900));
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "never replaced");
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no scp/rsync/port-forwards) — but it's listed RUNNING, not up, now (restarting?): waiting on");
        assert_eq!(at, Duration::from_secs(307));

        // The look fails (RunPod's list down from 304 s to 320 s): nothing terminated then; the
        // clock starts over and the host is replaced once a later look confirms it.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        (run.interval, run.timeout) = (Duration::from_secs(7), Duration::from_secs(900));
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let outage = async {
            tokio::time::sleep(Duration::from_secs(304)).await;
            *fleet.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(16));
        };
        let say = |to, l: &str| lines.say(to, l);
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &say), outage);
        let (_, waited) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no scp/rsync/port-forwards) — but it can't be looked at again just now (");
        assert_eq!(waited, Duration::from_secs(307));
        assert!(lines.all().iter().any(|l| l.ends_with(": waiting on, nothing terminated")), "{:#?}", lines.all());
        let (_, at) = lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        assert!(at >= Duration::from_secs(607), "a whole new clock first: {at:?}");
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// Review (codex r3): a restart that only the fresh look saw isn't read over by the older
    /// listing before it. The look (307 s) sees RUNNING with no container; RunPod's list then
    /// fails until 650 s, and the container is up behind the proxy again at 649 s. The clock
    /// starts at the first listing after the look that shows that (651 s), not from the stale
    /// proxy-only one: replaced a whole grace later (951 s, past the 900 s --timeout), not at 651.
    #[tokio::test(start_paused = true)]
    async fn a_restart_only_the_fresh_look_saw_starts_the_clock_over() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        fleet.later.lock().unwrap().insert(
            "devtest-apple".into(),
            vec![("bare".into(), 0, Duration::from_secs(304)), ("proxy".into(), 0, Duration::from_secs(649))],
        );
        let cfg = cfg(None);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        (run.interval, run.timeout) = (Duration::from_secs(7), Duration::from_secs(900));
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let outage = async {
            tokio::time::sleep(Duration::from_millis(307_500)).await;
            *fleet.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_millis(342_500));
        };
        let say = |to, l: &str| lines.say(to, l);
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &say), outage);
        lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no scp/rsync/port-forwards) — but it's listed RUNNING, not up, now (restarting?): waiting on");
        let (_, at) = lines.find("[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)");
        assert_eq!(at, Duration::from_secs(951), "a whole grace from the first listing after the look");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// RunPod v2 looks a running pod's endpoint up in its details; a failed lookup lists the pod
    /// without its endpoint *and* its proxy-only flag. That's no news: the clock started when it
    /// was seen behind the proxy runs on (replaced at ~5 minutes, not given a fresh 5) — and a pod
    /// whose lookups fail all the way to `--timeout` isn't known to have no endpoint, so it is
    /// left running as before, not replaced.
    #[tokio::test(start_paused = true)]
    async fn a_failed_endpoint_lookup_is_no_news() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let outage = async {
            tokio::time::sleep(Duration::from_secs(200)).await;
            *fleet.lookups_fail_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(60));
        };
        let say = |to, l: &str| lines.say(to, l);
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &say), outage);
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m");
        assert!(at < Duration::from_secs(330), "the clock ran on through the failed lookups: {at:?}");
        assert_eq!(rows[0].verdict, Verdict::Ready);

        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)])]);
        *fleet.lookups_fail_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(100_000));
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events().len(), 1, "unknown is not \"no endpoint\": nothing terminated");
        assert!(
            matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.starts_with("no SSH endpoint after 600s — left running")),
            "{:?}",
            rows[0].verdict
        );
    }

    /// Shadeform (spec §8.3/§10.4): a create that succeeded can still fail — the instance goes to
    /// `error`, or never becomes active. On a backend with a ready deadline, a pod listed ERROR
    /// during the endpoint wait is terminated at once (not waited out to the deadline, never
    /// left billing) and the name placed on the next option — even without --check, within its
    /// placements; the attempt log says `error: <status details>`.
    #[tokio::test(start_paused = true)]
    async fn a_pod_that_goes_to_error_is_deleted_and_the_next_option_tried() {
        let mut fleet = Fleet::new(&[("apple", &[("error", 0, 30), ("10.0.0.2", 22002, 0)])]);
        fleet.ready = Some(Duration::from_secs(900));
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!((rows[0].attempts[0].end.clone(), rows[0].attempts[0].terminated), (AttemptEnd::Dead("error: provider failed to launch".into()), true));
        let (_, at) = lines.find("[devtest-apple] error: provider failed to launch — before it came up");
        assert!(at < Duration::from_secs(60), "said at once, not at the deadline: {at:?}");
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("never came up — error: provider failed to launch; terminated"), "{summary}");
        assert!(fake.calls_to(&host("10.0.0.2", 22002)).len() == 2, "only the replacement was set up");

        // No placement left: still terminated (it may bill), and the name fails at `endpoint`.
        let mut fleet = Fleet::new(&[("apple", &[("error", 0, 30)])]);
        fleet.ready = Some(Duration::from_secs(900));
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.placements = 1;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()]);
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => {
                assert!(reason.contains("error: provider failed to launch") && reason.contains("no attempts left") && reason.contains("terminated it"), "{reason}");
            }
            other => panic!("{other:?}"),
        }

        // A backend without a ready deadline (RunPod, Vast): ERROR isn't known to be final there —
        // the wait runs on as before, and the pod is left as it is.
        let fleet = Fleet::new(&[("apple", &[("error", 0, 30)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events().len(), 1, "nothing terminated");
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.contains("no SSH endpoint after 120s")), "{:?}", rows[0].verdict);
    }

    /// Not up by the backend's ready deadline (SHADEFORM_READY_MINS) — shorter than `--timeout`:
    /// the same failed placement at the deadline, terminated, the next option tried.
    #[tokio::test(start_paused = true)]
    async fn a_pod_not_up_by_its_ready_deadline_is_deleted_and_replaced() {
        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 500), ("10.0.0.2", 22002, 0)])]);
        fleet.ready = Some(Duration::from_secs(120));
        fleet.pending = Some("PENDING"); // Shadeform's, until it's active
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert!(matches!(&rows[0].attempts[0].end, AttemptEnd::Dead(w) if w.starts_with("not up after 2m")), "{:?}", rows[0].attempts[0]);
        let (_, at) = lines.find("[devtest-apple] not up after 2m");
        assert_eq!(at, Duration::from_secs(120));
    }

    /// REVIEW sf SF1 (money): with the shipped defaults — `pods up --timeout 600` and
    /// `SHADEFORM_READY_MINS=15` — a Shadeform pod that never comes up is left billing. The
    /// endpoint stage keeps the backend's ready deadline only when it is `<= --timeout`
    /// (`ready_deadline(..).filter(|r| *r <= self.run.timeout)`); 900 s isn't, so `ready` is
    /// `None`, the wait ends at 600 s as a plain `TimedOut`, and that path says "no SSH endpoint
    /// after 600s — left running": no terminate, no next option, the VM bills (pending
    /// time may be billed — unverified) until someone notices. The lane's own test only used a
    /// deadline *under* the timeout (120 s). A backend with a ready deadline must make the
    /// endpoint timeout a failed placement whichever bound comes first (`min(ready, --timeout)`).
    #[tokio::test(start_paused = true)]
    async fn review_sf_sf1_a_pod_pending_past_the_default_timeout_is_not_left_billing() {
        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 5000), ("10.0.0.2", 22002, 0)])]);
        fleet.ready = Some(Duration::from_secs(15 * 60)); // SHADEFORM_READY_MINS' default
        fleet.pending = Some("PENDING");
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        assert_eq!(run.timeout, Duration::from_secs(600), "`pods up --timeout`'s default");
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(
            fleet.events().iter().any(|e| e == "terminate id1"),
            "a Shadeform pod still pending at --timeout was left running: events {:?}, verdict {:?}",
            fleet.events(),
            rows[0].verdict
        );
    }

    /// Review fix: the proxy-only grace restarted whenever one listing lacked the pod — and the
    /// poller's aggregate listing drops every RunPod pod while RunPod's own list fails (a single
    /// 5xx/429), so one gap turned a replaceable host into `FAILED endpoint`, left billing. A
    /// gap now proves nothing: the grace runs on, and the host is replaced at ~5 min as usual.
    #[tokio::test(start_paused = true)]
    async fn a_listing_gap_does_not_restart_the_proxy_only_grace() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let mut run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        run.timeout = Duration::from_secs(450);
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let outage = async {
            tokio::time::sleep(Duration::from_secs(240)).await;
            *fleet.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(15));
        };
        let say = |to, l: &str| lines.say(to, l);
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &say), outage);
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m");
        assert!(at < Duration::from_secs(330), "the grace kept running through the gap: {at:?}");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// An endpoint deadline shorter than the grace (`--timeout` < 300s) came while the pod was up
    /// behind the proxy only. An earlier review made that a host failure at once (left billing
    /// otherwise); codex r3: that condemns a pod after seconds without an endpoint. Now the pod
    /// gets its whole grace, past `--timeout` — then, still behind the proxy, it is the host
    /// failure, with or without --check; and one whose endpoint comes in that time is READY.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_inside_the_grace_waits_the_whole_grace_out() {
        let cfg = cfg(None);
        for check in [Some(2), None] {
            let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
            let mut run = up_run(&fleet, fake.clone(), &cfg, true, check, None).await;
            run.timeout = Duration::from_secs(120);
            let made = make(&fleet, &["apple"]).await;
            let lines = Lines::new();
            let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
            assert_eq!(
                fleet.events(),
                [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")],
                "{check:?}"
            );
            assert_eq!(rows[0].verdict, Verdict::Ready, "{check:?}");
            assert_eq!(rows[0].attempts[0].end, AttemptEnd::NoEndpoint(PROXY_ONLY_END.into()));
            let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s");
            assert_eq!(at, Duration::from_secs(310), "{check:?}: a whole grace, not the 120s --timeout");
        }

        // Behind the proxy from 100s, its endpoint at 200s — past the 120s --timeout, inside the
        // grace: READY, never replaced.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 100)])]);
        fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("10.0.0.1".into(), 22001, Duration::from_secs(200))]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, false, None, None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!(fleet.events().len(), 1, "never replaced");

        // A plain timeout — a pod still coming up (provisioning), never up without an endpoint —
        // is unchanged: left running.
        let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 500)])]);
        fleet.pending = Some("PROVISIONING");
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, Some(2), None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.starts_with("no SSH endpoint after 120s")), "{:?}", rows[0].verdict);
        assert_eq!(fleet.events().len(), 1, "nothing terminated");
    }

    /// The live case of 2026-10-30, end to end: RunPod listed the pod RUNNING with no container
    /// while it pulled its image (6m39s), then its container came up behind the proxy only (no
    /// public IP). Its clock starts with the container — so the 600s --timeout comes first — and
    /// it is still replaced after a whole grace, not left billing at the deadline.
    #[tokio::test(start_paused = true)]
    async fn a_slow_pull_onto_a_host_with_no_public_ip_is_still_replaced() {
        let fleet = Fleet::new(&[("apple", &[("bare", 0, 0), ("10.0.0.2", 22002, 0)])]);
        fleet.later.lock().unwrap().insert("devtest-apple".into(), vec![("proxy".into(), 0, Duration::from_secs(400))]);
        let cfg = cfg(None);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s");
        assert_eq!(at, Duration::from_secs(700), "from the container's start, past the 600s --timeout");
    }

    /// Live G: a pod terminated elsewhere while `up` waited for its endpoint ended `FAILED
    /// endpoint — no SSH endpoint after 600s — left running`. A missing listing still proves
    /// nothing during the wait, but at the deadline its own backend is asked: gone is said.
    /// One its backend can't list at all stays "left running" (unknown is not gone).
    #[tokio::test(start_paused = true)]
    async fn a_pod_terminated_elsewhere_during_the_endpoint_wait_is_not_left_running() {
        let cfg = cfg(None);
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)])]);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let elsewhere = async {
            tokio::time::sleep(Duration::from_secs(100)).await;
            fleet.terminate_pod("id1").await.unwrap();
        };
        let (rows, ()) = tokio::join!(run_up(&run, made, std::future::pending(), &|_, _| {}), elsewhere);
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => assert!(
                reason.starts_with("no SSH endpoint after 600s, and runpod no longer lists it (terminated elsewhere?)")
                    && reason.ends_with("— nothing left running")
                    && !reason.contains("left running:"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(rows[0].gpu, "-", "no pod to describe");
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()], "only the one from elsewhere");

        // Its backend failing to list at the deadline: unknown — as before.
        let fleet = Fleet {
            down_after_terminate: Duration::from_secs(100_000),
            ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 100_000)]), ("bloom", &[("10.0.0.2", 22002, 0)])])
        };
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        // Another pod's terminate makes the fake's RunPod listing fail from then on.
        let bloom = make(&fleet, &["bloom"]).await;
        fleet.terminate_pod(&bloom[0].pod.id).await.unwrap();
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(
            matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason }
                if reason == "no SSH endpoint after 600s — left running: `arena proxy apply` once it has an endpoint, or \
                              `arena pods terminate devtest-apple`"),
            "{:?}",
            rows[0].verdict
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_with_no_capacity_fails_the_name_and_says_it_has_no_pod() {
        // Only one endpoint is scripted, so every later create is "no capacity".
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                format!("create devtest-apple {A4000}"),
            ]
        );
        let row = &rows[0];
        match &row.verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.ends_with("; its replacement wasn't placed (no capacity on any option) — devtest-apple has no pod now"),
                "{reason}"
            ),
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert_eq!((row.gpu.as_str(), row.price.as_str(), row.proxy_port), ("-", "-", None), "no pod, nothing to show");
    }

    #[tokio::test(start_paused = true)]
    async fn when_attempts_run_out_the_name_fails_with_every_attempt_and_its_last_pod_kept() {
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)]),
            ("bloom", &[("10.0.0.5", 22005, 0)]),
        ]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.5", 22005), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let keys = keys_dir("exhausted");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), Some(&keys.0)).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        // Two placements for apple, the second left running for a look (no terminate id3).
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                format!("create devtest-bloom {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
            ]
        );
        let apple = &rows[0];
        match &apple.verdict {
            Verdict::Failed { stage: Stage::Check, reason } => {
                assert!(reason.ends_with("— no attempts left (--check-attempts 2); left running"), "{reason}")
            }
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert_eq!(rows[1].verdict, Verdict::Ready);
        // Keys only ever went to the READY pod.
        assert_eq!(execs_with(&fake, &host("10.0.0.1", 22001), "OPENAI_API_KEY"), 0);
        assert_eq!(execs_with(&fake, &host("10.0.0.2", 22002), "OPENAI_API_KEY"), 0);
        assert_eq!(execs_with(&fake, &host("10.0.0.5", 22005), "OPENAI_API_KEY"), 1);
        // The summary names every attempt.
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("devtest-apple: #1 1×RTX A4000 COMMUNITY (id1, 10.0.0.1): check FAIL — cuda: "), "{summary}");
        assert!(summary.contains("; terminated → #2 1×RTX 3090 COMMUNITY (id3, 10.0.0.2): check FAIL — cuda: "), "{summary}");
        assert!(summary.contains("; left running\ndevtest-apple: FAILED check — deep check failed (host): cuda: "), "{summary}");
        assert!(summary.contains("devtest-apple  1×RTX A4000"), "the check's own GPU view: {summary}");
        assert!(summary.contains("1 ready, 1 failed (of 2)"), "{summary}");
        assert!(conclude(&rows, &|_, _| {}).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn sshd_lagging_its_endpoint_is_waited_out_never_failed_or_replaced() {
        // sshd refuses for 4 minutes after the endpoint appears — past `pods setup`'s 150s
        // boot window, well inside this pod's 600s to come up.
        let refused = || FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22001: Connection refused");
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), (0..40).map(|_| refused()));
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready, "{:#?}", lines.all());
        let (_, at) = lines.find("[devtest-apple] setup ✓");
        assert!(at >= Duration::from_secs(240), "{at:?}");

        // Without setup the check waits for SSH the same way: a booting pod is not a bad host.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), (0..40).map(|_| refused()));
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), healthy()]);
        let run = up_run(&fleet, fake.clone(), &cfg, false, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!(rows[0].health, Some(Status::Pass));
        assert_eq!(fleet.events().len(), 1, "never replaced: {:?}", fleet.events());
    }

    #[tokio::test(start_paused = true)]
    async fn a_warning_counts_as_ready_and_pod_details_are_fetched_once_for_the_fleet() {
        // Three healthy pods checked together; cloud's host has a maintenance window (WARN).
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0)]),
            ("bloom", &[("10.0.0.2", 22002, 0)]),
            ("cloud", &[("10.0.0.3", 22003, 0)]),
        ]);
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22002, 22003] {
            fake.script(&host(&format!("10.0.0.{}", port - 22000), port), [FakeReply::ok(), healthy()]);
        }
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, false, Some(2), None).await;
        let made = make(&fleet, &["apple", "bloom", "cloud"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        let health: Vec<(Option<Status>, &Verdict)> = rows.iter().map(|r| (r.health, &r.verdict)).collect();
        assert_eq!(
            health,
            [(Some(Status::Pass), &Verdict::Ready), (Some(Status::Pass), &Verdict::Ready), (Some(Status::Warn), &Verdict::Ready)]
        );
        // No setup ran, so first a quick "does SSH answer?" — then the deep check as `pods
        // test --deep` runs it: one bounded exec.
        for port in [22001, 22002, 22003] {
            let calls = fake.calls_to(&host(&format!("10.0.0.{}", port - 22000), port));
            assert!(
                matches!(&calls[..], [RemoteCall::Exec { cmd, .. }, RemoteCall::Exec { timeout, .. }]
                    if cmd == "true" && *timeout == Some(DEEP_CHECK_TIMEOUT)),
                "{calls:?}"
            );
        }
        lines.find("[devtest-cloud] check: warn — maintenance: ");
        lines.find("[devtest-cloud] READY after 0s — 1×RTX A4000 COMMUNITY, check warn");
        // One details fetch (one RunPod GraphQL query) for three checks, plus the summary's.
        assert_eq!(fleet.enriched.load(Ordering::SeqCst), 2);
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("devtest-cloud: warn — maintenance: "), "{summary}");
        assert!(summary.contains("3 ready, 0 failed (of 3)"), "{summary}");
        assert!(conclude(&rows, &|_, _| {}).is_ok(), "a WARN is ready");
    }

    #[tokio::test(start_paused = true)]
    async fn proxy_writes_never_overlap_and_each_pod_gets_its_port() {
        // Three endpoints at the same instant: three pipelines reach the proxy together.
        let dir = tmp_dir("proxy");
        let conf = dir.0.join("proxy.conf");
        let fleet = Fleet {
            sync_delay: Duration::from_secs(2),
            ..Fleet::new(&[
                ("apple", &[("10.0.0.1", 22001, 0)]),
                ("bloom", &[("10.0.0.2", 22002, 0)]),
                ("cloud", &[("10.0.0.3", 22003, 0)]),
            ])
        };
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(Some(&conf));
        let run = up_run(&fleet, fake.clone(), &cfg, false, None, None).await;
        assert!(run.proxy.is_ok(), "a write-only proxy is deployable: {:?}", run.proxy);
        let made = make(&fleet, &["apple", "bloom", "cloud"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(fleet.max_syncing.load(Ordering::SeqCst), 1, "one proxy writer at a time");
        lines.find("[devtest-apple] proxy: port 9500 → 10.0.0.1:22001");
        lines.find("[devtest-bloom] proxy: port 9501 → 10.0.0.2:22002");
        lines.find("[devtest-cloud] proxy: port 9502 → 10.0.0.3:22003");
        lines.find("[proxy] after up: +0 ~0 -0 =0 (unchanged)");
        let text = std::fs::read_to_string(&conf).unwrap();
        for target in ["10.0.0.1:22001", "10.0.0.2:22002", "10.0.0.3:22003"] {
            assert!(text.contains(&format!("proxy_pass {target};")), "{text}");
        }
        let ports: Vec<Option<u16>> = rows.iter().map(|r| r.proxy_port).collect();
        assert_eq!(ports, [Some(9500), Some(9501), Some(9502)]);
        // --no-setup without --check: over SSH only the "does it answer?" before READY.
        for c in fake.calls() {
            assert!(matches!(&c, RemoteCall::Exec { cmd, .. } if cmd == "true"), "{c:?}");
        }
        assert_eq!(fake.calls().len(), 3);
    }

    /// Live R: `up --no-setup` on Hetzner said READY after 1 s, while the VM was still
    /// initializing — the endpoint (its IP) is known long before sshd answers. Without setup,
    /// READY now waits for SSH to answer, within the pod's `--timeout`; one that never does
    /// FAILs (left running), and a refused key is an answer, not a wait.
    #[tokio::test(start_paused = true)]
    async fn without_setup_ready_waits_for_ssh_to_answer() {
        let cfg = cfg(None);
        let refused = || FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22001: Connection refused");
        // sshd refuses for 2+ min (booting; one try every 6 s), then answers: READY only then.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), (0..24).map(|_| refused()));
        let run = up_run(&fleet, fake.clone(), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(rows[0].verdict, Verdict::Ready);
        let (_, at) = lines.find("[devtest-apple] READY after ");
        assert!(at >= Duration::from_secs(24 * 6), "READY only once SSH answered: {at:?}");
        assert_eq!(fake.calls().len(), 25, "24 refusals, then the answer");

        // Never answers within --timeout: FAILED at the endpoint stage, left running.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), (0..1000).map(|_| refused()));
        let run = up_run(&fleet, fake.clone(), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => {
                assert!(reason.contains("SSH never answered") && reason.contains("still unreachable after 600s"), "{reason}");
                assert!(reason.contains("left running"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!fleet.events().iter().any(|e| e.starts_with("terminate")), "{:?}", fleet.events());

        // Our key refused: that's an answer — FAILED at once, not after the whole wait.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::exit(255, "root@10.0.0.1: Permission denied (publickey).")]);
        let run = up_run(&fleet, fake.clone(), &cfg, false, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.contains("Permission denied")));
        let (_, at) = lines.find("[devtest-apple] FAILED ");
        assert!(at < Duration::from_secs(5), "{at:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_c_stops_every_pipeline_where_it_is_and_terminates_nothing() {
        // apple is READY quickly; bloom never gets an endpoint; cloud's check would come back
        // FAIL at t=120s — but Ctrl+C lands at t=100s, mid-check. dune's check "fails" at the
        // very instant of the Ctrl+C (in a terminal the SIGINT kills the ssh child too): that
        // must read as stopped, never as a FAIL to replace.
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0)]),
            ("bloom", &[("10.0.0.2", 22002, 10_000)]),
            ("cloud", &[("10.0.0.3", 22003, 0)]),
            ("dune", &[("10.0.0.4", 22004, 0)]),
        ]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), bad_host().after(Duration::from_secs(120))]);
        fake.script(&host("10.0.0.4", 22004), [FakeReply::ok(), FakeReply::ok(), FakeReply::exit(255, "").after(Duration::from_secs(100))]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple", "bloom", "cloud", "dune"]).await;
        let lines = Lines::new();
        let start = Instant::now();
        let rows = run_up(&run, made, tokio::time::sleep(Duration::from_secs(100)), &|to, l| lines.say(to, l)).await;

        assert_eq!(start.elapsed(), Duration::from_secs(100), "the report comes at the interrupt, not at any budget");
        let verdicts: Vec<Verdict> = rows.iter().map(|r| r.verdict.clone()).collect();
        let stopped_at = |stage| Verdict::Stopped { stage };
        assert_eq!(verdicts, [Verdict::Ready, stopped_at(Stage::Endpoint), stopped_at(Stage::Check), stopped_at(Stage::Check)]);
        assert!(fleet.events().iter().all(|e| e.starts_with("create")), "nothing terminated: {:?}", fleet.events());
        assert_eq!(fleet.events().len(), 4, "and no new attempt");
        lines.find("[devtest-cloud] STOPPED at check (Ctrl+C) — left as it is");
        assert!(conclude(&rows, &|_, _| {}).is_err(), "not everything is ready");
    }

    #[tokio::test(start_paused = true)]
    async fn up_check_end_to_end_replaces_on_the_single_configured_option() {
        // Through the command: no --gpu list, so a replacement reuses the one configured
        // option (`single_option_plan`) — a different machine of the same type.
        use crate::{handle_pods, PodCmd};
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let up = PodCmd::Up {
            names: vec!["apple".into()],
            count: None,
            add: None,
            gpu: None,
            gpus: None,
            server_type: None,
            cloud: None,
            max_price: None,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run: false,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            check: true,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 600,
            endpoint_timeout: None,
            interval: 10,
        };
        handle_pods(up, &fleet, fake.clone(), &cfg(None), true).await.unwrap();
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {A4000}")]
        );
        assert_eq!(fake.calls_to(&host("10.0.0.2", 22002)).len(), 3, "setup (2) + check on the replacement");
    }

    /// `pods up <names> --check` as the command runs it, otherwise default flags.
    fn up_cmd(names: &[&str]) -> crate::PodCmd {
        crate::PodCmd::Up {
            names: names.iter().map(|n| n.to_string()).collect(),
            count: None,
            add: None,
            gpu: None,
            gpus: None,
            server_type: None,
            cloud: None,
            max_price: None,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run: false,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            check: true,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 600,
            endpoint_timeout: None,
            interval: 10,
        }
    }

    /// Finding #14: `up -n 3 … --retry-mins 6` held alpha and bravo — created, RUNNING,
    /// billing — until charlie's capacity window ended. A placed pod's pipeline now starts
    /// as its create returns: apple is set up, checked and locked (its last step) before
    /// bloom, out of capacity until its third round, is created. Both create paths.
    #[tokio::test(start_paused = true)]
    async fn a_placed_pod_is_brought_up_while_placement_still_retries_for_the_others() {
        for gpu in [None, Some("A4000,3090")] {
            let mut fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 0)])]);
            fleet.lockable = true;
            *fleet.dry_until.lock().unwrap() = Some(("devtest-bloom".into(), Instant::now() + Duration::from_secs(50)));
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
            fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
            let mut cmd = up_cmd(&["apple", "bloom"]);
            if let crate::PodCmd::Up { gpu: g, retry_mins, retry_secs, lock, .. } = &mut cmd {
                (*g, *retry_mins, *retry_secs, *lock) = (gpu.map(String::from), 2, 30, true);
            }
            let start = Instant::now();
            crate::handle_pods_with(cmd, &fleet, fake.clone(), &cfg(None), true, crate::Interrupt::manual()).await.unwrap();
            let events = fleet.events();
            let apple_locked = events.iter().position(|e| e == "lock id1 true").unwrap_or_else(|| panic!("{gpu:?}: {events:?}"));
            let bloom_made = events.iter().rposition(|e| e.starts_with("create devtest-bloom")).unwrap();
            assert!(apple_locked < bloom_made, "{gpu:?}: apple was READY before bloom existed: {events:?}");
            assert!(events.iter().any(|e| e.starts_with("lock id") && e != "lock id1 true"), "{gpu:?}: bloom READY too: {events:?}");
            assert!(start.elapsed() >= Duration::from_secs(60) && start.elapsed() < Duration::from_secs(120), "{gpu:?}: {:?}", start.elapsed());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_c_during_the_create_still_stops_the_pipelines() {
        // apple is created on a bad host; bloom finds no capacity, so the create waits to
        // retry — and Ctrl+C lands in that wait. The same press must hold for the pipelines:
        // apple is not set up, checked, terminated or recreated. (A fresh Ctrl+C listener
        // for the pipelines never saw it: apple was checked, terminated and recreated.)
        // Both create paths: the single configured spec, and a `--gpu` option list.
        // apple's endpoint comes after the press (its pipeline starts the moment it's
        // created — finding #14 — so an earlier endpoint would be set up before Ctrl+C).
        for gpu in [None, Some("A4000,3090")] {
            let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 5), ("10.0.0.2", 22002, 0)])]);
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
            let mut cmd = up_cmd(&["apple", "bloom"]);
            if let crate::PodCmd::Up { gpu: g, retry_mins, retry_secs, .. } = &mut cmd {
                (*g, *retry_mins, *retry_secs) = (gpu.map(String::from), 1, 30);
            }
            let ctrl_c = crate::Interrupt::manual();
            let press = ctrl_c.clone();
            let start = Instant::now();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                press.press();
            });
            let err = crate::handle_pods_with(cmd, &fleet, fake.clone(), &cfg(None), true, ctrl_c)
                .await
                .unwrap_err()
                .to_string();
            let creates: Vec<String> = fleet.events();
            assert!(creates.iter().all(|e| e.starts_with("create ")), "{gpu:?}: nothing terminated: {creates:?}");
            assert_eq!(creates.iter().filter(|e| e.starts_with("create devtest-apple")).count(), 1, "{gpu:?}: {creates:?}");
            assert!(fake.calls().is_empty(), "{gpu:?}: apple was never set up or checked: {:?}", fake.calls());
            assert_eq!(start.elapsed(), Duration::from_secs(2), "{gpu:?}: ends at the press, not the 30s retry wait");
            assert_eq!(err, "2 of 2 pod(s) not ready: devtest-apple (STOPPED endpoint), devtest-bloom (STOPPED create)", "{gpu:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_got_no_pod_is_reported_and_fails_the_run() {
        // bloom has no capacity (and no --retry-mins): apple comes up READY, bloom never got
        // a pod — it's a row of its own and the exit is non-zero.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let err = crate::handle_pods_with(up_cmd(&["apple", "bloom"]), &fleet, fake.clone(), &cfg(None), true, crate::Interrupt::manual())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "1 of 2 pod(s) not ready: devtest-bloom (FAILED create)");

        let rows = not_created_rows(&["devtest-apple".into(), "devtest-bloom".into()], &["devtest-apple".into()], false);
        let summary = pipeline::render_up_summary(&rows);
        let row = summary.lines().find(|l| l.starts_with("devtest-bloom ")).unwrap_or_else(|| panic!("{summary}"));
        assert!(row.contains(" FAILED create ") && !row.contains("READY"), "{summary}");
        assert!(summary.contains("devtest-bloom: FAILED create — no pod was created for it (the create output above says why)"), "{summary}");
        let stopped = not_created_rows(&["devtest-bloom".into()], &[], true);
        assert_eq!(stopped[0].verdict, Verdict::Stopped { stage: Stage::Create });

        // Nothing created at all is a failed `up` too, not "nothing to wait for".
        let fleet = Fleet::new(&[]);
        let err = crate::handle_pods_with(up_cmd(&["apple"]), &fleet, fake, &cfg(None), true, crate::Interrupt::manual())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "no pods were created — nothing to bring up");
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_on_the_failed_machine_with_no_attempt_left_is_terminated() {
        // Default --check-attempts 2: #1 fails on 10.0.0.1, #2 lands on 10.0.0.1 again. With
        // no attempt left it's still rejected — terminated, never set up, never routed.
        let dir = tmp_dir("same-host-last");
        let conf = dir.0.join("proxy.conf");
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.1", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        let cfg = cfg(Some(&conf));
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                "terminate id2".to_string(),
            ]
        );
        lines.find("[devtest-apple] terminating id2 — no attempt left to replace it (--check-attempts 2)");
        assert!(fake.calls_to(&host("10.0.0.1", 22002)).is_empty(), "never set up or checked");
        assert!(fleet.listed().is_empty(), "nothing of apple's left billing");
        let row = &rows[0];
        match &row.verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.ends_with(
                    "attempt 2 landed on 10.0.0.1, a machine that already failed the deep check — no attempts left \
                     (--check-attempts 2); terminated it — devtest-apple has no pod now, nothing of it left billing"
                ),
                "{reason}"
            ),
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert_eq!(row.proxy_port, None);
        let text = std::fs::read_to_string(&conf).unwrap();
        assert!(!text.contains("10.0.0.1:22002"), "the name isn't routed to the rejected pod: {text}");
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("#2 1×RTX 3090 COMMUNITY (id2, 10.0.0.1): same host as a failed check — rejected; terminated"), "{summary}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_could_not_run_is_retried_and_never_condemns_its_host() {
        let reset = || FakeReply::exit(255, "kex_exchange_identification: read: Connection reset by peer");
        let fleet = Fleet::new(&[
            ("apple", &[("10.0.0.1", 22001, 0)]),
            ("bloom", &[("10.0.0.2", 22002, 0)]),
            // cloud's replacement lands on bloom's machine, after bloom's checks.
            ("cloud", &[("10.0.0.3", 22003, 0), ("10.0.0.2", 22004, 30)]),
        ]);
        let fake = Arc::new(FakeRemote::new());
        // apple: the check's session drops once; SSH answers; the rerun is healthy.
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), reset(), FakeReply::ok(), healthy()]);
        // bloom: it can't run twice — FAILED, but kept, and its machine not condemned.
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), reset(), FakeReply::ok(), reset()]);
        // cloud: a real FAIL, replaced onto bloom's machine — which is fine to use.
        fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22004), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple", "bloom", "cloud"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                format!("create devtest-bloom {A4000}"),
                format!("create devtest-cloud {A4000}"),
                "terminate id3".to_string(), // only the pod whose check ran and failed
                format!("create devtest-cloud {R3090}"),
            ]
        );
        lines.find("[devtest-apple] check: couldn't run it (ssh: exit 255: kex_exchange_identification");
        assert_eq!((rows[0].verdict.clone(), rows[0].health), (Verdict::Ready, Some(Status::Pass)));
        match &rows[1].verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.starts_with("couldn't run the deep check twice (ssh: exit 255: kex_exchange_identification")
                    && reason.ends_with("— left running (`arena pods test --deep devtest-bloom`); its host isn't counted as bad"),
                "{reason}"
            ),
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert_eq!(rows[2].verdict, Verdict::Ready, "{:#?}", lines.all());
        assert_eq!(rows[2].attempts[1].host.as_deref(), Some("10.0.0.2"));
        assert!(lines.all().iter().all(|l| !l.contains("a machine that already failed")), "{:#?}", lines.all());
        // The rerun waited for SSH first: a probe between the two checks.
        let apple = fake.calls_to(&host("10.0.0.1", 22001));
        assert!(matches!(&apple[3], RemoteCall::Exec { cmd, .. } if cmd == "true"), "{apple:?}");
    }

    /// Live BPF F1: `up hz2 --check --no-setup` on a bare Hetzner VM — no zsh until setup —
    /// ended "FAILED check: couldn't run the deep check twice (ssh: exit 127: … zsh: command
    /// not found)". That's the pod's software: a software/setup FAIL at once (no rerun, no
    /// replacement, its machine not counted as bad), saying to run setup.
    #[tokio::test(start_paused = true)]
    async fn no_zsh_before_setup_is_a_software_failure_not_a_check_that_could_not_run() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        let no_zsh = || FakeReply::exit(127, "bash: line 1: zsh: command not found");
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), no_zsh(), no_zsh()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, false, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "never replaced");
        lines.find("[devtest-apple] check: FAIL (software/setup, not the host) — script: no zsh on the pod");
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.starts_with("software/setup failure, not the host") && reason.contains("`arena pods setup devtest-apple`"),
                "{reason}"
            ),
            other => panic!("expected FAILED check, got {other:?}"),
        }
        assert!(!lines.all().iter().any(|l| l.contains("couldn't run")), "{:#?}", lines.all());
        // One probe, one check: no rerun of a check that can't start until setup has run.
        assert_eq!(execs_with(&fake, &host("10.0.0.1", 22001), "arena-deep-check"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_waits_until_the_owning_provider_lists_the_old_pod_gone() {
        // A two-backend fleet: right after the terminate RunPod's listing fails for 60s while
        // the (empty) other backend answers, and the old pod stays listed for 30s. The
        // aggregate listing — without RunPod — would read "gone" at once and create the
        // replacement next to the old pod (the fake refuses a second listed pod per name).
        let fleet = Fleet {
            linger: Duration::from_secs(30),
            down_after_terminate: Duration::from_secs(60),
            ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])])
        };
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;

        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        let (_, at) = lines.find("[created] devtest-apple id=id2");
        assert_eq!(at, Duration::from_secs(60), "created once RunPod itself listed id1 gone");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// Defence in depth for the review fix below: if another pod takes the name while the old
    /// one is going (made outside this pipeline), the replacement is not created — it would be
    /// a second pod under that name (the fake refuses one while another is listed).
    #[tokio::test(start_paused = true)]
    async fn a_replacement_is_not_created_while_another_pod_holds_the_name() {
        no_replacement_next_to_a_twin_on("runpod").await;
    }

    /// …whichever backend that answered lists it: one on Hetzner holds the name as much as one
    /// on RunPod (the old pod's own backend isn't the only place a twin can be).
    #[tokio::test(start_paused = true)]
    async fn a_replacement_is_not_created_while_another_backend_holds_the_name() {
        no_replacement_next_to_a_twin_on("hetzner").await;
    }

    async fn no_replacement_next_to_a_twin_on(backend: &'static str) {
        let fleet = Fleet {
            linger: Duration::from_secs(20),
            twin_after_terminate: Some(backend),
            ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])])
        };
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}"), "terminate id1".to_string()]);
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.contains("terminated it, but another pod named devtest-apple (twin) is listed now — no replacement created"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Review fix (#14's streaming): `up -n 2 --check --retry-mins 3`, the plain cohort start.
    /// apple lands on a bad host and bloom has no capacity, so the create loop retries while
    /// apple's pipeline terminates its pod and waits for it to leave the listing. A retry round
    /// that re-planned from free names read apple as free in that gap and created a second
    /// devtest-apple — a second pipeline, and then the replacement made a third (the fake
    /// refuses a second listed pod per name). A round now retries only the confirmed names not
    /// created yet. Swept over how long the old pod lingers and the round interval.
    #[tokio::test(start_paused = true)]
    async fn a_top_up_round_never_recreates_a_name_up_is_replacing() {
        for linger in [0u64, 5, 15, 25] {
            for secs in [5u64, 28, 30, 60] {
                let case = format!("linger {linger}s, --retry-secs {secs}");
                let fleet = Fleet {
                    linger: Duration::from_secs(linger),
                    ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0), ("10.0.0.3", 22003, 0)])])
                };
                let fake = Arc::new(FakeRemote::new());
                fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
                fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
                fake.script(&host("10.0.0.3", 22003), [FakeReply::ok(), FakeReply::ok(), healthy()]);
                let mut cmd = up_cmd(&[]);
                if let crate::PodCmd::Up { count, retry_mins, retry_secs, .. } = &mut cmd {
                    (*count, *retry_mins, *retry_secs) = (Some(2), 3, secs);
                }
                let err = crate::handle_pods_with(cmd, &fleet, fake.clone(), &cfg(None), true, crate::Interrupt::manual())
                    .await
                    .unwrap_err()
                    .to_string();
                let events = fleet.events();
                let apples = events.iter().filter(|e| e.starts_with("create devtest-apple")).count();
                assert_eq!(apples, 2, "{case}: the first and its one replacement: {events:?}");
                assert!(events.iter().any(|e| e.starts_with("create devtest-bloom")), "{case}: {events:?}");
                assert!(!events.iter().any(|e| e.starts_with("create devtest-cloud")), "{case}: never a name beyond the plan: {events:?}");
                assert_eq!(err, "1 of 2 pod(s) not ready: devtest-bloom (FAILED create)", "{case}");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn keep_trying_waits_for_capacity_for_a_check_replacement_too() {
        // After apple's bad host is terminated its type is out of stock for 60s. With
        // --keep-trying the replacement waits it out (a try every 30s) like the first create
        // would; without it, one try and the name has no pod.
        for keep_trying in [true, false] {
            let fleet = Fleet {
                dry_after_terminate: Duration::from_secs(60),
                ..Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])])
            };
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
            fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
            let mut cmd = up_cmd(&["apple"]);
            if let crate::PodCmd::Up { keep_trying: k, .. } = &mut cmd {
                *k = keep_trying;
            }
            let done = crate::handle_pods_with(cmd, &fleet, fake.clone(), &cfg(None), true, crate::Interrupt::manual()).await;
            let creates = fleet.events().iter().filter(|e| e.starts_with("create")).count();
            if keep_trying {
                done.unwrap();
                assert_eq!(creates, 4, "the first, two dry tries (t=0, 30s), then t=60s: {:?}", fleet.events());
            } else {
                let err = done.unwrap_err().to_string();
                assert_eq!(err, "1 of 1 pod(s) not ready: devtest-apple (FAILED check)");
                assert_eq!(creates, 2, "{:?}", fleet.events());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pod_ready_early_gets_the_fleet_map_of_pods_that_came_up_later() {
        // No proxy layout: the fleet map in ~/.ssh/config is rendered from live endpoints.
        // apple is READY long before bloom has one, so its keys write can't name bloom — the
        // report rewrites apple's map once bloom is listed; bloom's was already complete.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 300)])]);
        let fake = Arc::new(FakeRemote::new());
        let keys = keys_dir("fleet-map");
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, Some(&keys.0)).await;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready));

        let execs = |h: &str| -> Vec<String> {
            fake.calls_to(h).into_iter().filter_map(|c| if let RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None }).collect()
        };
        let apple = execs(&host("10.0.0.1", 22001));
        assert_eq!(apple.len(), 3, "config, keys, map refresh: {apple:#?}");
        assert!(apple[1].contains("OPENAI_API_KEY") && !apple[1].contains("HostName 10.0.0.2"), "{}", apple[1]);
        assert!(!apple[2].contains("OPENAI_API_KEY") && apple[2].contains("HostName 10.0.0.2"), "{}", apple[2]);
        assert!(apple[2].contains("HostName 10.0.0.1"), "{}", apple[2]);
        let bloom = execs(&host("10.0.0.2", 22002));
        assert_eq!(bloom.len(), 2, "config, keys — its map was complete: {bloom:#?}");
        assert!(bloom[1].contains("HostName 10.0.0.1") && bloom[1].contains("HostName 10.0.0.2"), "{}", bloom[1]);
        let (refreshed, _) = lines.find("[devtest-apple] fleet SSH map updated");
        let (bloom_ready, _) = lines.find("[devtest-bloom] READY");
        assert!(bloom_ready < refreshed, "{:#?}", lines.all());
    }

    /// W7 live (A-ckl9): with a backup archive and no key file at all, the keys stage still runs
    /// — each pod gets the archive's access (its block in the one exec, no key exported; the read
    /// key by scp), said as such. Without an archive (or without setup) there is no keys stage.
    #[tokio::test(start_paused = true)]
    async fn the_keys_stage_writes_the_archive_access_without_any_key_file() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        let state = tmp_dir("archive-state");
        let none = tmp_dir("keys-none");
        let cfg = Config::parse(&format!("{}ARENA_STATE_DIR={}\n", cfg_text(None), state.0.display()));
        let dir = none.0.to_string_lossy().into_owned();
        assert!(crate::up_keys_stage(&cfg, &dir, true).is_none(), "no keys, no archive: no stage");
        crate::archive::seed_pod_access(&cfg);
        assert!(crate::up_keys_stage(&cfg, &dir, false).is_none(), "--no-setup: no stage");
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, Some(&none.0)).await;
        assert!(run.keys.is_some());
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert!(rows.iter().all(|r| r.verdict == Verdict::Ready), "{:#?}", lines.all());
        let calls = fake.calls_to(&host("10.0.0.1", 22001));
        assert!(
            calls.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("# BEGIN arena-archive") && !cmd.contains("export "))),
            "{calls:#?}"
        );
        assert!(calls.iter().any(|c| matches!(c, RemoteCall::Copy { remote, .. } if remote == ".ssh/arena_archive_key")), "{calls:#?}");
        lines.find("[devtest-apple] backup archive access ✓ (keys: none generated)");
    }

    /// `--lock` is the last step: a pod that fails its check is replaced (it was never
    /// locked, so it could be terminated), and only the pod that ends READY is locked — once.
    #[tokio::test(start_paused = true)]
    async fn with_lock_only_the_pod_that_ends_ready_is_locked_after_its_check() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 30)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.1", 22001), [FakeReply::ok(), FakeReply::ok(), bad_host()]);
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let mut run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        run.lock = true;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [
                format!("create devtest-apple {A4000}"),
                "terminate id1".to_string(),
                format!("create devtest-apple {R3090}"),
                "lock id2 true".to_string(),
            ]
        );
        lines.find("[devtest-apple] READY after 30s — 1×RTX 3090 COMMUNITY, check pass, locked");
        assert_eq!(rows[0].verdict, Verdict::Ready);
    }

    /// A lock that doesn't take fails the name — the operator asked for a protected pod —
    /// with the pod left running (never terminated over it) and the retry command named.
    #[tokio::test(start_paused = true)]
    async fn a_lock_that_fails_fails_the_name_and_leaves_the_pod_running() {
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 0)])]);
        fleet.lock_refused.lock().unwrap().push("id1".into());
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(None);
        let mut run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        run.lock = true;
        let made = make(&fleet, &["apple", "bloom"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Lock, reason } => {
                assert!(reason.contains("pod not found") && reason.contains("NOT locked"), "{reason}");
                assert!(reason.contains("`arena pods lock devtest-apple`"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(rows[1].verdict, Verdict::Ready, "bloom is locked and ready on its own");
        assert!(!fleet.events().iter().any(|e| e.starts_with("terminate")), "{:?}", fleet.events());
        assert!(fleet.events().contains(&"lock id2 true".to_string()), "{:?}", fleet.events());
        lines.find("[devtest-apple] FAILED lock: ");
        assert!(conclude(&rows, &|_, _| {}).is_err(), "a FAILED lock is a non-zero exit");
    }

    #[test]
    fn the_plan_text_names_every_stage_it_will_run() {
        assert!(pipeline_text(true, None, false, false, true, (5, 2)).contains(
            "provision it, lock it (RunPod then refuses stop/restart/terminate until `arena pods unlock`) — then READY/FAILED"
        ));
        let endpoint = |mins: u64, n: u32| {
            format!(
                "wait for its SSH endpoint — a pod up without one for {mins} min (RunPod: behind its SSH proxy only), or \
                 a RunPod pod with no container at --timeout (10 min or more), is TERMINATED and its name recreated (up \
                 to {n} placement(s) per name)"
            )
        };
        assert_eq!(
            pipeline_text(true, Some(3), true, false, false, (5, 3)),
            format!(
                "per pod, as soon as it can: {}, sync the proxy (if nginx is set up), provision it, \
                 deep-check it — a pod that FAILs on its host is TERMINATED and its name recreated (up to 3 placement(s) \
                 per name; options that haven't failed first; never on a machine that already failed — this run, or an \
                 `up --check` in the last 72h); a software/setup FAIL (e.g. torch missing) is left running, not replaced, \
                 copy its API keys — then READY/FAILED per pod",
                endpoint(5, 3)
            )
        );
        assert_eq!(
            pipeline_text(false, None, true, false, false, (8, 2)),
            format!(
                "per pod, as soon as it can: {}, sync the proxy (if nginx is set up), wait for SSH to answer — then \
                 READY/FAILED per pod",
                endpoint(8, 2)
            )
        );
    }

    #[test]
    fn check_flags_parse() {
        use crate::{Cli, Cmd, PodCmd};
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        match parse(&["arena", "pods", "up", "-n", "2", "--check"]).unwrap() {
            Cmd::Pods(PodCmd::Up { check, check_attempts, .. }) => assert_eq!((check, check_attempts), (true, 2)),
            _ => panic!("not up"),
        }
        // --lock needs the pipeline it ends: refused with --no-wait.
        match parse(&["arena", "pods", "up", "-n", "2", "--lock", "--check"]).unwrap() {
            Cmd::Pods(PodCmd::Up { lock, .. }) => assert!(lock),
            _ => panic!("not up"),
        }
        assert!(parse(&["arena", "pods", "up", "-n", "2", "--lock", "--no-wait"]).is_err());
        match parse(&["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "3"]).unwrap() {
            Cmd::Pods(PodCmd::Up { check_attempts, retry_bad_hosts, .. }) => assert_eq!((check_attempts, retry_bad_hosts), (3, false)),
            _ => panic!("not up"),
        }
        match parse(&["arena", "pods", "up", "-n", "2", "--check", "--retry-bad-hosts"]).unwrap() {
            Cmd::Pods(PodCmd::Up { retry_bad_hosts, .. }) => assert!(retry_bad_hosts),
            _ => panic!("not up"),
        }
        // Without --check too: it bounds replacing a pod that never got an endpoint.
        match parse(&["arena", "pods", "up", "-n", "2", "--check-attempts", "1"]).unwrap() {
            Cmd::Pods(PodCmd::Up { check, check_attempts, .. }) => assert_eq!((check, check_attempts), (false, 1)),
            _ => panic!("not up"),
        }
        for bad in [
            &["arena", "pods", "up", "-n", "2", "--check-attempts", "3", "--no-wait"][..], // nothing to bound
            &["arena", "pods", "up", "-n", "2", "--check", "--no-wait"],                   // checking needs waiting
            &["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "0"],
            &["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "11"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
