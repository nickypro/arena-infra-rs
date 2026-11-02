//! What the actions that run over SSH do on a pod (test, run, set-branch, backup, setup), each within its
//! budget, and the commands a confirm shows (`v`).

use std::time::Duration;

use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;
use arena_core::{snapshot, Config, Pod};

use crate::backups;
use crate::state::Action;
use crate::table::truncate;

// Per-call SSH budgets for the actions (every pod call goes through `Remote` with one, so
// a wedged pod ends its action with `timed out after Ns` instead of a `working…` modal
// that never closes). The metrics probe uses core's `PROBE_TIMEOUT`, setup core's
// `SetupTimeouts` and the deep check core's `DEEP_CHECK_TIMEOUT`; the rest mirror the
// CLI's budgets for the same commands, which live in its binary, not in core.

/// `test` (import torch): a cold import takes 10–30s, so 90s means wedged — as `pods test`.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `run`: an arbitrary command can legitimately take a while (a download, a test suite),
/// but never forever — the CLI's `pods run` default, 30 min.
const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `backup`: git add + commit + push — seconds normally, a first push of notebooks can take
/// minutes. 5 min, as `pods backup`.
const BACKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `set-branch`: fetch + checkout + fast-forward against GitHub — 2 min means stuck, as `pods
/// set-branch`.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// set-branch's switch and init-branches bound themselves on the pod (`timeout`, then a KILL) inside
// that budget, so their pod side has ended before arena reports a timeout and releases the pod's
// lock (review ai-review claude #13).
const _: () = assert!(arena_core::backup::SWITCH_BOUND_SECS + arena_core::backup::SWITCH_KILL_AFTER_SECS < BRANCH_TIMEOUT.as_secs());

/// The torch health check (mirrors `arena pods test`).
pub(crate) const TORCH_TEST_CMD: &str =
    "python -c 'import torch; print(torch.__version__)' 2>&1 || python3 -c 'import torch; print(torch.__version__)'";

/// Run a command over SSH (within `timeout`) and report its last output line — `test`'s
/// torch version (`run` shows the whole output: [`run_ssh_output`]).
pub(crate) async fn run_ssh_oneline(
    remote: &dyn Remote,
    cfg: &Config,
    pod: &Pod,
    cmd: &str,
    what: &str,
    timeout: Duration,
) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match remote.exec(&target, cmd, Some(timeout)).await {
        Ok(out) if out.success => {
            let line = out.stdout.lines().last().unwrap_or("").trim();
            format!("✓ {} {what}: {line}", pod.name)
        }
        Ok(out) => format!("✗ {} {what} ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
        Err(e) => format!("✗ {} {what} failed: {}", pod.name, describe_error(&e)),
    }
}

/// `run`: the operator's command over SSH, within [`RUN_TIMEOUT`], reported with its output
/// ([`run_report`]).
pub(crate) async fn run_ssh_output(remote: &dyn Remote, cfg: &Config, pod: &Pod, cmd: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match remote.exec(&target, cmd, Some(RUN_TIMEOUT)).await {
        Ok(out) => run_report(&pod.name, &out),
        Err(e) => format!("✗ {} run failed: {}", pod.name, describe_error(&e)),
    }
}

/// The most output lines `run` shows per pod: the last ones — where a command's result and its
/// errors end up — with the rest counted. The result popup scrolls; a fleet's worth of
/// unbounded output would not.
const RUN_OUTPUT_LINES: usize = 40;

/// The most characters of one output line `run` shows (a minified file, a progress log).
const RUN_LINE_CHARS: usize = 300;

/// `run`'s outcome for one pod: the command's output — stdout, plus stderr when it failed —
/// each line as a terminal would show it (`jobs::display_line`: a progress bar's last state, no
/// escape sequences), on the heading's line when it's one line, else indented under it, the
/// last [`RUN_OUTPUT_LINES`] of it with how many came before. Live: it showed only the last
/// line — `nvidia-smi -L; hostname` read as just the hostname. Pure.
fn run_report(name: &str, out: &arena_core::ssh::SshOutput) -> String {
    let head = if out.success { format!("✓ {name} run") } else { format!("✗ {name} run ({})", out.exit_label()) };
    let mut text: Vec<String> = out.stdout.lines().map(|l| arena_core::jobs::display_line(l.as_bytes())).collect();
    if !out.success {
        text.extend(out.stderr.lines().map(|l| arena_core::jobs::display_line(l.as_bytes())));
    }
    while text.last().is_some_and(|l| l.trim().is_empty()) {
        text.pop();
    }
    let shown: Vec<String> = text.iter().map(|l| truncate(&l.replace('\t', "    "), RUN_LINE_CHARS)).collect();
    match shown.as_slice() {
        [] => format!("{head}: (no output)"),
        [one] => format!("{head}: {one}"),
        _ => {
            let skip = shown.len().saturating_sub(RUN_OUTPUT_LINES);
            let mut lines = vec![format!("{head}:")];
            if skip > 0 {
                lines.push(format!("  … {skip} earlier line(s) not shown"));
            }
            lines.extend(shown[skip..].iter().map(|l| format!("  {l}")));
            lines.join("\n")
        }
    }
}

/// Gently switch a pod's ARENA checkout to `branch` (mirrors `arena pods set-branch`),
/// within [`BRANCH_TIMEOUT`] — after the branch it's on is backed up ([`run_backup`]), as the
/// CLI does: after a switch, commits that couldn't be pushed live only in a local branch on the
/// pod. A branch that can't be pushed isn't switched away from here, since the TUI can't copy
/// the pod's files first; the line names the CLI command that does. A pod with a merge in
/// progress is never switched by a gentle switch, the CLI's included: its line says what is in
/// progress and how to end it ([`busy_not_switched`]).
pub(crate) async fn run_set_branch(remote: &dyn Remote, cfg: &Config, pod: &Pod, branch: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let (backed, kept) = backup_and_report(remote, cfg, pod).await;
    match kept {
        // A pod whose `origin` isn't the configured repo: the CLI doesn't switch it either.
        Some(Kept::WrongOrigin(why)) => return format!("✗ {} not switched to {branch}: {why}", pod.name),
        Some(Kept::Busy { what, on, unmerged }) => return busy_not_switched(&pod.name, branch, &what, &on, unmerged),
        None => {}
    }
    if backed.starts_with('✗') || backed.contains(" — busy:") {
        return format!(
            "✗ {} not switched to {branch}: the branch it's on wasn't pushed first ({backed}) — `arena pods set-branch {branch} {}` \
             copies its files, then switches",
            pod.name, pod.name
        );
    }
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    // The TUI set-branch is gentle (ff-only); the destructive --hard reset is CLI-only.
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, cfg.get("GIT_SSH_KEY_REMOTE"), false);
    match remote.exec(&target, &cmd, Some(BRANCH_TIMEOUT)).await {
        Ok(out) if out.success => format!("✓ {} → {branch}", pod.name),
        Ok(out) => format!("✗ {} set-branch ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
        Err(e) => format!("✗ {} set-branch failed: {}", pod.name, describe_error(&e)),
    }
}

/// Commit + push the pod's ARENA tree over SSH **on its current branch** (mirrors
/// `arena backup` for a single pod): never switches/creates a branch, skips main/master, and
/// refuses a pod whose `origin` isn't the configured repo. Within [`BACKUP_TIMEOUT`].
pub(crate) async fn run_backup(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> String {
    backup_and_report(remote, cfg, pod).await.0
}

/// Why a pod's backup leaves it where it is for the TUI's set-branch — said in words of its own,
/// not as a push that failed ([`backup_and_report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kept {
    /// Its `origin` isn't the configured repo: the refusal in its own words (`origin is …, not the
    /// configured … — not backed up to git; run …`).
    WrongOrigin(String),
    /// An operation in progress (`BUSY`): what it is (`merge`, …: `arena_core::backup::BUSY_OP_SH`),
    /// on which branch, and whether paths are still unmerged.
    Busy { what: String, on: String, unmerged: bool },
}

/// The TUI set-branch's line for a pod with an operation in progress (review F5): what it is and
/// how to end it, as the CLI's gentle switch would refuse it (copying the pod first only to
/// refuse, the CLI's advice before, would gain nothing) — and `--hard` named as the destructive
/// way, which copies the files first and then discards the uncommitted changes to tracked files,
/// the operation with them. Pure.
fn busy_not_switched(pod: &str, branch: &str, what: &str, on: &str, unmerged: bool) -> String {
    use arena_core::backup::{busy_advice, busy_noun, busy_situation, busy_switch_why};
    let noun = busy_noun(what);
    let why = busy_switch_why(what).map_or_else(String::new, |w| format!(" ({w})"));
    format!(
        "✗ {pod} not switched to {branch}: {} — not committed or switched{why}; {} on the pod, then switch again. Or, \
         destructively: `arena pods set-branch {branch} {pod} --hard` copies its files first, then discards its uncommitted \
         changes to tracked files ({noun} with them)",
        busy_situation(what, on),
        busy_advice(what, unmerged)
    )
}

/// [`run_backup`]'s line, and why the pod must stay where it is beyond a failed push, when it
/// must ([`Kept`]).
pub(crate) async fn backup_and_report(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> (String, Option<Kept>) {
    use arena_core::backup::{self, parse_backup_report};
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return (format!("✗ {}: {e}", pod.name), None),
    };
    // The pod's backup lock, like every other writer of its backup (RR10): never alongside a
    // pull, the cron's push or the reaper's own backup of it (whose git step would then fail
    // on `.git/index.lock`). Busy = say so and do nothing.
    // The CLI's backup dir (`LOCAL_BACKUP_DIR`, else the fleet's `backup/`), so the lock is the same one.
    let (dir, _) = cfg.local_backup_dir();
    let _lock = match arena_core::pull::try_pod_lock_now(&dir, &pod.name) {
        Ok(lock) => lock,
        Err(()) => return (format!("⊘ {} — busy: another backup of it (a pull, the cron or the reaper) is running; try again shortly", pod.name), None),
    };
    let repo = backup_repo_path(cfg);
    let origin = backup::configured_repo(cfg);
    let cmd = backup::backup_command(&repo, cfg.get("GIT_SSH_KEY_REMOTE"), &format!("arena-tui backup {}", pod.name), origin.as_deref());
    use arena_core::backups::record::Outcome;
    let note = |outcome, detail: String| backups::note_git(cfg, &pod.name, outcome, detail);
    match remote.exec(&target, &cmd, Some(BACKUP_TIMEOUT)).await {
        Ok(out) if out.success => {
            let report = parse_backup_report(&out.stdout);
            let notes = report.notes();
            let also = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
            let line = match &report.outcome {
                Some((backup::BACKUP_PUSHED, branch)) => {
                    note(Outcome::Pushed, format!("→ {branch}"));
                    format!("✓ backed up {} → {branch}{also}", pod.name)
                }
                Some((backup::BACKUP_NO_CHANGES, branch)) => {
                    note(Outcome::NoChanges, format!("on {branch}"));
                    format!("✓ {} — no changes (on {branch}){also}", pod.name)
                }
                Some((backup::BACKUP_SKIPPED, branch)) => {
                    note(Outcome::NotSaved, format!("on {branch}"));
                    format!("⊘ {} — skipped (on protected branch {branch})", pod.name)
                }
                _ => {
                    note(Outcome::Pushed, String::new());
                    format!("✓ backed up {}{also}", pod.name)
                }
            };
            (line, None)
        }
        Ok(out) => {
            let report = parse_backup_report(&out.stdout);
            let fix = report.failure(&repo, &pod.name, out.code);
            let line = match &fix {
                Some(fix) => format!("✗ backup {}: {fix}", pod.name),
                None => format!("✗ backup {} ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
            };
            note(Outcome::Failed, report.failure_brief(out.code).unwrap_or_else(|| format!("{}: {}", out.exit_label(), out.stderr.trim())));
            let kept = match (report.wrong_origin_why(out.code), report.in_progress(out.code)) {
                (Some(_), _) => fix.map(Kept::WrongOrigin),
                (None, Some(what)) => Some(Kept::Busy {
                    what: what.to_string(),
                    on: report.branch.clone().unwrap_or_else(|| "its branch".to_string()),
                    unmerged: report.unmerged,
                }),
                (None, None) => None,
            };
            (line, kept)
        }
        Err(e) => {
            note(Outcome::Failed, describe_error(&e));
            (format!("✗ backup {} failed: {}", pod.name, describe_error(&e)), None)
        }
    }
}

/// The ARENA checkout path on a pod (config `BACKUP_REPO_PATH`, else /root/<repo name>).
fn backup_repo_path(cfg: &Config) -> String {
    cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    })
}

/// Provision the pod over SSH — the steps `arena pods setup` runs for its provider (non-force,
/// matching the CLI default): on an image-based pod, copy the deploy key, move the repo onto
/// the pod's volume when it has one, run the config; on a bare VM (Hetzner, Shadeform), copy
/// the deploy key, authorize the keys, set the hostname and run the VM script (swap included).
/// Through core's runner, so each step keeps its own budget (core `SetupTimeouts`:
/// `SETUP_TIMEOUT_SECS` honoured) and a step's `arena-warning:` lines — the only sign that the
/// repo did NOT go onto the volume, and why — reach the result line (`✓ set up <pod> (warning:
/// …)`) instead of being dropped with its stdout.
pub(crate) async fn run_setup(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> String {
    use arena_core::setup::{provision, BootRetry, ProvisionOutcome};
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let script = vm_script_path(pod);
    let steps = match setup_steps(cfg, pod, &script.to_string_lossy()) {
        Ok(s) => s,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    // A bare VM's steps scp the script from this staged copy, held until the steps are done.
    let _staged = match arena_core::setup::is_bare_vm(&pod.provider).then(|| StagedScript::write(&script)).transpose() {
        Ok(staged) => staged,
        Err(e) => return format!("✗ {}: staging the VM setup script at {}: {e}", pod.name, script.display()),
    };
    // No boot-race retries: the operator presses Setup on a pod that is already up.
    let no_wait = BootRetry { window: Duration::ZERO, every: Duration::ZERO };
    let done = match provision(remote, &target, &steps, no_wait).await {
        ProvisionOutcome::Done { warnings, .. } if warnings.is_empty() => format!("✓ set up {}", pod.name),
        ProvisionOutcome::Done { warnings, .. } => format!("✓ set up {} (warning: {})", pod.name, warnings.join("; ")),
        failed => return format!("✗ setup {}: {}", pod.name, failed.describe()),
    };
    // Its own Claude Code account's token, as `pods setup` puts on (one by spread if it has none).
    let one = [(pod.clone(), target)];
    match arena_core::claude::stage(remote, cfg, &one, None, snapshot::unix_now()).await {
        Ok(None) => done,
        Ok(Some(outcomes)) => {
            let said: Vec<String> = outcomes.iter().map(|o| o.line()).collect();
            format!("{done}\nClaude Code: {}", said.join("; "))
        }
        Err(e) => format!("{done}\n✗ Claude Code: not pushed — {e}"),
    }
}

/// The TUI's setup steps: core's list for the pod's own provider, as `arena pods setup` builds
/// it — a bare VM's script (`script`: the local copy its scp sends, [`vm_script_path`]) and its
/// swap, key and hostname steps included; until 2026-10-31 it was the image-based list for every
/// pod, so a Hetzner VM got no script and no swap. The agents' notes too (a reset container has
/// lost them). No VS Code warm-up: that stays `arena pods setup`'s, as do its API keys.
fn setup_steps(cfg: &Config, pod: &Pod, script: &str) -> arena_core::Result<Vec<arena_core::setup::ProvisionStep>> {
    use arena_core::setup::{provisioning_steps, SetupConfig, SetupTimeouts};
    let mut scfg = SetupConfig::from_config(cfg)?;
    scfg.vscode = None;
    let budget = SetupTimeouts::from_config(cfg, None)?;
    Ok(provisioning_steps(&pod.provider, &scfg, &pod.name, false, script, &budget))
}

/// Where this dashboard stages the bare-VM script for `pod`'s setup: per process and pod, so
/// setups running at once (a marked set) never write over or remove each other's copy.
fn vm_script_path(pod: &Pod) -> std::path::PathBuf {
    let name: String = pod.name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
    std::env::temp_dir().join(format!("arena-tui-vm-setup-{}-{name}.sh", std::process::id()))
}

/// The bare-VM script staged for one setup: a new file only this user can read, made with
/// `create_new` — so never written through a link, or over a file someone else put at that
/// predictable path (a second setup of the same pod at once is refused the same way) — and
/// removed when dropped, however the setup ends. A file that was already there is left alone.
struct StagedScript(std::path::PathBuf);

impl StagedScript {
    fn write(path: &std::path::Path) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        // Ours from here on: removed on any return, the write's failure included.
        let staged = StagedScript(path.to_path_buf());
        file.write_all(arena_core::setup::HETZNER_SETUP.as_bytes())?;
        Ok(staged)
    }
}

impl Drop for StagedScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The exact command(s) a safe (non-typed) action will run, for the confirm modal's `v` (it
/// says what the action does otherwise: [`confirm_body`]). Lifecycle actions return `None`.
pub(crate) fn build_preview(cfg: &Config, action: Action, pod: &Pod) -> Option<String> {
    let target = SshTarget::from_pod(pod, cfg);
    match action {
        Action::Backup => Some(match &target {
            Ok(t) => {
                // The script's arguments, not its ~170 lines (`arena pods backup --dry-run` prints it,
                // as the here-doc that writes the file this line reads).
                let msg = format!("arena-tui backup {}", pod.name);
                let origin = arena_core::backup::configured_repo(cfg);
                arena_core::backup::backup_dry_run_line(t, &backup_repo_path(cfg), cfg.get("GIT_SSH_KEY_REMOTE"), &msg, origin.as_deref())
            }
            Err(e) => format!("⚠ {e}"),
        }),
        Action::Setup => Some(match (&target, setup_steps(cfg, pod, &vm_script_path(pod).to_string_lossy())) {
            (Ok(t), Ok(steps)) => steps
                .iter()
                .map(|step| {
                    use arena_core::setup::ProvisionStep;
                    match step {
                        ProvisionStep::Scp { local, remote, .. } => t.display_scp(local, remote),
                        ProvisionStep::Run { cmd, .. } => t.display_command(cmd),
                        ProvisionStep::Optional { label, summary, .. } => format!("# {label} (best-effort): {summary}"),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            (Err(e), _) => format!("⚠ {e}"),
            (_, Err(e)) => format!("⚠ {e}"),
        }),
        Action::Test => Some(match &target {
            Ok(t) => t.display_command(TORCH_TEST_CMD),
            Err(e) => format!("⚠ {e}"),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use crate::tests::{cfg, pod};

    /// Every action's SSH goes through `Remote` with its budget, so a wedged pod ends the
    /// action with `timed out after …` instead of a `working…` modal that never closes.
    #[tokio::test(start_paused = true)]
    async fn actions_go_through_remote_with_their_budgets() {
        let host = "10.0.0.1:22001";
        let wrong_origin = || {
            FakeReply::exit(5, "")
                .with_stdout("WRONG_ORIGIN nickypro/arena-sandbox-materials https://github.com/ARENA-education/ARENA_materials\n")
        };
        let cfg = cfg("ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\n");
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let budgets = |fake: &FakeRemote| -> Vec<Option<Duration>> {
            fake.calls()
                .into_iter()
                .map(|c| match c {
                    RemoteCall::Exec { timeout, .. } | RemoteCall::Copy { timeout, .. } => timeout,
                })
                .collect()
        };
        // (what, the scripted replies, the outcome line starts with, budgets used)
        let setup = arena_core::setup::SetupTimeouts::default();
        // The agents' notes, last (best-effort; the dashboard's setup writes them too).
        let notes = arena_core::agent_notes::TIMEOUT;
        type Case = (&'static str, Vec<FakeReply>, &'static str, Vec<Duration>);
        let cases: Vec<Case> = vec![
            ("test", vec![FakeReply::stdout("noise\n2.9.0+cu130\n")], "✓ devtest-alpha torch: 2.9.0+cu130", vec![TEST_TIMEOUT]),
            ("test-hung", vec![FakeReply::hang()], "✗ devtest-alpha torch failed: timed out after 90s", vec![TEST_TIMEOUT]),
            ("run-hung", vec![FakeReply::hang()], "✗ devtest-alpha run failed: timed out after 1800s", vec![RUN_TIMEOUT]),
            ("backup", vec![FakeReply::exit(1, "rejected")], "✗ backup devtest-alpha (exit 1): rejected", vec![BACKUP_TIMEOUT]),
            // The branch it's on backed up first, then switched.
            ("set-branch", vec![FakeReply::stdout("NO_CHANGES autocommit-x\n"), FakeReply::ok()], "✓ devtest-alpha → w1d2", vec![BACKUP_TIMEOUT, BRANCH_TIMEOUT]),
            // …and not switched when that push fails (the CLI copies its files first; the TUI can't).
            (
                "set-branch",
                vec![FakeReply::exit(4, "remote: error: GH013"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: the branch it's on wasn't pushed first (✗ backup devtest-alpha (exit 4): \
                 remote: error: GH013) — `arena pods set-branch w1d2 devtest-alpha` copies its files, then switches",
                vec![BACKUP_TIMEOUT],
            ),
            // A pod whose origin isn't the configured repo: refused by the backup (exit 5), said with
            // the fix — and not switched, the CLI wouldn't either.
            (
                "backup",
                vec![wrong_origin()],
                "✗ backup devtest-alpha: origin is https://github.com/ARENA-education/ARENA_materials, not the configured \
                 nickypro/arena-sandbox-materials — not backed up to git; run `arena pods setup devtest-alpha`",
                vec![BACKUP_TIMEOUT],
            ),
            // A pod mid-merge (review F5): not switched, said as what is in progress and how to end
            // it, `--hard` named as the destructive way — never "the CLI copies its files, then
            // switches": the CLI's gentle switch refuses it too.
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x merge unmerged\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: a merge is in progress on autocommit-x — not committed or switched (a switch \
                 now would end the merge); resolve the conflicts and `git add` them, then `git commit` — or abort it (`git merge \
                 --abort`) on the pod, then switch again. Or, destructively: `arena pods set-branch w1d2 devtest-alpha --hard` \
                 copies its files first, then discards its uncommitted changes to tracked files (the merge with them)",
                vec![BACKUP_TIMEOUT],
            ),
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x conflicts\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: autocommit-x has unresolved conflicts — not committed or switched; resolve \
                 them and `git add` them, or undo them (`git reset --merge`) on the pod, then switch again. Or, destructively: \
                 `arena pods set-branch w1d2 devtest-alpha --hard` copies its files first, then discards its uncommitted changes to \
                 tracked files (the unresolved conflicts with them)",
                vec![BACKUP_TIMEOUT],
            ),
            // Review pr4: `git am` (HEAD attached) — a gentle switch would carry it onto the other
            // branch, half-done, not end it.
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x am\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: a `git am` is in progress on autocommit-x — not committed or switched (a \
                 switch now would carry the `git am`, half-done, onto the other branch); finish it (`git am --continue`) or abort it \
                 (`git am --abort`) on the pod, then switch again. Or, destructively: `arena pods set-branch w1d2 devtest-alpha \
                 --hard` copies its files first, then discards its uncommitted changes to tracked files (the `git am` with them)",
                vec![BACKUP_TIMEOUT],
            ),
            (
                "set-branch",
                vec![wrong_origin(), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: origin is https://github.com/ARENA-education/ARENA_materials, not the \
                 configured nickypro/arena-sandbox-materials — not backed up to git; run `arena pods setup devtest-alpha`",
                vec![BACKUP_TIMEOUT],
            ),
            ("setup", vec![FakeReply::ok(), FakeReply::ok(), FakeReply::ok()], "✓ set up devtest-alpha", vec![setup.copy, setup.relocate, setup.config, notes]),
            (
                "setup-hung",
                vec![FakeReply::ok(), FakeReply::ok(), FakeReply::hang()],
                "✗ setup devtest-alpha: timed out at repo + keys config after 300s",
                vec![setup.copy, setup.relocate, setup.config],
            ),
            // What the relocation says reaches the line: the repo did NOT go onto the volume.
            (
                "setup-warns",
                vec![
                    FakeReply::ok(),
                    FakeReply::stdout("arena-warning: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle\n"),
                    FakeReply::ok(),
                ],
                "✓ set up devtest-alpha (warning: repo onto volume: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle)",
                vec![setup.copy, setup.relocate, setup.config, notes],
            ),
            // …and so does what the config step says.
            (
                "setup-config-warns",
                vec![FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("arena-warning: another setup is still moving the repo onto the volume - the update ran anyway\n")],
                "✓ set up devtest-alpha (warning: repo + keys config: another setup is still moving the repo onto the volume - the update ran anyway)",
                vec![setup.copy, setup.relocate, setup.config, notes],
            ),
        ];
        for (what, replies, want, budget) in cases {
            let fake = FakeRemote::new();
            fake.script(host, replies);
            let line = match what {
                "test" | "test-hung" => run_ssh_oneline(&fake, &cfg, &alpha, TORCH_TEST_CMD, "torch", TEST_TIMEOUT).await,
                "run-hung" => run_ssh_output(&fake, &cfg, &alpha, "sleep 99999").await,
                "backup" => run_backup(&fake, &cfg, &alpha).await,
                "set-branch" => run_set_branch(&fake, &cfg, &alpha, "w1d2").await,
                _ => run_setup(&fake, &cfg, &alpha).await,
            };
            assert_eq!(line, want, "{what}");
            assert_eq!(budgets(&fake), budget.into_iter().map(Some).collect::<Vec<_>>(), "{what}");
        }
        // The backup it runs carries the configured repo for that check.
        let fake = FakeRemote::new();
        run_backup(&fake, &cfg, &alpha).await;
        let sent: Vec<String> = fake.calls().into_iter().filter_map(|c| match c { RemoteCall::Exec { cmd, .. } => Some(cmd), _ => None }).collect();
        assert!(sent.len() == 1 && sent[0].ends_with(" 'nickypro/arena-sandbox-materials'"), "{sent:?}");
        // No endpoint: said, and nothing is attempted.
        let fake = FakeRemote::new();
        let charlie = pod("charlie", "runpod", "rp3", 3, None);
        assert!(run_backup(&fake, &cfg, &charlie).await.starts_with("✗ devtest-charlie: "));
        // RR10: the pod's backup lock held by another backup (a pull, the cron, the reaper):
        // busy, said, nothing run on the pod.
        let dir = std::env::temp_dir().join(format!("arena-tui-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let locked = self::cfg(&format!(
            "ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\nLOCAL_BACKUP_DIR={}\n",
            dir.display()
        ));
        let held = arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&dir.display().to_string(), &alpha.name).unwrap()).unwrap().expect("free");
        let quiet = FakeRemote::new();
        let line = run_backup(&quiet, &locked, &alpha).await;
        assert!(line.starts_with("⊘ ") && line.contains("busy"), "{line}");
        assert!(quiet.calls().is_empty(), "{:?}", quiet.calls());
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fake.calls().is_empty());
    }

    /// The dashboard's setup on a Hetzner VM runs what `arena pods setup` runs there — the deploy
    /// key, the keys authorized, the hostname, then the VM script with its swap — not the
    /// image-based steps it gave every pod before (2026-10-31, the project-week rehearsal: no
    /// script, no swap, on the provider organisers use most). The script goes from a copy staged
    /// for this pod and removed afterwards; a RunPod pod's steps are unchanged.
    #[tokio::test(start_paused = true)]
    async fn the_dashboards_setup_runs_a_bare_vms_own_steps() {
        let cfg = cfg("ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\n");
        let t = arena_core::setup::SetupTimeouts::default();
        let hetzner = pod("hzbox", "hetzner", "4242", 7, Some(22));
        let script = vm_script_path(&hetzner);
        let fake = FakeRemote::new();
        assert_eq!(run_setup(&fake, &cfg, &hetzner).await, "✓ set up devtest-hzbox");
        let calls = fake.calls();
        let budgets: Vec<Option<Duration>> = calls
            .iter()
            .map(|c| match c {
                RemoteCall::Exec { timeout, .. } | RemoteCall::Copy { timeout, .. } => *timeout,
            })
            .collect();
        // deploy key, authorize keys, hostname, the script's copy, repo update, the script, the
        // agents' notes.
        let want = [t.copy, t.config, t.config, t.copy, t.config, t.bare_vm, arena_core::agent_notes::TIMEOUT];
        assert_eq!(budgets, want.map(Some).to_vec(), "{calls:?}");
        let copied = calls.iter().find_map(|c| match c {
            RemoteCall::Copy { local, remote, .. } if remote == "/root/hetzner_setup.sh" => Some(local.clone()),
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some(script.to_string_lossy().as_ref()), "{calls:?}");
        let run = calls.iter().find_map(|c| match c {
            RemoteCall::Exec { cmd, .. } if cmd.ends_with("bash /root/hetzner_setup.sh") => Some(cmd.clone()),
            _ => None,
        });
        assert!(run.as_deref().is_some_and(|c| c.starts_with("ARENA_SWAP_GB=8 ")), "{calls:?}");
        assert!(calls.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("hostnamectl set-hostname"))), "{calls:?}");
        assert!(!script.exists(), "the staged script is removed: {}", script.display());
        // Its confirm's `v` shows the same steps, the script's copy among them.
        let preview = build_preview(&cfg, Action::Setup, &hetzner).unwrap();
        assert!(preview.contains("/root/hetzner_setup.sh") && preview.contains("# hostname (best-effort)"), "{preview}");
        assert!(!script.exists(), "a preview stages nothing");
        // A file (or a link) already at that path: refused before anything runs, and left alone.
        let elsewhere = std::env::temp_dir().join(format!("arena-tui-vm-setup-target-{}", std::process::id()));
        std::fs::write(&elsewhere, "keep me").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &script).unwrap();
        let fake = FakeRemote::new();
        let line = run_setup(&fake, &cfg, &hetzner).await;
        assert!(line.starts_with("✗ devtest-hzbox: staging the VM setup script at ") && line.contains("exists"), "{line}");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep me");
        assert!(std::fs::symlink_metadata(&script).is_ok_and(|m| m.file_type().is_symlink()), "the link is left as it was");
        std::fs::remove_file(&script).unwrap();
        std::fs::remove_file(&elsewhere).unwrap();

        // A RunPod pod: the image-based steps, as before, and nothing staged.
        let runpod = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let fake = FakeRemote::new();
        assert_eq!(run_setup(&fake, &cfg, &runpod).await, "✓ set up devtest-alpha");
        let budgets: Vec<Option<Duration>> = fake
            .calls()
            .into_iter()
            .map(|c| match c {
                RemoteCall::Exec { timeout, .. } | RemoteCall::Copy { timeout, .. } => timeout,
            })
            .collect();
        assert_eq!(budgets, vec![Some(t.copy), Some(t.relocate), Some(t.config), Some(arena_core::agent_notes::TIMEOUT)]);
        assert!(!fake.calls().iter().any(|c| matches!(c, RemoteCall::Copy { remote, .. } if remote == "/root/hetzner_setup.sh")));
        assert!(!vm_script_path(&runpod).exists());
    }

    /// Review ai-review claude #6, checked: does the TUI's set-branch honour `DISCARD_MAX_GB`, the
    /// most a `--hard` save copies? It has nothing to honour: the TUI only ever switches gently —
    /// `--hard` is the CLI's — and a gentle switch saves nothing. Pinned: the switch it sends is
    /// `checkout_command(…, false)`, holding no save, the same whatever the limit says (a bad value
    /// included), and the same as the limited command with any limit. Were the TUI ever to switch
    /// with `--hard`, this fails, and the limit would have to be passed as the CLI passes it.
    #[tokio::test]
    async fn the_tuis_set_branch_is_gentle_so_no_save_limit_applies() {
        use arena_core::backup::{checkout_command, checkout_command_limited, discard_max_kb};
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        for extra in ["", "DISCARD_MAX_GB=0.001\n", "DISCARD_MAX_GB=lots\n"] {
            let cfg = cfg(&format!("GIT_SSH_KEY_REMOTE=/k\n{extra}"));
            let fake = FakeRemote::new();
            fake.script("10.0.0.1:22001", vec![FakeReply::stdout("NO_CHANGES autocommit-x\n"), FakeReply::ok()]);
            assert_eq!(run_set_branch(&fake, &cfg, &alpha, "w1d2").await, "✓ devtest-alpha → w1d2", "{extra:?}");
            let sent: Vec<String> = fake.calls().into_iter().filter_map(|c| match c { RemoteCall::Exec { cmd, .. } => Some(cmd), _ => None }).collect();
            let gentle = checkout_command("/root/ARENA_materials", "w1d2", Some("/k"), false);
            assert_eq!(sent.get(1), Some(&gentle), "{extra:?}");
            assert!(!gentle.contains("discarded_save"));
            if let Ok(kb) = discard_max_kb(&cfg) {
                assert_eq!(checkout_command_limited("/root/ARENA_materials", "w1d2", Some("/k"), false, kb), gentle, "{extra:?}");
            }
        }
    }
}
