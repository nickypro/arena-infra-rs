//! The actions: a menu pick, its confirm, and the action run in the background on one pod or a set
//! ([`execute`]: the only place the dashboard changes a pod), a stop's plan, the CLI's actions as runs.

use std::sync::{Arc, Mutex};

use arena_core::capability::Op;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::remote::Remote;
use arena_core::{snapshot, Config, Pod};
use tokio::task::JoinSet;

use crate::refresh::{claude_path, spend_path};
use crate::ssh_actions::{build_preview, run_backup, run_set_branch, run_setup, run_ssh_oneline, run_ssh_output, TEST_TIMEOUT, TORCH_TEST_CMD};
use crate::state::{marked_pods, Action, Confirm};
use crate::{aikey, runs, selected_pod, InputScope, Mode, Scope, Shared, Ui};

/// Run a pod/fleet action off the UI thread: mark busy, spawn it, and stash the result
/// in `Shared` for the loop to pick up. This keeps the dashboard live during slow SSH
/// work (a `setup`/`backup` across the fleet takes many seconds) instead of freezing.
pub(crate) fn start_action<F>(shared: &Arc<Mutex<Shared>>, task: F)
where
    F: std::future::Future<Output = String> + Send + 'static,
{
    {
        let mut s = shared.lock().unwrap();
        s.action_running = true;
        s.action_result = None;
    }
    let shared = shared.clone();
    tokio::spawn(async move {
        let msg = task.await;
        let mut s = shared.lock().unwrap();
        s.action_result = Some(msg);
        s.action_running = false;
    });
}

/// Resolve a [`Scope`] to the pods it targets, from the live list + the marked set.
pub(crate) fn scope_pods(
    shared: &Arc<Mutex<Shared>>,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
) -> Vec<Pod> {
    let s = shared.lock().unwrap();
    match scope {
        Scope::All => s.pods.clone(),
        Scope::Marked => marked_pods(&s.pods, marked).into_iter().cloned().collect(),
    }
}

/// Run an action against every pod concurrently, returning a summary line, then each pod's
/// outcome. Used by the fleet menu (backup / setup / test / run / set-branch, and restart /
/// stop / terminate on a marked set). A stop acts only on the pods [`stop_plan`] stops, then
/// names the ones it skipped — and ends in a ✗ while one of them still bills, as `pods stop`
/// exits non-zero then.
pub(crate) async fn execute_fleet(
    provider: &Arc<dyn Provider>,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    action: Action,
    pods: Vec<Pod>,
    arg: Option<String>,
) -> String {
    let (pods, skipped) = match action {
        Action::Stop => {
            let plan = stop_plan(provider.as_ref(), pods);
            let mut skipped = plan.skip_lines();
            skipped.extend(plan.still_billing());
            (plan.stop, skipped)
        }
        _ => (pods, Vec::new()),
    };
    let total = pods.len();
    let mut set = JoinSet::new();
    for pod in pods {
        let provider = provider.clone();
        let remote = remote.clone();
        let cfg = cfg.clone();
        let arg = arg.clone();
        set.spawn(async move { execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, arg.as_deref()).await });
    }
    let mut ok = 0usize;
    let mut lines: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(msg) = joined {
            if msg.starts_with('✓') {
                ok += 1;
            }
            lines.push(msg);
        }
    }
    // Show EVERY pod's line (sorted), not just failures — so e.g. `test` shows each
    // pod's torch version. The result modal scrolls if it's taller than the screen.
    lines.sort();
    let header = format!("{} — {ok}/{total} ok", action.label());
    std::iter::once(header).chain(lines).chain(skipped).collect::<Vec<_>>().join("\n")
}

/// What a multi-pod `stop` does with each pod — `pods stop`'s rules, through the same core
/// helpers: only a pod that bills ([`arena_core::status::is_billing`]: running or starting —
/// an allow-list, so a state nobody knows never gets a pod stopped) is stopped; the rest are
/// said and left alone. A billing pod whose backend has no stop at all
/// ([`Provider::supports`] — Shadeform: it bills until it's deleted) is skipped and named,
/// and the others still stop (review SF3: one overflow VM must not veto the rest). A locked
/// pod is refused by [`execute`], as for one pod.
struct StopPlan {
    stop: Vec<Pod>,
    /// Billing pods whose backend can't stop them, each with why: they still bill.
    no_stop: Vec<(Pod, String)>,
    /// Pods that aren't billing (already stopped, a state nobody knows).
    idle: Vec<Pod>,
}

/// Sort `pods` for a stop ([`StopPlan`]). Pure: no request.
fn stop_plan(provider: &dyn Provider, pods: Vec<Pod>) -> StopPlan {
    let mut plan = StopPlan { stop: Vec::new(), no_stop: Vec::new(), idle: Vec::new() };
    for p in pods {
        if !arena_core::status::is_billing(&p.status) {
            plan.idle.push(p);
        } else if let Some(why) = provider.supports(Op::Stop, Some(&p)).refusal() {
            plan.no_stop.push((p, why));
        } else {
            plan.stop.push(p);
        }
    }
    plan
}

impl StopPlan {
    /// Why there is nothing to confirm: every billing pod has no stop — refused, as `pods stop`
    /// refuses such a selection — or nothing bills at all.
    fn refusal(&self) -> Option<String> {
        if !self.stop.is_empty() {
            return None;
        }
        Some(match self.no_stop.first() {
            Some((_, why)) => {
                let names: Vec<&str> = self.no_stop.iter().map(|(p, _)| p.name.as_str()).collect();
                format!("✗ can't stop {} — {why}", names.join(", "))
            }
            None => "⊘ nothing to stop — no running or starting pod among them".into(),
        })
    }

    /// One line per pod left alone, and why — for the confirm and the result.
    fn skip_lines(&self) -> Vec<String> {
        let idle = self.idle.iter().map(|p| format!("⊘ {} — skipped: not running or starting ({})", p.name, p.status));
        let no_stop = self.no_stop.iter().map(|(p, why)| format!("⊘ {} — skipped, still billing: {why}", p.name));
        idle.chain(no_stop).collect()
    }

    /// The closing ✗ while a skipped pod still bills (`pods stop` exits non-zero then): what
    /// was asked isn't done while one of them bills.
    fn still_billing(&self) -> Option<String> {
        (!self.no_stop.is_empty()).then(|| {
            let names: Vec<&str> = self.no_stop.iter().map(|(p, _)| p.name.as_str()).collect();
            format!(
                "✗ {} pod(s) still billing — their provider has no stop: {} (back up, then `arena pods terminate`)",
                names.len(),
                names.join(", ")
            )
        })
    }
}

/// A menu pick for the cursor pod: run/set-branch collect an argument first; everything
/// else goes through a confirm modal. Shared by the arrow-Enter and letter-key paths. An action
/// the pod's provider can't do — the capability table's answer (`Provider::supports`), the one
/// the CLI refuses on, and why the menu greys the row out — is refused here, before a confirm or
/// a new name is asked for: a stop of a Shadeform pod (no stop: the confirm's "keeps no data"
/// warning would only mislead), say.
pub(crate) fn choose_pod_action(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, action: Action) {
    let Some(pod) = selected_pod(shared, ui.selected) else {
        ui.mode = Mode::List;
        return;
    };
    let shown = ui.shown_name(&pod.name);
    if action.ai_key() {
        return aikey::choose(shared, ui, pod, shown, action);
    }
    if let Some(why) = action.op().and_then(|op| provider.supports(op, Some(&pod)).refusal()) {
        ui.mode = Mode::Result(format!("✗ can't {} {} — {why}", action.label(), pod.name));
        return;
    }
    match action {
        // Read-only: straight to the runs panel.
        Action::Jobs | Action::Logs => return start_cli_action(ui, shared, action, &pod, None),
        Action::Start if arena_core::status::is_billing(&pod.status) => {
            ui.mode = Mode::Result(format!("✗ {} is {} — start is for a stopped pod", pod.name, pod.status));
            return;
        }
        _ => {}
    }
    ui.mode = if action.needs_input() {
        Mode::Input { action, scope: InputScope::Pod { name: shown, id: pod.id }, value: String::new() }
    } else {
        let preview = build_preview(&ui.cfg, action, &pod);
        let wipes_disk = provider.restart_wipes_container_disk(&pod);
        let reset = provider.restart_resets_container(&pod);
        Mode::Confirm(Confirm { wipes_disk, reset, ..Confirm::new(action, shown, pod.id, preview) })
    };
}

/// A menu pick for a multi-pod scope: `test` runs right away (read-only); run/set-branch
/// collect an argument; the rest go through the typed-token confirm. A stop first sorts its
/// pods as `pods stop` does ([`stop_plan`]): one with nothing to stop is refused here, and the
/// confirm names what it will skip.
pub(crate) fn choose_fleet_action(
    shared: &Arc<Mutex<Shared>>,
    ui: &mut Ui,
    provider: &Arc<dyn Provider>,
    action: Action,
    scope: Scope,
) {
    match action {
        _ if action.ai_key() => aikey::choose_set(shared, ui, action, scope),
        Action::Stop => {
            let plan = stop_plan(provider.as_ref(), scope_pods(shared, &ui.marked, scope));
            ui.mode = match plan.refusal() {
                Some(r) => Mode::Result(r),
                None => Mode::FleetConfirm { action, typed: String::new(), scope, note: plan.skip_lines().join("\n") },
            };
        }
        Action::Test => {
            let pods = scope_pods(shared, &ui.marked, scope);
            let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
            ui.mode = Mode::Working(format!("testing torch on {} pod(s)…", pods.len()));
            start_action(shared, async move {
                execute_fleet(&provider, &remote, &cfg, Action::Test, pods, None).await
            });
        }
        Action::Run | Action::SetBranch => {
            ui.mode = Mode::Input { action, scope: InputScope::Set(scope), value: String::new() };
        }
        _ => {
            // A restart or terminate of a set with a pod its provider can't do it for (none can't
            // today) is refused whole, naming each — as the CLI refuses it.
            let cant: Vec<String> = match action.op() {
                Some(op) => scope_pods(shared, &ui.marked, scope)
                    .iter()
                    .filter_map(|p| provider.supports(op, Some(p)).refusal().map(|why| format!("{} — {why}", p.name)))
                    .collect(),
                None => Vec::new(),
            };
            ui.mode = if cant.is_empty() {
                Mode::FleetConfirm { action, typed: String::new(), scope, note: String::new() }
            } else {
                Mode::Result(format!("✗ can't {}:\n{}", action.label(), cant.join("\n")))
            };
        }
    }
}

/// Start a confirmed single-pod action **in the background** (so a slow SSH op doesn't
/// freeze the dashboard): show a busy modal and spawn the work, whose outcome the run
/// loop later swaps into a result modal. The pod is looked up fresh by id.
pub(crate) fn apply_action(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, c: &Confirm) {
    let Some(pod) = shared.lock().unwrap().pods.iter().find(|p| p.id == c.pod_id).cloned() else {
        ui.mode = Mode::Result(format!("{} is gone — refresh", c.pod_name));
        return;
    };
    if c.action.ai_key() {
        // The key as the confirm showed it (review codex r3 #1), not as read since.
        return aikey::start(ui, shared, c.action, &c.ai, Some(&pod.provider), None);
    }
    if c.action.cli() {
        return start_cli_action(ui, shared, c.action, &pod, None);
    }
    ui.mode = Mode::Working(format!("{} {}…", c.action.doing(), pod.name));
    let (provider, remote, cfg, action) = (provider.clone(), ui.remote.clone(), ui.cfg.clone(), c.action);
    start_action(shared, async move {
        execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, None).await
    });
}

/// Record a stop or terminate the dashboard just made in the spend ledger (quietly).
fn record_spend(cfg: &Config, ev: &arena_core::spend::Event) {
    let now = snapshot::unix_now();
    arena_core::spend::record_quietly(spend_path(cfg).as_deref(), now, |s| arena_core::spend::apply_event(s, ev, now));
}

/// Perform one action against a pod, returning a one-line human-readable outcome.
/// This is the *only* place the dashboard mutates anything. `arg` carries the typed
/// command (`Run`) or branch (`SetBranch`); it's `None` for the other actions.
///
/// A pod the listing reports locked ([`arena_core::lock`]) is refused restart, stop and
/// terminate here, before any call — as RunPod itself would refuse them, and as the CLI does;
/// a refusal the listing didn't predict reads the same way ([`arena_core::lock::explain`]).
/// The dashboard never unlocks: that's `arena pods unlock`, on purpose.
pub(crate) async fn execute(
    provider: &dyn Provider,
    remote: &dyn Remote,
    cfg: &Config,
    action: Action,
    pod: &Pod,
    arg: Option<&str>,
) -> String {
    use arena_core::lock;
    let name = &pod.name;
    if matches!(action, Action::Restart | Action::Stop | Action::Terminate) && lock::is_locked(pod) {
        return format!("✗ {} {name} refused: {}", action.label(), lock::unlock_hint(&[name]));
    }
    match action {
        // Only a backend known to reset on restart (RunPod v2) is said to have; one that may
        // not have (RunPod v1, live 2026-10-09) gets the same advice, without the claim.
        Action::Restart => match provider.restart_pod(&pod.id).await {
            Ok(()) => match provider.restart_resets_container(pod) {
                ContainerReset::Yes => format!("✓ restarted {name} — reset to the image: run setup (p) once it's up"),
                ContainerReset::Unknown => {
                    format!("✓ restarted {name} — it may have been reset to the image: run setup (p) once it's up")
                }
                ContainerReset::No => format!("✓ restarted {name}"),
            },
            Err(e) => format!("✗ restart {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Stop => match provider.stop_pod_outcome(&pod.id).await {
            Ok(outcome) => {
                record_spend(cfg, &arena_core::spend::Event::Stopped { provider: Some(&pod.provider), id: &pod.id });
                match outcome.note() {
                    Some(note) => format!("✓ stopped {name} — ⚠ {note}"),
                    None => format!("✓ stopped {name}"),
                }
            }
            Err(e) => format!("✗ stop {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Terminate => match provider.terminate_pod(&pod.id).await {
            Ok(()) => {
                record_spend(cfg, &arena_core::spend::Event::Terminated { provider: Some(&pod.provider), id: &pod.id });
                arena_core::claude::record_terminated(claude_path(cfg).as_deref(), Some(&pod.provider), &pod.id);
                format!("✓ terminated {name}")
            }
            Err(e) => format!("✗ terminate {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Backup => run_backup(remote, cfg, pod).await,
        Action::Setup => run_setup(remote, cfg, pod).await,
        Action::Test => run_ssh_oneline(remote, cfg, pod, TORCH_TEST_CMD, "torch", TEST_TIMEOUT).await,
        Action::Run => match arg {
            Some(cmd) if !cmd.trim().is_empty() => run_ssh_output(remote, cfg, pod, cmd).await,
            _ => format!("✗ {name}: no command given"),
        },
        Action::SetBranch => match arg {
            Some(branch) if !branch.trim().is_empty() => run_set_branch(remote, cfg, pod, branch.trim()).await,
            _ => format!("✗ {name}: no branch given"),
        },
        // Runs of the CLI (`start_cli_action`), never an action of this one.
        Action::Start
        | Action::Rename
        | Action::Claude
        | Action::Jobs
        | Action::Logs
        | Action::Pull
        | Action::KeyLimit
        | Action::KeyPause
        | Action::KeyResume
        | Action::KeyRotate => {
            format!("✗ {} {name}: runs as `arena {}`", action.label(), action.command())
        }
    }
}

/// The `arena` command for one of the CLI's actions on `name` (`arg`: rename's new name): `jobs`
/// and `logs` read; `start` and `rename` change the pod, after the dashboard's confirm or input
/// (`--yes`). `None`: not one of them. Pure.
pub(crate) fn cli_action_args(action: Action, name: &str, arg: Option<&str>) -> Option<(runs::Kind, Vec<String>)> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Some(match action {
        Action::Start => (runs::Kind::Start, v(&["pods", "start", name, "--yes"])),
        Action::Rename => (runs::Kind::Rename, v(&["pods", "rename", name, arg?.trim(), "--yes"])),
        // `spread` (any case) is the flag; anything else the account, as typed (the CLI says what
        // the pool has when it's none of them).
        Action::Claude => match arg?.trim() {
            "" => return None,
            a if a.eq_ignore_ascii_case("spread") => (runs::Kind::Claude, v(&["claude", "assign", name, "--spread", "--yes"])),
            a => (runs::Kind::Claude, v(&["claude", "assign", name, a, "--yes"])),
        },
        Action::Jobs => (runs::Kind::Jobs, v(&["pods", "jobs", name])),
        Action::Logs => (runs::Kind::Logs, v(&["pods", "logs", name, "--tail", "100"])),
        Action::Pull => (runs::Kind::Pull, v(&["pods", "pull", name, "--yes"])),
        _ => return None,
    })
}

/// Run one of the CLI's actions on `pod` in the background (`runs.rs`) and open the runs panel
/// on it — or say why it can't run.
pub(crate) fn start_cli_action(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, action: Action, pod: &Pod, arg: Option<&str>) {
    if action.ai_key() {
        let keyed = aikey::keyed(&aikey::snapshot(&shared.lock().unwrap(), std::slice::from_ref(pod)));
        return aikey::start(ui, shared, action, &keyed, Some(&pod.provider), arg);
    }
    let Some((kind, args)) = cli_action_args(action, &pod.name, arg) else { return };
    let title = format!("{} {}", action.command(), ui.shown_name(&pod.name));
    // The pod's own provider: the CLI finds a pod fleet-wide whatever `--provider` says, but its
    // messages then name the right one.
    let provider = if pod.provider.is_empty() { ui.provider_name.clone() } else { pod.provider.clone() };
    ui.mode = match runs::start(ui, shared, kind, &provider, args, title) {
        Ok(id) => Mode::Runs { id, scroll: None, stop: false },
        Err(e) => Mode::Result(format!("✗ {e}")),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use arena_core::openrouter::UsageView;
    use arena_core::remote::{FakeRemote, FakeReply};
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use arena_core::PodSpec;
    use crate::refresh::{backend_refusals, backend_resets};
    use crate::state::{dashboard_snapshot, mark_key};
    use crate::submit_input;
    use crate::tests::{cfg, draw, fixture_shared, pod, ui, FakeFleet, NOW};

    /// The dashboard refuses restart/stop/terminate on a pod the listing reports locked —
    /// before any call (this fleet panics on one) — saying how to unlock it; a refusal the
    /// listing didn't predict (the provider's `Locked` error) reads the same way.
    #[tokio::test]
    async fn lifecycle_actions_on_a_locked_pod_are_refused_with_the_unlock_hint() {
        let fleet = FakeFleet { pods: vec![], lists: AtomicUsize::new(0), details: AtomicUsize::new(0), details_error: None };
        let remote = FakeRemote::new();
        let cfg = cfg("");
        let mut bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        bravo.locked = Some(true);
        for action in [Action::Restart, Action::Stop, Action::Terminate] {
            let out = execute(&fleet, &remote, &cfg, action, &bravo, None).await;
            assert_eq!(
                out,
                format!("✗ {} devtest-bravo refused: devtest-bravo is locked — `arena pods unlock devtest-bravo` first", action.label())
            );
        }

        /// Locked since it was listed: the provider refuses, tagged `Locked`.
        struct Refuses;
        #[async_trait::async_trait]
        impl Provider for Refuses {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            /// The recorded live refusal, through the v2 backend's own classification.
            async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
                Err(arena_core::provider::runpod_v2::recorded::locked_refusal("stop pod"))
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
        }
        bravo.locked = Some(false);
        let out = execute(&Refuses, &remote, &cfg, Action::Stop, &bravo, None).await;
        assert_eq!(out, "✗ stop devtest-bravo failed: devtest-bravo is locked — `arena pods unlock devtest-bravo` first");
        assert_eq!(execute(&Refuses, &remote, &cfg, Action::Terminate, &bravo, None).await, "✓ terminated devtest-bravo");
    }

    /// The result line says only what the backend knows: "reset to the image" after a restart
    /// that resets (RunPod v2), "may have been" after one that may not (RunPod v1, live
    /// 2026-10-09: marker files survived), nothing for a kept disk — and a Hetzner stop that
    /// had to be forced says so.
    #[tokio::test]
    async fn restart_and_stop_lines_say_only_what_happened() {
        use arena_core::provider::StopOutcome;
        struct Says(ContainerReset, StopOutcome);
        #[async_trait::async_trait]
        impl Provider for Says {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!("the dashboard asks for the stop's outcome")
            }
            async fn stop_pod_outcome(&self, _id: &str) -> arena_core::Result<StopOutcome> {
                Ok(self.1)
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            fn restart_resets_container(&self, _pod: &Pod) -> ContainerReset {
                self.0
            }
        }
        let (remote, cfg) = (FakeRemote::new(), cfg(""));
        let bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        for (reset, want) in [
            (ContainerReset::Yes, "✓ restarted devtest-bravo — reset to the image: run setup (p) once it's up"),
            (ContainerReset::Unknown, "✓ restarted devtest-bravo — it may have been reset to the image: run setup (p) once it's up"),
            (ContainerReset::No, "✓ restarted devtest-bravo"),
        ] {
            let p = Says(reset, StopOutcome::Stopped);
            assert_eq!(execute(&p, &remote, &cfg, Action::Restart, &bravo, None).await, want, "{reset:?}");
        }
        let plain = Says(ContainerReset::No, StopOutcome::Stopped);
        assert_eq!(execute(&plain, &remote, &cfg, Action::Stop, &bravo, None).await, "✓ stopped devtest-bravo");
        let forced = Says(ContainerReset::No, StopOutcome::ForcedOff { waited: Duration::from_secs(90) });
        let out = execute(&forced, &remote, &cfg, Action::Stop, &bravo, None).await;
        assert!(out.starts_with("✓ stopped devtest-bravo — ⚠ it didn't shut down within 90s, so it was powered off"), "{out}");
    }

    /// A provider for the action tests that never answers a fleet call it isn't asked.
    fn quiet_fleet() -> FakeFleet {
        FakeFleet { pods: vec![], lists: AtomicUsize::new(0), details: AtomicUsize::new(0), details_error: None }
    }

    /// `run` shows what the command printed — every line, not the last one (live: `nvidia-smi
    /// -L; hostname` read as just the hostname) — with stderr when it fails, as a terminal would
    /// show it, and past RUN_OUTPUT_LINES the earlier lines counted; per pod for several pods.
    /// `test` stays its one-liner (`actions_go_through_remote_with_their_budgets`).
    #[tokio::test]
    async fn run_shows_the_whole_output_per_pod() {
        let cfg = cfg("");
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let hundred: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        // (the reply, the outcome)
        let cases: Vec<(FakeReply, String)> = vec![
            (
                FakeReply::stdout("GPU 0: NVIDIA RTX A4000 (UUID: GPU-1)\nGPU 1: NVIDIA RTX A4000 (UUID: GPU-2)\nalpha-host\n"),
                "✓ devtest-alpha run:\n  GPU 0: NVIDIA RTX A4000 (UUID: GPU-1)\n  GPU 1: NVIDIA RTX A4000 (UUID: GPU-2)\n  alpha-host".into(),
            ),
            (FakeReply::stdout("alpha-host\n"), "✓ devtest-alpha run: alpha-host".into()),
            (FakeReply::ok(), "✓ devtest-alpha run: (no output)".into()),
            (
                FakeReply::exit(2, "ls: cannot access 'nope': No such file or directory\n").with_stdout("found:\n"),
                "✗ devtest-alpha run (exit 2):\n  found:\n  ls: cannot access 'nope': No such file or directory".into(),
            ),
            (FakeReply::stdout("\u{1b}[32mok\u{1b}[0m\n 10%\r100%\n\n"), "✓ devtest-alpha run:\n  ok\n  100%".into()),
            (
                FakeReply::stdout(&hundred),
                format!(
                    "✓ devtest-alpha run:\n  … 60 earlier line(s) not shown\n{}",
                    (61..=100).map(|i| format!("  line {i}")).collect::<Vec<_>>().join("\n")
                ),
            ),
        ];
        for (reply, want) in cases {
            let fake = FakeRemote::new();
            fake.script("10.0.0.1:22001", [reply]);
            assert_eq!(execute(&quiet_fleet(), &fake, &cfg, Action::Run, &alpha, Some("nvidia-smi -L; hostname")).await, want);
        }
        // Several pods: each pod's output under its own line.
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout("alpha-host\nup 3 days\n")]);
        fake.script("10.0.0.2:22002", [FakeReply::exit(127, "sh: uptim: not found\n")]);
        let (provider, remote): (Arc<dyn Provider>, Arc<dyn Remote>) = (Arc::new(quiet_fleet()), fake.clone());
        let pods = vec![alpha.clone(), pod("bravo", "runpod", "rp2", 2, Some(22002))];
        let out = execute_fleet(&provider, &remote, &cfg, Action::Run, pods, Some("hostname; uptim".into())).await;
        assert_eq!(out, "run — 1/2 ok\n✓ devtest-alpha run:\n  alpha-host\n  up 3 days\n✗ devtest-bravo run (exit 127): sh: uptim: not found");
    }

    /// The menus' restart label says what each pod's backend says a restart does to its disk —
    /// live: RunPod v2's wiped it, a Hetzner VM keeps it, RunPod v1 (seen keeping it) and Vast
    /// aren't known to — from the real fleet's answers, as the refresh records them; a marked
    /// set's menu says the worst of its pods'. It used to say "WIPES" for every pod. The confirm
    /// is as strict as before: the name typed back, and a ⚠ wherever the backend may wipe.
    /// A config without `RUNPOD_API` ("") is v2's: its RunPod pods say WIPES.
    #[test]
    fn the_restart_label_follows_each_pods_backend() {
        let fleet = |api: &str| -> Arc<dyn Provider> {
            let line = if api.is_empty() { String::new() } else { format!("RUNPOD_API={api}\n") };
            let keys = format!("RUNPOD_API_KEY=r\n{line}VAST_API_KEY=v\nHETZNER_API_KEY=h\n");
            Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse(&keys), false).unwrap())
        };
        let (rp, va, hz) = (pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("bravo", "vast", "va1", 2, Some(22002)), pod("charlie", "hetzner", "hz1", 3, Some(22)));
        let pods = [rp.clone(), va.clone(), hz.clone()];
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let at = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // Nothing recorded yet: never "keeps", never "WIPES".
        ui.mode = Mode::Menu { sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("restart — may wipe the container disk"));
        for (api, runpod) in [("v2", "WIPES the container disk"), ("v1", "may wipe the container disk"), ("", "WIPES the container disk")] {
            let f = fleet(api);
            shared.lock().unwrap().resets = backend_resets(f.as_ref(), &pods);
            for (id, want) in [("rp1", runpod), ("va1", "may wipe the container disk"), ("hz1", "keeps the disk")] {
                (ui.selected, ui.mode) = (at(id), Mode::Menu { sel: 0 });
                let text = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
                assert!(text.contains(&format!("[r] restart    restart — {want}")), "{api} {id}: {text}");
            }
            for (marked, want) in [(vec![&hz], "keeps the disk"), (vec![&hz, &va], "may wipe the container disk"), (vec![&hz, &va, &rp], runpod)] {
                ui.marked = marked.iter().map(|p| mark_key(p)).collect();
                ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
                let text = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
                assert!(text.contains(&format!("restart — {want}")), "{api} {marked:?}: {text}");
            }
            ui.marked.clear();
            // The single-pod confirm: the name typed back whatever the answer; the warning where
            // the backend may wipe, worded by its answer.
            for (id, warn) in [("rp1", if api == "v1" { "⚠ MAY WIPE THE CONTAINER DISK" } else { "⚠ WIPES THE CONTAINER DISK" }), ("va1", "⚠ MAY WIPE"), ("hz1", "the disk is kept")] {
                (ui.selected, ui.mode) = (at(id), Mode::List);
                choose_pod_action(&shared, &mut ui, &f, Action::Restart);
                let Mode::Confirm(c) = &ui.mode else { panic!("{:?}", ui.mode) };
                assert!(c.action.requires_typed_name() && !c.is_satisfied(), "{api} {id}");
                assert!(c.warning().is_some_and(|w| w.contains(warn)), "{api} {id}: {:?}", c.warning());
            }
        }
    }

    /// A multi-pod stop sorts its pods as `pods stop` does, through the core helpers
    /// (`status::is_billing`, and `Provider::supports` — here the real fleet's, where the
    /// capability table says Shadeform has no stop): a billing pod with no stop is skipped and
    /// named, a pod not running is left alone, the rest are stopped; a selection of only no-stop
    /// pods is refused, as `pods stop` refuses it — and one pod's stop of such a pod is refused
    /// before its confirm, its menu row greyed out with why. The marked-set menu has stop; the
    /// whole fleet's doesn't.
    #[test]
    fn a_marked_stop_skips_what_it_cannot_stop_and_refuses_it_alone() {
        let no_stop = arena_core::capability::support(arena_core::capability::Backend::Shadeform, Op::Stop).refusal().unwrap();
        let fleet: Arc<dyn Provider> =
            Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k"), false).unwrap());
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let mayor = pod("mayor", "shadeform", "sf1", 5, Some(22));
        let mut delta = pod("delta", "runpod", "rp4", 4, None);
        delta.status = "EXITED".into();
        let plan = stop_plan(fleet.as_ref(), vec![alpha.clone(), mayor.clone(), delta.clone()]);
        assert_eq!(plan.stop.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha"]);
        assert_eq!(plan.refusal(), None);
        assert_eq!(
            plan.skip_lines(),
            [
                "⊘ devtest-delta — skipped: not running or starting (EXITED)".to_string(),
                format!("⊘ devtest-mayor — skipped, still billing: {no_stop}"),
            ]
        );
        assert!(plan.still_billing().unwrap().starts_with("✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor"));
        let alone = stop_plan(fleet.as_ref(), vec![mayor.clone(), delta.clone()]);
        assert_eq!(alone.refusal().unwrap(), format!("✗ can't stop devtest-mayor — {no_stop}"));
        assert!(stop_plan(fleet.as_ref(), vec![delta.clone()]).refusal().unwrap().starts_with("⊘ nothing to stop"));

        // Through the menu picks, as the key loop calls them.
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&[alpha.clone(), mayor.clone(), delta.clone()], &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.marked = [&alpha, &mayor, &delta].iter().map(|p| mark_key(p)).collect();
        choose_fleet_action(&shared, &mut ui, &fleet, Action::Stop, Scope::Marked);
        let Mode::FleetConfirm { action: Action::Stop, ref note, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert!(note.contains("devtest-mayor — skipped, still billing: Shadeform has no stop") && note.contains("devtest-delta — skipped"), "{note}");
        let all = draw(180, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(all.contains("skipped, still billing") && all.contains("Type ALL to confirm"), "{all}");
        // Only the Shadeform pod: refused, nothing to confirm.
        ui.marked = [mark_key(&mayor)].into();
        choose_fleet_action(&shared, &mut ui, &fleet, Action::Stop, Scope::Marked);
        assert!(matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't stop devtest-mayor — Shadeform has no stop")), "{:?}", ui.mode);
        // One pod's stop: refused before the confirm for it, the confirm for another.
        let row = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        ui.selected = row("sf1");
        choose_pod_action(&shared, &mut ui, &fleet, Action::Stop);
        assert!(matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't stop devtest-mayor — Shadeform has no stop")), "{:?}", ui.mode);
        ui.selected = row("rp1");
        choose_pod_action(&shared, &mut ui, &fleet, Action::Stop);
        assert!(matches!(&ui.mode, Mode::Confirm(c) if c.action == Action::Stop), "{:?}", ui.mode);
        // The Shadeform pod's menu greys out stop and start, saying why; the RunPod pod's doesn't
        // — from the refresh's answers, the table's.
        shared.lock().unwrap().refusals = backend_refusals(fleet.as_ref(), &[alpha.clone(), mayor.clone(), delta.clone()]);
        (ui.selected, ui.mode) = (row("sf1"), Mode::Menu { sel: 0 });
        let menu = draw(160, 50, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[s] stop       (Shadeform has no stop: an instance bills") && menu.contains("[u] start      (Shadeform has no stop"), "{menu}");
        assert!(menu.contains("[t] terminate  terminate — irreversible"), "{menu}");
        (ui.selected, ui.mode) = (row("rp1"), Mode::Menu { sel: 0 });
        let menu = draw(160, 50, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[s] stop       stop the pod") && menu.contains("[u] start      start a stopped pod"), "{menu}");
        // A marked set with a pod that can stop: offered; only the Shadeform pod: greyed.
        ui.marked = [&alpha, &mayor].iter().map(|p| mark_key(p)).collect();
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        assert!(draw(160, 50, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop       stop the pod"));
        ui.marked = [mark_key(&mayor)].into();
        assert!(draw(160, 50, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop       (Shadeform has no stop"));
        ui.marked = [&alpha, &mayor, &delta].iter().map(|p| mark_key(p)).collect();
        // The menus.
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
        ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 };
        assert!(!draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
    }

    /// Rename is offered where the pod's backend can rename it — every backend now, Hetzner and
    /// Vast included (prod 2026-10-30: the dashboard offered it on a Hetzner VM and the CLI
    /// refused) — asked of the real fleet, the answer `pods rename` goes by: a pod no configured
    /// backend owns is refused before a new name is asked for, with the fleet's own words.
    #[test]
    fn rename_is_offered_where_the_pods_backend_can_rename() {
        let fleet: Arc<dyn Provider> = Arc::from(
            arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nHETZNER_API_KEY=h\nVAST_API_KEY=v"), false).unwrap(),
        );
        let pods = [pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("delta", "hetzner", "hz1", 4, Some(22)), pod("echo", "vast", "va1", 5, Some(22)), pod("mayor", "shadeform", "sf1", 3, Some(22))];
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let row = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        for id in ["rp1", "hz1", "va1"] {
            (ui.selected, ui.mode) = (row(id), Mode::List);
            choose_pod_action(&shared, &mut ui, &fleet, Action::Rename);
            assert!(matches!(&ui.mode, Mode::Input { action: Action::Rename, .. }), "{id}: {:?}", ui.mode);
        }
        // Shadeform isn't configured here: nothing owns its pod, so nothing can rename it.
        (ui.selected, ui.mode) = (row("sf1"), Mode::List);
        choose_pod_action(&shared, &mut ui, &fleet, Action::Rename);
        assert!(
            matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't rename devtest-mayor — no configured provider owns this pod")),
            "{:?}",
            ui.mode
        );
    }

    /// The marked stop as it runs: the stoppable pods stopped (through the stop's outcome), the
    /// skipped ones named after them, and a ✗ while one still bills (`pods stop` exits non-zero).
    #[tokio::test]
    async fn a_marked_stop_stops_the_rest_and_says_what_still_bills() {
        /// Records each stop; a `shadeform` pod has no stop, as that backend says.
        struct Stops(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Provider for Stops {
            fn name(&self) -> &'static str {
                "fleet"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            async fn stop_pod(&self, id: &str) -> arena_core::Result<()> {
                assert_ne!(id, "sf1", "a pod with no stop was sent a stop");
                self.0.lock().unwrap().push(id.to_string());
                Ok(())
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            fn supports(&self, op: Op, pod: Option<&Pod>) -> arena_core::capability::Support {
                match (op, pod.map(|p| p.provider.as_str())) {
                    (Op::Stop, Some("shadeform")) => arena_core::capability::Support::No { why: "Shadeform has no stop (fake)", instead: "terminate it" },
                    _ => arena_core::capability::Support::Yes,
                }
            }
        }
        let stops = Arc::new(Stops(Mutex::new(Vec::new())));
        let provider: Arc<dyn Provider> = stops.clone();
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let mut delta = pod("delta", "runpod", "rp4", 4, None);
        delta.status = "EXITED".into();
        let pods = vec![pod("alpha", "runpod", "rp1", 1, None), pod("mayor", "shadeform", "sf1", 5, None), delta, pod("bravo", "runpod", "rp2", 2, None)];
        let out = execute_fleet(&provider, &remote, &cfg(""), Action::Stop, pods, None).await;
        assert_eq!(
            out,
            "stop — 2/2 ok\n✓ stopped devtest-alpha\n✓ stopped devtest-bravo\n⊘ devtest-delta — skipped: not running or starting (EXITED)\n\
             ⊘ devtest-mayor — skipped, still billing: Shadeform has no stop (fake) — terminate it\n\
             ✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor (back up, then `arena pods terminate`)"
        );
        let mut stopped = stops.0.lock().unwrap().clone();
        stopped.sort();
        assert_eq!(stopped, ["rp1", "rp2"]);
    }

    /// The AI key's rows end to end, against a stub `arena`: with a provisioning key the per-pod
    /// menu has them — greyed, with why, for a pod without a key, and refused if picked anyway; `a`
    /// opens the raise-cap box prefilled with one key's worth, saying what the key spent and the new
    /// cap; a cap under the spend keeps the box open; Enter runs `arena keys limit <pod> <amount>
    /// --yes`; `o` on a paused key is resume, confirmed with `y`; a marked set's cap is one run for
    /// its pods with a key. A key run's end asks for the keys to be read again.
    #[tokio::test]
    async fn the_ai_key_rows_run_the_clis_keys_commands() {
        use arena_core::openrouter::KeyInfo;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arena-tui-aikey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("arena");
        std::fs::write(&stub, "#!/bin/sh\necho \"args: $*\"\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let key = |name: &str, spent: f64, reset: Option<&str>, off: bool| KeyInfo {
            hash: format!("h{}", name.replace('-', "")),
            name: Some(name.into()),
            limit: Some(5.0),
            limit_remaining: Some(5.0 - spent),
            limit_reset: reset.map(String::from),
            usage: Some(spent),
            usage_monthly: Some(spent),
            disabled: off,
            ..Default::default()
        };
        let mut fixture = fixture_shared();
        fixture.ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.42, Some("monthly"), false), key("devtest-bravo", 1.0, None, true)], None));
        let shared = Arc::new(Mutex::new(fixture));
        let provider: Arc<dyn Provider> = Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\n"), false).unwrap());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.cli = Ok(runs::Cli { exe: stub.clone(), config: "/test/config.env".into(), dir: dir.join("runs") });
        let at = |name: &str| shared.lock().unwrap().pods.iter().position(|p| p.name == name).unwrap();
        async fn first_line_when_ended(shared: &Mutex<Shared>, id: u64) -> Option<String> {
            for _ in 0..200 {
                {
                    let s = shared.lock().unwrap();
                    let r = s.runs.iter().find(|r| r.id == id).unwrap();
                    if !r.running() {
                        return r.lines.front().cloned();
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None
        }

        // No provisioning key: no rows.
        ui.selected = at("devtest-alpha");
        ui.mode = Mode::Menu { sel: 0 };
        assert!(!draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("ai cap"));
        ui.ai_keys = true;
        // A pod with no key: the rows greyed with why; picked anyway, refused.
        ui.selected = at("devtest-charlie");
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui);
        let cap_row = menu.iter().find(|l| l.contains("[a] ai cap")).unwrap_or_else(|| panic!("{}", menu.join("\n")));
        assert!(cap_row.contains("(no OpenRouter key is named after it)"), "{cap_row}");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyLimit);
        let Mode::Result(msg) = &ui.mode else { panic!("{:?}", ui.mode) };
        assert!(msg.contains("✗ can't ai cap devtest-charlie — no OpenRouter key is named after it — `arena keys gen devtest-charlie`"), "{msg}");

        // `a` (the menu), `a` (ai cap): the box, prefilled.
        ui.selected = at("devtest-alpha");
        ui.mode = Mode::Menu { sel: 0 };
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[a] ai cap     AI key: raise or set its cap") && menu.contains("[o] ai pause"), "{menu}");
        let i = at("devtest-alpha");
        let alpha = shared.lock().unwrap().pods[i].clone();
        let rows = aikey::pod_rows(&shared.lock().unwrap(), &ui, Some(&alpha)).0;
        let pick = rows.iter().find(|(k, _)| *k == 'a').unwrap().1;
        choose_pod_action(&shared, &mut ui, &provider, pick);
        let Mode::Input { action: Action::KeyLimit, scope, value } = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert_eq!(value, "+5");
        let boxed = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(boxed.contains("spent $0.42 of $5.00 this month") && boxed.contains("new cap: $10.00"), "{boxed}");
        // Under the spend: refused in the box, Enter keeps it.
        ui.mode = Mode::Input { action: Action::KeyLimit, scope: scope.clone(), value: "0.1".into() };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("✗ a $0.10 cap is under what it has spent this month ($0.42)"));
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope.clone(), "0.1".into());
        assert!(matches!(&ui.mode, Mode::Input { value, .. } if value == "0.1"), "{:?}", ui.mode);
        assert!(shared.lock().unwrap().runs.is_empty());
        // A refresh while the box is open (another operator raised the cap to $25) changes neither
        // the preview nor what Enter runs on (review codex r4 #2): the CLI then refuses the moved key.
        let before = shared.lock().unwrap().ai.clone();
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[KeyInfo { limit: Some(25.0), ..key("devtest-alpha", 0.42, Some("monthly"), false) }], None));
        ui.mode = Mode::Input { action: Action::KeyLimit, scope: scope.clone(), value: "+5".into() };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("spent $0.42 of $5.00 this month"));
        // Enter on +5: `arena keys limit devtest-alpha +5 --yes`, then the keys are read again.
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope, "+5".into());
        shared.lock().unwrap().ai = before;
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!(first_line_when_ended(&shared, id).await.as_deref(), Some("args: --config /test/config.env --provider runpod keys limit devtest-alpha +5 --expect hdevtestalpha=5/monthly/on --yes"));
        {
            let mut s = shared.lock().unwrap();
            assert!(std::mem::take(&mut s.ai_wanted), "the AI$ column is read again");
            assert_eq!(s.notice.as_ref().map(|n| n.text.as_str()), Some("✓ keys limit done — [p] shows it"));
        }

        // A paused key: its `o` row is resume, a `y` confirm saying the key's state.
        ui.selected = at("devtest-bravo");
        ui.mode = Mode::Menu { sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[o] ai resume"));
        let i = at("devtest-bravo");
        let bravo = shared.lock().unwrap().pods[i].clone();
        let rows = aikey::pod_rows(&shared.lock().unwrap(), &ui, Some(&bravo)).0;
        choose_pod_action(&shared, &mut ui, &provider, rows.iter().find(|(k, _)| *k == 'o').unwrap().1);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert_eq!(c.action, Action::KeyResume);
        assert!(c.note.as_deref().unwrap().starts_with("ai key now: disabled · $1.00 of $5.00"), "{:?}", c.note);
        assert_eq!(c.preview.as_deref(), Some("arena keys resume devtest-bravo --expect hdevtestbravo=5/never/off --yes"));
        apply_action(&shared, &mut ui, &provider, &c);
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!(first_line_when_ended(&shared, id).await.as_deref(), Some("args: --config /test/config.env --provider runpod keys resume devtest-bravo --expect hdevtestbravo=5/never/off --yes"));
        // A confirm runs on the key it showed, not on one read since (review codex r3 #1): the key
        // rotated while the pause confirm is open — the run still names the old one, which the CLI
        // then refuses.
        ui.selected = at("devtest-alpha");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyPause);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[KeyInfo { hash: "hnew".into(), ..key("devtest-alpha", 0.0, None, false) }], None));
        apply_action(&shared, &mut ui, &provider, &c);
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        let line = first_line_when_ended(&shared, id).await.unwrap();
        assert!(line.ends_with("keys pause devtest-alpha --expect hdevtestalpha=5/monthly/on --yes"), "{line}");
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.42, Some("monthly"), false), key("devtest-bravo", 1.0, None, true)], None));
        // Rotate: red, its warning, and `--copy`.
        ui.selected = at("devtest-alpha");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyRotate);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert!(c.warning().unwrap().contains("the old key stops working at once") && c.note.as_deref().unwrap().contains("the new key: cap $5.00"), "{c:?}");
        assert_eq!(c.preview.as_deref(), Some("arena keys rotate devtest-alpha --copy --yes"));

        // A marked set: one run for the pods with a key; the one without is left out.
        let pods = shared.lock().unwrap().pods.clone();
        ui.marked = pods.iter().filter(|p| ["devtest-alpha", "devtest-bravo", "devtest-charlie"].contains(&p.name.as_str())).map(mark_key).collect();
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[a] ai cap") && menu.contains("[O] ai resume") && !menu.contains("ai rotate"), "{menu}");
        choose_fleet_action(&shared, &mut ui, &provider, Action::KeyPause, Scope::Marked);
        assert!(matches!(&ui.mode, Mode::FleetConfirm { action: Action::KeyPause, note, .. } if note == "Left out (no AI key that was read): charlie"), "{:?}", ui.mode);
        choose_fleet_action(&shared, &mut ui, &provider, Action::KeyLimit, Scope::Marked);
        let Mode::Input { action: Action::KeyLimit, scope, .. } = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope, "+1".into());
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        let line = first_line_when_ended(&shared, id).await.unwrap();
        let mut named: Vec<&str> = line.split(' ').filter(|w| w.starts_with("devtest-")).collect();
        named.sort();
        assert_eq!(named, ["devtest-alpha", "devtest-bravo"], "{line}");
        assert!(line.starts_with("args: --config /test/config.env --provider runpod keys limit ") && line.contains(" +1 --expect ") && line.ends_with(" --yes"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
