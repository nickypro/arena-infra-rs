//! `pods run`: a shell command on every selected pod over SSH, in parallel, each within its budget.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::remote::Remote;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::confirm_brief;
use arena_core::out;
use crate::pods::exec_each_pod;
use crate::pods::selection::Selected;

/// `run`'s flags that only mean something BEFORE the command. After it, clap hands them to
/// the command (`trailing_var_arg`), so `run hostname -t apple` would run `hostname -t
/// apple` on *every* pod, `--exclude`/`--on`/`--gpus` would stop narrowing, and a trailing
/// `--dry-run` would make the preview real. (`--all` is left alone: misplaced, it changes
/// nothing — `run` defaults to every pod.) A trailing `--background` would run the command
/// in the foreground, to be cut off at the timeout.
const RUN_FLAGS_BEFORE_COMMAND: &[&str] = &[
    "-t", "--target", "--include", "--exclude", "--on", "--gpus", "--dry-run", "--dryrun", "--dry", "--background", "--bg",
];

/// Why `run`'s command can't be taken as typed: it holds one of
/// [`RUN_FLAGS_BEFORE_COMMAND`] as a bare word (or `--flag=value`). Only whole words count,
/// so a quoted command (`run 'tmux kill-session -t lab'` → one word) passes. Pure, so
/// table-tested.
pub(crate) fn misplaced_run_flags(command: &[String]) -> Option<String> {
    let hits: Vec<&str> = command
        .iter()
        .map(String::as_str)
        .filter(|w| RUN_FLAGS_BEFORE_COMMAND.iter().any(|f| w == f || w.strip_prefix(f).is_some_and(|r| r.starts_with('='))))
        .collect();
    if hits.is_empty() {
        return None;
    }
    Some(format!(
        "`{}` after the command would be part of the command — `pods run` takes its flags BEFORE \
         the command, so as typed this runs `{}` on the whole selection (every pod, unless \
         flags before it narrow it). Nothing was run. Put the flags first (`arena pods run -t \
         apple <command>`), or — if the command itself takes that flag — quote the whole \
         command: `arena pods run '{}'`.",
        hits.join(" "),
        command.join(" "),
        command.join(" ")
    ))
}

/// Why `run`'s command can't be taken as typed: its first word is the name of a selected pod
/// (full, `devtest-apple`, or short, `apple`). `pods run apple 'ls'` — the `-t` forgotten — runs
/// `apple ls` on every pod; a live test did exactly that once, and only the missing terminal (no
/// `-y`) stopped it. `names` = each selected pod's (full, short) name. A command that really
/// starts with such a word can be written `command apple …`. Pure, so table-tested.
pub(crate) fn pod_name_as_command(command: &[String], names: &[(&str, &str)]) -> Option<String> {
    let first = command.first()?.split_whitespace().next()?;
    let (full, _) = names.iter().find(|(full, short)| first == *full || first == *short)?;
    let rest = command.join(" ");
    let rest = rest.trim_start().strip_prefix(first).unwrap_or(&rest).trim_start();
    Some(format!(
        "`{first}` is a pod's name ({full}) — as typed, `pods run` would run `{}` on all {} selected pod(s), \
         since the pods are named with -t BEFORE the command. Nothing was run. Did you mean `arena pods run -t \
         {first} '{rest}'`? (A command that really starts with `{first}`: `arena pods run 'command {first} …'`.)",
        command.join(" "),
        names.len()
    ))
}

/// Run `cmd` on every selected pod with an SSH endpoint, concurrently, each pod within
/// `timeout` (a pod that runs over is a failure; the others carry on). `compact` prints one
/// line per pod (last stdout line); otherwise a per-pod block. `confirm_needed` gates it
/// behind the y/N prompt (arbitrary exec); read-only checks pass false.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_run(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    // The pods to run on, resolved by the caller.
    sel: &Selected,
    cmd: &str,
    timeout: Duration,
    dry_run: bool,
    yes: bool,
    confirm_needed: bool,
    compact: bool,
) -> Result<()> {
    // Pods we can't reach yet are reported (not silently dropped), so a "run on every pod"
    // can't quietly skip part of the fleet while reporting success.
    let missing = sel.endpoint_failures();
    let mut targets = sel.named_targets(cfg)?;
    if targets.is_empty() && !missing.is_empty() { anyhow::bail!("no SSH endpoint for running pods: {}", missing.join(", ")); }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    if targets.is_empty() {
        out!(Out, "(no pods with an SSH endpoint)");
        return Ok(());
    }

    // Source the participants' ~/.zshrc and activate the conda env, so commands see the
    // participants' `arena-env` (python/packages) and the token exports from setup — on a box
    // with no zsh, bash's ~/.bashrc, else plain /bin/sh, said beside that pod's result.
    // `CONDA_ENV=""` in config disables activation (still sources the rc for tokens).
    let conda_env = cfg.get("CONDA_ENV").unwrap_or("arena-env");
    let remote_cmd = arena_core::ssh::login_shell_wrap(cmd, Some(conda_env));

    let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
    // By default the command and every pod in a line; the shell it's wrapped in is `-v`'s.
    let brief = format!("`{cmd}` on {}", crate::pods::setup::all_names(&names));
    if dry_run {
        out!(Detail, "{}", run_preview(true, cmd, &remote_cmd, &names, timeout));
        out!(Brief, "[dry-run] would run {brief}");
        if !missing.is_empty() { anyhow::bail!("no SSH endpoint for running pods: {}", missing.join(", ")); }
        return Ok(());
    }
    if confirm_needed && !confirm_brief(yes, &run_preview(false, cmd, &remote_cmd, &names, timeout), &format!("Run {brief}?"))? {
        out!(Out, "aborted.");
        return Ok(());
    }

    let results = run_fleet(&remote, targets, &remote_cmd, timeout).await;
    let (lines, bad) = render_run(&results, compact);
    let bad = bad + missing.len();
    // The pods' output as before; its last line (the tally) only with -v, or — by default — when
    // there are several pods (one pod's block says it all; a failure ends in the error).
    let (tally, blocks) = lines.split_last().map_or((None, &lines[..]), |(t, b)| (Some(t), b));
    for line in blocks {
        out!(Out, "{line}");
    }
    if let Some(t) = tally {
        out!(Detail, "{t}");
        if results.len() > 1 {
            out!(Brief, "{t}");
        }
    }
    // Propagate partial failure to the exit code, like the mutating sibling handlers — so
    // a scripted `pods test` / `pods run` can't pass while the command failed on pods.
    if bad > 0 {
        anyhow::bail!("{bad} pod(s) failed");
    }
    Ok(())
}

/// What `run` says before it runs (pure, so tested): the dry-run preview (`dry_run`) or the
/// confirm prompt — both name every resolved pod, so the operator sees exactly where an
/// arbitrary command will go.
pub(crate) fn run_preview(dry_run: bool, cmd: &str, remote_cmd: &str, names: &[&str], timeout: Duration) -> String {
    if dry_run {
        format!(
            "[dry-run] would run on {} pod(s) ({}s budget each): {}\n  {remote_cmd}",
            names.len(),
            timeout.as_secs(),
            names.join(", ")
        )
    } else {
        format!("Run `{cmd}` on {} pod(s) over SSH: {}", names.len(), names.join(", "))
    }
}

/// One pod's `pods run` / `pods test` outcome: its output (or why it failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunResult {
    pub name: String,
    /// Everything it printed: stdout, then — on a failure — the exit and stderr.
    pub text: String,
    pub ok: bool,
    /// A failure on one line, for the compact layout: `exit 1: <last stderr line>` (or the last
    /// stdout line when stderr is empty), `timed out after 90s`. `None` = it worked.
    pub why: Option<String>,
    /// The shell it ran under when the pod has no zsh (`bash`, `sh`: `ssh::take_fallback_note`).
    pub shell: Option<&'static str>,
}

/// A failed command's [`RunResult`] text and one-line reason. stdout is kept — what a command
/// printed before it failed is often what says why (a script's own progress, `grep`'s matches
/// so far); the old report dropped it and showed only stderr.
pub(crate) fn run_failure(out: &arena_core::ssh::SshOutput) -> (String, String) {
    use arena_core::ssh::strip_interactive_noise;
    let stdout = strip_interactive_noise(out.stdout.trim()).trim().to_string();
    let stderr = strip_interactive_noise(out.stderr.trim()).trim().to_string();
    let exit = if stderr.is_empty() { out.exit_label() } else { format!("{}: {stderr}", out.exit_label()) };
    let text = if stdout.is_empty() { exit } else { format!("{stdout}\n{exit}") };
    let last = |s: &str| s.lines().map(str::trim).rfind(|l| !l.is_empty()).map(String::from);
    let why = match (last(&stderr), last(&stdout)) {
        (Some(e), _) => format!("{}: {e}", out.exit_label()),
        (None, Some(o)) => format!("{} — last output: {o}", out.exit_label()),
        (None, None) => out.exit_label(),
    };
    (text, why)
}

/// Run `remote_cmd` on every target concurrently over `remote`, each within `timeout`;
/// results sorted by pod name. A non-zero exit, a timeout (`timed out after Ns`) or a
/// crashed task is a failed result — never dropped from the tally.
pub(crate) async fn run_fleet(
    remote: &Arc<dyn Remote>,
    targets: Vec<(String, SshTarget)>,
    remote_cmd: &str,
    timeout: Duration,
) -> Vec<RunResult> {
    use arena_core::ssh::strip_interactive_noise;
    let jobs = targets.into_iter().map(|(name, t)| (name, t, remote_cmd.to_string())).collect();
    let mut results = Vec::new();
    exec_each_pod(remote, jobs, timeout, |_, _, name, mut call| {
        // No zsh on the pod (a bare VM before setup, a staff box): it ran under bash or sh. The
        // wrap's note comes off stderr, to be said once beside the pod's result.
        let shell = call.as_mut().ok().and_then(|out| {
            let (shell, stderr) = arena_core::ssh::take_fallback_note(&out.stderr);
            out.stderr = stderr;
            shell
        });
        // a no-PTY shell can emit harmless job-control chatter; strip it.
        let (text, why) = match call {
            Ok(out) if out.success => (strip_interactive_noise(out.stdout.trim()).trim().to_string(), None),
            Ok(out) => {
                let (text, why) = run_failure(&out);
                (text, Some(why))
            }
            Err(why) => (why.clone(), Some(why)),
        };
        results.push(RunResult { name: name.to_string(), text, ok: why.is_none(), why, shell });
    })
    .await;
    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

/// The `pods run` / `pods test` report: `compact` = one `<name>  <last line>` row per pod
/// (`✗ <why>` on failure, e.g. `✗ timed out after 90s`), else a `── <name>` block with the
/// full output; then the `N ok, M failed` tally. A pod with no zsh has a dim `(no zsh: ran
/// under bash)` at the end of its row, or of its block's `──` line. Returns the lines and the
/// failure count. Pure, so per-pod reporting is tested without a terminal.
pub(crate) fn render_run(results: &[RunResult], compact: bool) -> (Vec<String>, usize) {
    let (mut lines, mut ok, mut bad) = (Vec::new(), 0, 0);
    for r in results {
        if r.ok {
            ok += 1
        } else {
            bad += 1
        }
        let note = r.shell.map(|s| out::dim(format!("(no zsh: ran under {s})")));
        if compact {
            let shown = match &r.why {
                None => r.text.lines().last().unwrap_or("").trim().to_string(),
                Some(why) => format!("✗ {why}"),
            };
            let note = note.map(|n| if shown.is_empty() { n } else { format!(" {n}") }).unwrap_or_default();
            lines.push(format!("{:<22} {shown}{note}", r.name));
        } else {
            let failed = if r.ok { "" } else { "(FAILED)" };
            let note = note.map(|n| if r.ok { n } else { format!(" {n}") }).unwrap_or_default();
            lines.push(format!("\n── {} {failed}{note}", r.name));
            lines.push(r.text.clone());
        }
    }
    lines.push(format!("\n{ok} ok, {bad} failed"));
    (lines, bad)
}
