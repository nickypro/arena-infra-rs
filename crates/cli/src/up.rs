//! `pods up` after the create: one independent pipeline per pod (PLAN 2.B).
//!
//! The batch flow this replaces waited for *every* endpoint, then set the whole fleet up,
//! so one slow pod held up everyone — the ops playbook's "bring a whole fleet up as
//! independent pipelines, never as a batch". Here each pod advances the moment it is
//! individually ready, and says so:
//!
//!   endpoint → proxy sync → setup → [--check: deep check] → API keys → [--lock] → `[name] READY …`
//!
//! or `[name] FAILED <stage>: …` (left running). With `--check` a pod that FAILs the deep
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
use arena_core::snapshot::{self, BadHost};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod, PodSpec, Provider};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{
    copy_keys_command, enrich_best_effort, fleet_ssh_for, judge_deep_call, proxy_sync_now, record_health, repair_pod_keys,
    sync_line, KeySources, Made, ProxySync, COPY_KEYS_TIMEOUT, ENRICH_TIMEOUT,
};

/// How long a pod may be up with no direct SSH endpoint — reachable only through RunPod's
/// SSH proxy ([`Pod::proxy_only`]) — before `up` calls its host bad. Live (2026-10-08) such
/// community hosts never got a public endpoint (15+ minutes); a healthy pod has one within
/// seconds of its container starting. With `--check` the pod is replaced (bounded by
/// `--check-attempts`), else the name FAILs at `endpoint` with the pod left running. The
/// endpoint waits of `replace`/`migrate`/`start`/`reimage` give up on such a pod after it too.
pub(crate) const NO_PUBLIC_IP_GRACE: Duration = Duration::from_secs(300);

/// What waiting for a pod's SSH endpoint came to.
enum EndpointWait {
    /// Listed with an endpoint.
    Ready(Pod),
    /// Up, but only RunPod's SSH proxy reaches it — for [`NO_PUBLIC_IP_GRACE`], or for `since`
    /// when the endpoint deadline came first: a host failure either way (as last listed).
    NoPublicIp { pod: Pod, since: Duration },
    /// The deadline passed with no endpoint, and not up behind the proxy only.
    TimedOut,
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
    /// `--lock`: lock each pod as its last step (checked lockable before the create).
    pub lock: bool,
    /// `--retry-bad-hosts`: ignore the machines earlier runs remember as bad (a machine that
    /// fails again is remembered again; one that passes is forgotten).
    pub retry_bad_hosts: bool,
    /// Where a replacement's price is remembered for the balance, the moment its create
    /// returns — as the create phase does for the first pods (`OnMade`) — so a later listing
    /// with no price (RunPod v1's REST list) still prices it. `None`: not remembered.
    pub balance: Option<&'a crate::money::BalanceCtx>,
}

/// The per-pod part of `up`'s plan, for the dry-run and the confirm prompt.
pub(crate) fn pipeline_text(setup: bool, check: Option<u32>, keys: bool, lock: bool) -> String {
    let mut steps = vec!["wait for its SSH endpoint".to_string(), "sync the proxy (if nginx is set up)".to_string()];
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

        // 1. Its SSH endpoint, within its own deadline.
        let started = Instant::now();
        let pod = match self.or_stop(self.wait_endpoint(&pod.id, started + self.run.timeout)).await {
            None => return self.end(run, Verdict::Stopped { stage: Stage::Endpoint }),
            Some(EndpointWait::TimedOut) => {
                // The wait never trusts one missing listing (a backend that failed to list drops
                // its pods from the aggregate), but the verdict must say how things stand now:
                // ask the owning backend once — a pod terminated elsewhere meanwhile is gone,
                // not "left running" (live G: one deleted out of band read so for 10 minutes).
                let secs = self.run.timeout.as_secs();
                let gone = match self.or_stop(self.run.provider.list_by_provider()).await {
                    Some(listings) => gone_from_owner(&listings, &pod),
                    None => false,
                };
                let reason = if gone {
                    run.pod = None;
                    format!(
                        "no SSH endpoint after {secs}s, and {} no longer lists it (terminated elsewhere?) — nothing \
                         left running",
                        pod.provider
                    )
                } else {
                    format!("no SSH endpoint after {secs}s — left running (`arena proxy apply` once it has one)")
                };
                return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
            }
            Some(EndpointWait::NoPublicIp { pod, since }) => {
                // A host failure, like a failed deep check: nothing can be set up, backed up
                // or forwarded through RunPod's proxy. Replaced under --check (another option
                // first); otherwise the name fails here, the pod left running. Also when the
                // endpoint deadline cut the grace short (a `--timeout` under it): a running
                // container with no direct port is this host's signature, and leaving it
                // FAILED and billing helps no one.
                let why = format!(
                    "{} for {} (its host has no public IP — no scp/rsync/port-forwards)",
                    fleet::PROXY_ONLY,
                    pipeline::fmt_elapsed(since)
                );
                say(To::Err, &format!("[{name}] {why}"));
                if let Some(a) = run.attempts.last_mut() {
                    a.end = AttemptEnd::NoPublicIp;
                }
                if self.run.check.is_none() {
                    let reason = format!(
                        "{why} — left running (`arena pods terminate {name}` and create it again; `up --check` replaces such pods)"
                    );
                    return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
                }
                failed_options.push(option);
                run.pod = Some(pod.clone());
                return self.replace(run, &pod, failed_options, why, Stage::Endpoint).await;
            }
            Some(EndpointWait::Ready(pod)) => pod,
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
                    say(To::Out, &format!("[{name}] setup ✓ ({})", pipeline::fmt_elapsed(t.elapsed())));
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
    /// rejected for its machine is terminated even with no attempt left (unless Ctrl+C): it
    /// was never set up and its host is known bad, so there's nothing to look at — kept, it
    /// would only bill and take the name's proxy port.
    ///
    /// `stage` is where the name fails if it isn't replaced: `check` for a failed deep check
    /// or a rejected machine, `endpoint` for a host with no public endpoint.
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
        let max = self.run.check.as_ref().map_or(1, |c| c.attempts) as usize;
        let n = run.attempts.len();
        let rejected = run.attempts.last().is_some_and(|a| matches!(a.end, AttemptEnd::SameHost | AttemptEnd::KnownBad(_)));
        let last = n >= max;
        if last && !rejected {
            let reason = format!("{why} — no attempts left (--check-attempts {max}); left running");
            return self.end(run, Verdict::Failed { stage, reason });
        }
        if self.stopped() {
            let reason = format!("{why} — not replaced (interrupted); left running");
            return self.end(run, Verdict::Failed { stage, reason });
        }
        if last {
            say(To::Err, &format!("[{name}] terminating {} — no attempt left to replace it (--check-attempts {max})", pod.id));
        } else {
            say(To::Err, &format!("[{name}] terminating {} to recreate {name} (attempt {} of {max})", pod.id, n + 1));
        }
        if let Err(e) = self.run.provider.terminate_pod(&pod.id).await {
            let reason = format!("{why}; terminating it failed: {e} — left running");
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
                _ => format!(" — {name} has no pod now"),
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

    /// The pod `id` once the listing shows it with an SSH endpoint — or, once it has been up
    /// with only RunPod's SSH proxy ([`Pod::proxy_only`]) for [`NO_PUBLIC_IP_GRACE`], that;
    /// or the deadline (still that, if it was up behind the proxy only when it came).
    ///
    /// The grace starts over only when the pod is listed *not* proxy-only. A listing it is
    /// missing from proves nothing: the poller's aggregate listing skips a backend whose list
    /// failed, so one RunPod 5xx/429 drops every RunPod pod from it — and used to restart every
    /// proxy-only pod's grace, turning a replaceable host into `FAILED endpoint`, left billing.
    async fn wait_endpoint(&self, id: &str, deadline: Instant) -> EndpointWait {
        let _waiting = Waiting::new(&self.waiters);
        self.waiter_added.notify_one();
        let mut rx = self.listing.subscribe();
        // When it was first seen up behind the proxy only (this stretch), and as last seen so.
        let mut proxy_only: Option<(Instant, Pod)> = None;
        let at_deadline = |proxy_only: Option<(Instant, Pod)>| match proxy_only {
            Some((since, pod)) => EndpointWait::NoPublicIp { pod, since: since.elapsed() },
            None => EndpointWait::TimedOut,
        };
        loop {
            let listed = rx.borrow_and_update().iter().find(|p| p.id == id).cloned();
            match listed {
                Some(p) if p.ssh_ip.is_some() && p.ssh_port.is_some() => return EndpointWait::Ready(p),
                Some(p) if p.proxy_only => {
                    let since = proxy_only.as_ref().map_or_else(Instant::now, |(t, _)| *t);
                    if since.elapsed() >= NO_PUBLIC_IP_GRACE {
                        return EndpointWait::NoPublicIp { pod: p, since: since.elapsed() };
                    }
                    proxy_only = Some((since, p));
                }
                // Listed, neither reachable nor proxy-only (not up yet, or restarted): over.
                Some(_) => proxy_only = None,
                // Not in this listing: no news — the grace runs on.
                None => {}
            }
            // Wake for the next listing, the deadline, or the end of a grace still running (one
            // already over waits for a listing that shows the pod — or the deadline).
            let grace_end = proxy_only.as_ref().map(|(t, _)| *t + NO_PUBLIC_IP_GRACE).filter(|end| *end > Instant::now());
            let wake = grace_end.map_or(deadline, |end| deadline.min(end));
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return at_deadline(proxy_only) },
                () = tokio::time::sleep_until(wake) => {
                    if Instant::now() >= deadline {
                        return at_deadline(proxy_only);
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
    /// line to print and the fleet-SSH write it made (none if there was nothing to send).
    async fn copy_keys(
        &self,
        name: &str,
        pod: &Pod,
        target: &SshTarget,
        keys: &KeySources,
    ) -> std::result::Result<(To, String, Option<String>), String> {
        let (vars, matched) = keys.vars_for(&pod.name);
        if vars.is_empty() {
            let line = format!("[{name}] keys: none for it (no per-host key matched {name}, no broadcast token set)");
            return Ok((To::Err, line, None));
        }
        let fleet_pods = self.listing.borrow().clone();
        let (fleet_ssh, _) = fleet_ssh_for(self.run.cfg, &fleet_pods);
        let written = self.run.remote.exec(target, &copy_keys_command(&vars, &fleet_ssh), Some(COPY_KEYS_TIMEOUT)).await;
        // The backup archive's read key (when this user has an archive), by scp — as `copy-keys`.
        let archive_note = match (&written, super::archive::pod_archive(self.run.cfg)) {
            (Ok(o), super::archive::PodArchive::Ready(a)) if o.success => {
                super::archive::install_read_key(self.run.remote.as_ref(), target, &a, COPY_KEYS_TIMEOUT).await.err()
            }
            (Ok(o), unreadable @ super::archive::PodArchive::Unreadable(_)) if o.success => unreadable.warning(),
            _ => None,
        };
        if let Some(e) = archive_note {
            return Ok((To::Err, format!("[{name}] keys ✓ — backup archive: {e}"), Some(fleet_ssh)));
        }
        match written {
            Ok(o) if o.success && matched => Ok((To::Out, format!("[{name}] keys ✓"), Some(fleet_ssh))),
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

/// Error text onto one line (it often embeds a command's multi-line stderr).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `pod`'s own backend answered in `listings` (per backend, `list_by_provider`) and
/// lists it no more — or only as terminated. A backend that failed to list, or none that
/// owns it, proves nothing (`false`): the aggregate listing skips a failed backend, and "not
/// in a listing RunPod isn't in" must never read as gone. Pure.
fn gone_from_owner(listings: &[(String, arena_core::Result<Vec<Pod>>)], pod: &Pod) -> bool {
    let owner = match listings.iter().find(|(p, _)| *p == pod.provider) {
        Some(l) => Some(l),
        None => match listings {
            [only] => Some(only),
            _ => None,
        },
    };
    matches!(owner, Some((_, Ok(pods))) if !pods.iter().any(|p| p.id == pod.id && !p.status.eq_ignore_ascii_case("TERMINATED")))
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

    /// A RunPod-like fleet: a create makes a pod with no endpoint, which gets the next
    /// endpoint scripted for its name once that endpoint's delay has passed; a terminated pod
    /// leaves the listing after `linger` (at once by default). Records creates/terminates in
    /// order, counts list calls, and how many `list_by_provider` calls overlap (the proxy
    /// test makes each take `sync_delay`, so overlapping proxy writers would show).
    #[derive(Default)]
    struct Fleet {
        endpoints: Mutex<HashMap<String, VecDeque<(String, u16, Duration)>>>,
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
        /// Each create's `avoid_machines`, in order.
        avoids: Mutex<Vec<Vec<String>>>,
    }

    /// A pod in the fake: its endpoint from `from`; listed until `gone_at`.
    struct Slot {
        pod: Pod,
        from: Instant,
        ip: String,
        port: u16,
        gone_at: Option<Instant>,
    }

    impl Fleet {
        /// `(name, [(ip, port, endpoint after N s)])` — one endpoint per create of that name.
        fn new(script: &[(&str, &[(&str, u16, u64)])]) -> Self {
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
            pods.iter()
                .filter(|s| s.gone_at.is_none_or(|g| now < g))
                .map(|s| {
                    let mut p = s.pod.clone();
                    if now >= s.from {
                        // `proxy` = a host with no public IP: up, behind RunPod's proxy only.
                        if s.ip == "proxy" {
                            p.proxy_only = true;
                        } else {
                            (p.ssh_ip, p.ssh_port) = (Some(s.ip.clone()), Some(s.port));
                        }
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
            Ok(if self.runpod_down() { Vec::new() } else { self.listed() })
        }
        async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
            let now = self.syncing.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_syncing.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.sync_delay).await;
            self.syncing.fetch_sub(1, Ordering::SeqCst);
            let runpod = if self.runpod_down() { Err(Error::provider("runpod: HTTP 502 Bad Gateway")) } else { Ok(self.listed()) };
            let mut out = vec![("runpod".to_string(), runpod)];
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
            pods.push(Slot { pod: pod.clone(), from: Instant::now() + after, ip, port, gone_at: None });
            Ok(pod)
        }
        async fn enrich(&self, pods: &mut [Pod]) -> CoreResult<()> {
            self.enriched.fetch_add(1, Ordering::SeqCst);
            let machines = self.details_machines.lock().unwrap().clone();
            let gpus = self.details_gpus.lock().unwrap().clone();
            for p in pods.iter_mut().filter(|p| p.provider == "runpod") {
                if let Some(m) = p.ssh_port.and_then(|port| machines.get(&port)) {
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
                    pods.push(Slot { pod, from: never, ip: String::new(), port: 0, gone_at: None });
                } else {
                    self.hetzner.lock().unwrap().push(pod);
                }
            }
            if !self.down_after_terminate.is_zero() {
                *self.down_until.lock().unwrap() = Some(now + self.down_after_terminate);
            }
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
            keys: keys.map(|d| KeySources::load(cfg, &d.to_string_lossy(), None, None)).filter(|k| !k.csv.is_empty()),
            lock: false,
            retry_bad_hosts: false,
            balance: None,
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

    /// Live (wave 6): community hosts came up RUNNING with a runtime but no direct SSH — only
    /// RunPod's proxy — for 15+ minutes. After the grace that's a host failure: FAILED
    /// endpoint (left running) without --check; replaced (bounded) with it.
    #[tokio::test(start_paused = true)]
    async fn a_pod_up_behind_the_ssh_proxy_only_is_a_host_failure() {
        // Without --check: the name fails at `endpoint` once the grace is over — not at the
        // 600s endpoint timeout — and nothing is terminated.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        let fake = Arc::new(FakeRemote::new());
        let cfg = cfg(None);
        let run = up_run(&fleet, fake.clone(), &cfg, true, None, None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => {
                assert!(reason.contains("no public endpoint (RunPod ssh proxy only)") && reason.contains("left running"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        let (_, at) = lines.find("[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m");
        assert!(at >= NO_PUBLIC_IP_GRACE && at < Duration::from_secs(330), "{at:?}");
        assert_eq!(fleet.events(), [format!("create devtest-apple {A4000}")], "left running");
        assert!(fake.calls().is_empty(), "nothing to SSH into");

        // With --check: terminated and recreated (another option first); the new pod is READY.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        let made = make(&fleet, &["apple"]).await;
        let lines = Lines::new();
        let rows = run_up(&run, made, std::future::pending(), &|to, l| lines.say(to, l)).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!((rows[0].attempts[0].end.clone(), rows[0].attempts[0].terminated), (AttemptEnd::NoPublicIp, true));
        let summary = pipeline::render_up_summary(&rows);
        assert!(summary.contains("no public SSH endpoint (RunPod ssh proxy only); terminated"), "{summary}");

        // With --check but no attempt left: the name fails at `endpoint`, the pod left running.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        let run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, Some(1), None).await;
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.contains("no attempts left")), "{:?}", rows[0].verdict);
        assert_eq!(fleet.events().len(), 1, "nothing terminated");
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

    /// Review fix: an endpoint deadline shorter than the grace (`--timeout` < 300s) came while
    /// the pod was up behind the proxy only — known to be such a host — and FAILED the name with
    /// the pod left billing, never replaced. It is that host failure: replaced under --check.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_inside_the_proxy_only_grace_is_still_a_host_failure() {
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::ok(), healthy()]);
        let cfg = cfg(None);
        let mut run = up_run(&fleet, fake.clone(), &cfg, true, Some(2), None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert_eq!(
            fleet.events(),
            [format!("create devtest-apple {A4000}"), "terminate id1".to_string(), format!("create devtest-apple {R3090}")]
        );
        assert_eq!(rows[0].verdict, Verdict::Ready);
        assert_eq!(rows[0].attempts[0].end, AttemptEnd::NoPublicIp);

        // Without --check: FAILED endpoint, naming the host's problem, the pod left running.
        let fleet = Fleet::new(&[("apple", &[("proxy", 0, 5)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, None, None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        match &rows[0].verdict {
            Verdict::Failed { stage: Stage::Endpoint, reason } => {
                assert!(reason.contains("no public endpoint (RunPod ssh proxy only)") && reason.contains("left running"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fleet.events().len(), 1, "nothing terminated");

        // A plain timeout (no endpoint, not up behind the proxy) is unchanged: left running.
        let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 500)])]);
        let mut run = up_run(&fleet, Arc::new(FakeRemote::new()), &cfg, true, Some(2), None).await;
        run.timeout = Duration::from_secs(120);
        let made = make(&fleet, &["apple"]).await;
        let rows = run_up(&run, made, std::future::pending(), &|_, _| {}).await;
        assert!(matches!(&rows[0].verdict, Verdict::Failed { stage: Stage::Endpoint, reason } if reason.starts_with("no SSH endpoint after 120s")), "{:?}", rows[0].verdict);
        assert_eq!(fleet.events().len(), 1, "nothing terminated");
    }

    /// Live G: a pod terminated elsewhere while `up` waited for its endpoint ended `FAILED
    /// endpoint — no SSH endpoint after 600s — left running`. A missing listing still proves
    /// nothing during the wait, but at the deadline its own backend is asked: gone is said.
    /// One its backend can't list stays "left running" (unknown is not gone).
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
                    && !reason.contains("left running (`"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(rows[0].gpu, "-", "no pod to describe");

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
                if reason == "no SSH endpoint after 600s — left running (`arena proxy apply` once it has one)"),
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
            cloud: None,
            max_price: None,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
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
            cloud: None,
            max_price: None,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
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
                     (--check-attempts 2); terminated it — devtest-apple has no pod now"
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
        assert!(pipeline_text(true, None, false, true).contains(
            "provision it, lock it (RunPod then refuses stop/restart/terminate until `arena pods unlock`) — then READY/FAILED"
        ));
        assert_eq!(
            pipeline_text(true, Some(2), true, false),
            "per pod, as soon as it can: wait for its SSH endpoint, sync the proxy (if nginx is set up), provision it, \
             deep-check it — a pod that FAILs on its host is TERMINATED and its name recreated (up to 2 placement(s) \
             per name; options that haven't failed first; never on a machine that already failed — this run, or an \
             `up --check` in the last 72h); a software/setup FAIL (e.g. torch missing) is left running, not replaced, \
             copy its API keys — then READY/FAILED per pod"
        );
        assert_eq!(
            pipeline_text(false, None, true, false),
            "per pod, as soon as it can: wait for its SSH endpoint, sync the proxy (if nginx is set up), wait for SSH to \
             answer — then READY/FAILED per pod"
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
        for bad in [
            &["arena", "pods", "up", "-n", "2", "--check-attempts", "3"][..], // needs --check
            &["arena", "pods", "up", "-n", "2", "--check", "--no-wait"],      // checking needs waiting
            &["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "0"],
            &["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "11"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
