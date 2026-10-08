//! `pods up` after the create: one independent pipeline per pod (PLAN 2.B).
//!
//! The batch flow this replaces waited for *every* endpoint, then set the whole fleet up,
//! so one slow pod held up everyone — the ops playbook's "bring a whole fleet up as
//! independent pipelines, never as a batch". Here each pod advances the moment it is
//! individually ready, and says so:
//!
//!   endpoint → proxy sync → setup → [--check: deep check] → API keys → `[name] READY …`
//!
//! or `[name] FAILED <stage>: …` (left running). With `--check` a pod that FAILs the deep
//! check is terminated — and confirmed gone, so a name never has two pods — then its name is
//! recreated from the confirmed options and the new pod runs the pipeline again; one that
//! lands on a machine that already failed is rejected the same way (and terminated even with
//! no attempt left to replace it: it was never set up, and its machine is known bad). Only a
//! check whose script actually ran can condemn a host: one that couldn't run (SSH dropped,
//! timed out) is retried once, then FAILs the name with the pod left running. Ctrl+C stops
//! every pipeline where it is and launches nothing new; nothing is terminated because of it.
//!
//! Concurrency is cooperative, on this one task: the pipelines borrow the fleet provider,
//! so they can't be spawned, and [`join_unordered`] drives them side by side instead —
//! next to one shared endpoint poller (one list call per `--interval` for the whole fleet,
//! as before) and the Ctrl+C watcher. Proxy writes go through one async mutex, so two
//! pipelines never write the proxy config at once. The decisions and the final report are
//! pure, in `arena_core::pipeline`.

use std::collections::HashMap;
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
use arena_core::pod::Maintenance;
use arena_core::provider::{bounded_list, LIST_TIMEOUT};
use arena_core::proxy::Listing;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::setup::{
    key_rejected, looks_unreachable, provision, provision_after_key_repair, provisioning_steps, BootRetry, SetupConfig,
    SetupTimeouts,
};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod, PodSpec, Provider};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{
    copy_keys_command, enrich_best_effort, fleet_ssh_for, judge_deep_call, proxy_sync_now, record_health, repair_pod_keys,
    sync_line, KeySources, Made, ProxySync, COPY_KEYS_TIMEOUT, ENRICH_TIMEOUT,
};

/// How long a pod we terminated (to replace it) may stay listed before we give up on the
/// replacement: creating the new pod while the old one is still listed would put two pods
/// under one name (the proxy merge, `still_needed` and every name lookup assume one).
const GONE_TIMEOUT: Duration = Duration::from_secs(300);

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
}

/// The per-pod part of `up`'s plan, for the dry-run and the confirm prompt.
pub(crate) fn pipeline_text(setup: bool, check: Option<u32>, keys: bool) -> String {
    let mut steps = vec!["wait for its SSH endpoint".to_string(), "sync the proxy (if nginx is set up)".to_string()];
    if setup {
        steps.push("provision it".into());
    }
    if let Some(n) = check {
        steps.push(format!(
            "deep-check it — a pod that FAILs is TERMINATED and its name recreated (up to {n} placement(s) per \
             name; options that haven't failed first; never on a machine IP that already failed)"
        ));
    }
    if setup && keys {
        steps.push("copy its API keys".into());
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
    /// One proxy writer at a time.
    proxy_gate: tokio::sync::Mutex<()>,
    /// Machines ([`health::host_key`]) that failed the deep check this run (any name's: a bad
    /// host breaks every pod on it).
    failed_hosts: Mutex<Vec<String>>,
    /// The provider's pod details for the deep checks, and when they were fetched.
    details: tokio::sync::Mutex<Option<(Instant, Vec<Pod>)>>,
}

/// Bring every pod in `made` up through its own pipeline, concurrently; returns one row per
/// name, in `made`'s order, after a final proxy sync. `interrupt` resolving (Ctrl+C in the
/// CLI) stops every pipeline where it is: no new attempt, no terminate, just the report.
pub(crate) async fn run_up<I>(run: &UpRun<'_>, made: Vec<Made>, interrupt: I, say: Say<'_>) -> Vec<UpRow>
where
    I: Future<Output = ()>,
{
    let (stop_tx, stop) = watch::channel(false);
    let (listing, _) = watch::channel(Arc::new(Vec::new()));
    let shared = Shared {
        run,
        say,
        stop,
        listing,
        waiters: AtomicUsize::new(0),
        proxy_gate: tokio::sync::Mutex::new(()),
        failed_hosts: Mutex::new(Vec::new()),
        details: tokio::sync::Mutex::new(None),
    };
    let shared = &shared;
    let pipelines: Vec<_> =
        made.into_iter().enumerate().map(|(i, m)| async move { (i, shared.pipeline(m).await) }).collect();
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
        names = join_unordered(pipelines) => names,
        () = shared.poll_endpoints() => unreachable!("the endpoint poller never returns"),
    };
    names.sort_by_key(|(i, _)| *i);
    shared.report(names.into_iter().map(|(_, n)| n).collect()).await
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
            Some(None) => {
                let reason = format!(
                    "no SSH endpoint after {}s — left running (`arena proxy apply` once it has one)",
                    self.run.timeout.as_secs()
                );
                return self.end(run, Verdict::Failed { stage: Stage::Endpoint, reason });
            }
            Some(Some(pod)) => pod,
        };
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
            return self.replace(run, &pod, failed_options, why).await;
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
                    // A best-effort step (the VS Code warm-up) that didn't work out: said, not fatal.
                    for w in outcome.warnings() {
                        say(To::Err, &format!("[{name}] setup warning: {w}"));
                    }
                }
            }
        }

        // 4. The deep check: a FAIL is a bad host — replace it while attempts remain.
        if let Some(check) = &self.run.check {
            // Without setup nothing has waited out the boot race yet: a pod whose sshd is still
            // starting must not be judged (unreachable = FAIL) and replaced for it.
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
            let Some(mut h) = self.or_stop(self.deep_check(&name, &pod, &target, check)).await else {
                return self.end(run, Verdict::Stopped { stage: Stage::Check });
            };
            // Only a check whose script ran to the end can condemn the host (terminate it,
            // count its IP against every name). One that couldn't run — the SSH session
            // dropped, or the call timed out, moments after setup reached this very pod —
            // says nothing about the machine ("confirm before alarming"): once SSH answers
            // again it's run once more, and if that can't run either the name FAILs with the
            // pod left running for a look.
            if !script_ran(&h) {
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
            let (status, notes) = (h.status, h.notes());
            run.health = Some(h);
            match status {
                Status::Fail => {
                    if let Some(ip) = &host {
                        self.failed_hosts.lock().unwrap().push(ip.clone());
                    }
                    failed_options.push(option);
                    if let Some(a) = run.attempts.last_mut() {
                        a.end = AttemptEnd::CheckFailed(notes.clone());
                    }
                    say(To::Err, &format!("[{name}] check: FAIL — {notes}"));
                    return self.replace(run, &pod, failed_options, format!("deep check failed: {notes}")).await;
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

        run.ready_at = Some(Instant::now());
        self.end(run, Verdict::Ready)
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
    async fn replace(&self, run: &mut NameRun, pod: &Pod, failed_options: &[usize], why: String) -> Option<(Pod, usize)> {
        let say = self.say;
        let name = run.name.clone();
        let max = self.run.check.as_ref().map_or(1, |c| c.attempts) as usize;
        let n = run.attempts.len();
        let rejected = run.attempts.last().is_some_and(|a| a.end == AttemptEnd::SameHost);
        let last = n >= max;
        if last && !rejected {
            let reason = format!("{why} — no attempts left (--check-attempts {max}); left running");
            return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
        }
        if self.stopped() {
            let reason = format!("{why} — not replaced (interrupted); left running");
            return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
        }
        if last {
            say(To::Err, &format!("[{name}] terminating {} — no attempt left to replace it (--check-attempts {max})", pod.id));
        } else {
            say(To::Err, &format!("[{name}] terminating {} to recreate {name} (attempt {} of {max})", pod.id, n + 1));
        }
        if let Err(e) = self.run.provider.terminate_pod(&pod.id).await {
            let reason = format!("{why}; terminating it failed: {e} — left running");
            return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
        }
        if let Some(a) = run.attempts.last_mut() {
            a.terminated = true;
        }
        run.pod = None;
        run.port = None;
        let gone = self.or_stop(self.wait_gone(pod)).await;
        if last {
            // Waited for all the same, so the final proxy sync doesn't route the name to it.
            let listed = if gone == Some(false) {
                format!(" ({} still listed after {}s: `arena proxy apply` once it's gone)", pod.id, GONE_TIMEOUT.as_secs())
            } else {
                String::new()
            };
            let reason =
                format!("{why} — no attempts left (--check-attempts {max}); terminated it — {name} has no pod now{listed}");
            return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
        }
        match gone {
            None => {
                let reason = format!("{why}; terminated it, then Ctrl+C before its replacement — {name} has no pod now");
                return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
            }
            Some(false) => {
                let reason = format!(
                    "{why}; {} was terminated but is still listed after {}s — no replacement created (it would \
                     duplicate the name)",
                    pod.id,
                    GONE_TIMEOUT.as_secs()
                );
                return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
            }
            Some(true) => {}
        }
        if self.stopped() {
            let reason = format!("{why}; terminated it, then Ctrl+C before its replacement — {name} has no pod now");
            return self.end(run, Verdict::Failed { stage: Stage::Check, reason });
        }
        let order = pipeline::replacement_order(self.run.options.options.len(), failed_options);
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
            placement::place(self.run.provider, &self.run.base, &names, &plan, self.run.rounds, None, interrupt, &mut show)
                .await;
        match (placed.created.first(), placed.log.first().and_then(|l| l.placed)) {
            (Some(new), Some(k)) => Some((new.clone(), order[k])),
            _ => {
                let reason = format!("{why}; its replacement wasn't placed ({}) — {name} has no pod now", unplaced(&placed));
                self.end(run, Verdict::Failed { stage: Stage::Check, reason })
            }
        }
    }

    /// The fleet listing, every `--interval`, while any pipeline waits on an endpoint — one
    /// list call for everyone, however many pods are coming up. Never returns.
    async fn poll_endpoints(&self) {
        let every = self.run.interval.max(Duration::from_secs(1));
        let provider = self.run.provider;
        loop {
            if self.waiters.load(Ordering::SeqCst) > 0 {
                match bounded_list(provider.name(), provider.list_pods(), LIST_TIMEOUT).await {
                    Ok(pods) => {
                        self.listing.send_replace(Arc::new(pods));
                    }
                    Err(e) => (self.say)(To::Err, &format!("  endpoint poll failed ({e}); retrying in {}s", every.as_secs())),
                }
            }
            tokio::time::sleep(every).await;
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
                Ok(o) => format!("exit {:?}: {}", o.code, o.stderr.trim()),
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

    /// The pod `id` once the listing shows it with an SSH endpoint; `None` at `deadline`.
    async fn wait_endpoint(&self, id: &str, deadline: Instant) -> Option<Pod> {
        let _waiting = Waiting::new(&self.waiters);
        let mut rx = self.listing.subscribe();
        loop {
            let found = rx.borrow_and_update().iter().find(|p| p.id == id && p.ssh_ip.is_some() && p.ssh_port.is_some()).cloned();
            if found.is_some() {
                return found;
            }
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return None },
                () = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    /// Whether the terminated `pod` has left its provider's listing (or is listed as
    /// terminated) within [`GONE_TIMEOUT`]. Asked of the backend that owns it, not the fleet
    /// aggregate: that skips a backend that fails to list, and "not in RunPod's listing"
    /// read off a listing RunPod isn't in would create the replacement next to the old pod.
    /// So a failed (or absent) owner listing proves nothing — it just counts as "not yet".
    async fn wait_gone(&self, pod: &Pod) -> bool {
        let every = self.run.interval.max(Duration::from_secs(1));
        let deadline = Instant::now() + GONE_TIMEOUT;
        loop {
            // Per backend, each list bounded (`list_by_provider`'s contract).
            let listings = self.run.provider.list_by_provider().await;
            let owner = match listings.iter().find(|(p, _)| *p == pod.provider) {
                Some(l) => Some(l),
                None => match &listings[..] {
                    [only] => Some(only),
                    _ => None,
                },
            };
            if let Some((_, Ok(pods))) = owner {
                if !pods.iter().any(|p| p.id == pod.id && !p.status.eq_ignore_ascii_case("TERMINATED")) {
                    return true;
                }
            }
            if Instant::now() + every > deadline {
                return false;
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
    /// with the provider's maintenance window looked up alongside.
    async fn deep_check(&self, name: &str, pod: &Pod, target: &SshTarget, check: &CheckStage) -> PodHealth {
        let call = self.run.remote.exec(target, &check.cmd, Some(DEEP_CHECK_TIMEOUT));
        let (maintenance, call) = tokio::join!(self.maintenance_of(name, pod), call);
        let pod = Pod { maintenance, ..pod.clone() };
        judge_deep_call(&pod, call.map_err(|e| describe_error(&e)), &check.policy)
    }

    /// `pod`'s host maintenance window, from the provider's best-effort details: one fetch
    /// for the whole listing, reused for [`DETAILS_TTL`] (refetched sooner only for a pod it
    /// doesn't cover, e.g. a replacement). Never fails a check: no details, no window.
    async fn maintenance_of(&self, name: &str, pod: &Pod) -> Option<Maintenance> {
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
        cache.as_ref().and_then(|(_, pods)| pods.iter().find(|p| p.id == pod.id)).and_then(|p| p.maintenance.clone())
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
        match self.run.remote.exec(target, &copy_keys_command(&vars, &fleet_ssh), Some(COPY_KEYS_TIMEOUT)).await {
            Ok(o) if o.success && matched => Ok((To::Out, format!("[{name}] keys ✓"), Some(fleet_ssh))),
            Ok(o) if o.success => Ok((
                To::Err,
                format!("[{name}] keys ✓ — broadcast tokens only: no per-host key in keys/*_api_keys.csv matched {name}"),
                Some(fleet_ssh),
            )),
            Ok(o) => Err(format!("copying its keys: exit {:?}: {}", o.code, o.stderr.trim())),
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
                        "[{name}] fleet SSH map NOT updated (exit {:?}: {}) — `arena pods copy-keys {name}`",
                        o.code,
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

/// Whether a deep check's script ran start to end — the only kind of check that says
/// anything about the machine. No facts (ssh failed, the call timed out), no header (it
/// never started), or no trailer (the session was cut mid-run) is a check that couldn't run.
fn script_ran(h: &PodHealth) -> bool {
    h.facts.as_ref().is_some_and(|f| f.started && f.complete)
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
                        (p.ssh_ip, p.ssh_port) = (Some(s.ip.clone()), Some(s.port));
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
            if !self.down_after_terminate.is_zero() {
                out.push(("hetzner".into(), Ok(Vec::new())));
            }
            out
        }
        async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
            self.events.lock().unwrap().push(format!("create {} {}", spec.name, spec.gpu_type));
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
                        GPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nVSCODE_PREINSTALL=0\n"
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
        let (fail, _) = lines.find("[devtest-apple] check: FAIL — cuda: RuntimeError: Unexpected error from cudaGetDeviceCount()");
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
        assert!(summary.contains("; left running\ndevtest-apple: FAILED check — deep check failed: cuda: "), "{summary}");
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
        assert!(fake.calls().is_empty(), "--no-setup without --check: nothing over SSH");
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
            dry_run: false,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            check: true,
            check_attempts: 2,
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
            dry_run: false,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            check: true,
            check_attempts: 2,
            timeout: 600,
            interval: 10,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_c_during_the_create_still_stops_the_pipelines() {
        // apple is created on a bad host; bloom finds no capacity, so the create waits to
        // retry — and Ctrl+C lands in that wait. The same press must hold for the pipelines:
        // apple is not set up, checked, terminated or recreated. (A fresh Ctrl+C listener
        // for the pipelines never saw it: apple was checked, terminated and recreated.)
        // Both create paths: the single configured spec, and a `--gpu` option list.
        for gpu in [None, Some("A4000,3090")] {
            let fleet = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0), ("10.0.0.2", 22002, 0)])]);
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
        lines.find("[devtest-apple] check: couldn't run it (ssh: exit Some(255): kex_exchange_identification");
        assert_eq!((rows[0].verdict.clone(), rows[0].health), (Verdict::Ready, Some(Status::Pass)));
        match &rows[1].verdict {
            Verdict::Failed { stage: Stage::Check, reason } => assert!(
                reason.starts_with("couldn't run the deep check twice (ssh: exit Some(255): kex_exchange_identification")
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

    #[test]
    fn the_plan_text_names_every_stage_it_will_run() {
        assert_eq!(
            pipeline_text(true, Some(2), true),
            "per pod, as soon as it can: wait for its SSH endpoint, sync the proxy (if nginx is set up), provision it, \
             deep-check it — a pod that FAILs is TERMINATED and its name recreated (up to 2 placement(s) per name; \
             options that haven't failed first; never on a machine IP that already failed), copy its API keys — then \
             READY/FAILED per pod"
        );
        assert_eq!(
            pipeline_text(false, None, true),
            "per pod, as soon as it can: wait for its SSH endpoint, sync the proxy (if nginx is set up) — then READY/FAILED per pod"
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
        match parse(&["arena", "pods", "up", "-n", "2", "--check", "--check-attempts", "3"]).unwrap() {
            Cmd::Pods(PodCmd::Up { check_attempts, .. }) => assert_eq!(check_attempts, 3),
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
