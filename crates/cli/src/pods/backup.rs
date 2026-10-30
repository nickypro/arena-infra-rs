//! `pods backup`: push each pod's branch, then pull its home here; the fallback copy when a push
//! fails, and the per-pod report.

use std::sync::Arc;

use anyhow::Result;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::cron::resolve_week_day;
use crate::pods::pull::{busy_line, local_backup_dir, pull_pods, pull_roots, take_pod_locks, LockMode, PodLocks, PullRun, Pulled, POD_LOCK_WAIT};
use crate::pods::selection::{say, Selected};
use arena_core::backups::record::Outcome;
use arena_core::backups::Job;
use crate::pods::{exec_each_pod, failure_line, PodCall, BACKUP_TIMEOUT};
use crate::{confirm, resolve};

pub(crate) async fn handle_backup(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    apply: bool,
    message: Option<String>,
    // The pods to back up, resolved by the caller (`pods backup` shares it with the pull).
    sel: &Selected,
    // The pods' backup locks the caller took ([`take_pod_locks`]; empty = none): each is
    // released as that pod's push ends (RR3).
    locks: &mut PodLocks,
    // Filled with the pods whose git backup failed (for the caller's fallback copy).
    unpushed: &mut Vec<Unpushed>,
) -> Result<()> {
    use arena_core::backup;

    // Backup commits the *current* branch (never switches/creates one), so it just needs
    // the repo path + push key — no week/day / autocommit-branch naming.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE").map(String::from);
    let msg_for = |name: &str| message.clone().unwrap_or_else(|| format!("arena backup {name}"));
    // The repo every pod's `origin` must be (else WRONG_ORIGIN: nothing committed or pushed).
    let origin = backup::configured_repo(cfg);

    let missing = sel.endpoint_failures();
    // Back up only pods that actually have an SSH endpoint; report the rest.
    let targets = sel.named_targets(cfg)?;
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to back up)");
        if !missing.is_empty() { return Err(BackupFailed { pods: missing }.into()); }
        return Ok(());
    }

    if !apply {
        let checked = origin.as_deref().map_or_else(String::new, |o| format!("; a pod whose origin isn't {o} is refused"));
        println!(
            "Dry-run — would commit + push the current branch on {} pod(s) (skips main/master; files over {} MB \
             and nested repos with their own history stay out of git{checked}), running the backup script below on each:\n",
            targets.len(),
            backup::GIT_MAX_FILE_BYTES / (1024 * 1024)
        );
        // The script is the same for every pod — printed once, at the end, as a here-doc that
        // writes the file each pod's line reads (so both paste); the arguments are exact.
        for (name, target) in &targets {
            println!("# {name}");
            println!("{}\n", backup::backup_dry_run_line(target, &repo_path, key.as_deref(), &msg_for(name), origin.as_deref()));
        }
        println!(
            "# The backup script, run under sh on each pod — paste this first: it writes ./{}, which the lines above read\n{}",
            backup::BACKUP_SCRIPT_FILE,
            backup::backup_script_heredoc()
        );
        println!("\nPreview only — run without --dry-run to execute over SSH.");
        if !missing.is_empty() { return Err(BackupFailed { pods: missing }.into()); }
        return Ok(());
    }

    // Run concurrently with a live [done/total] counter (one SSH per pod, slow serially).
    // Each pod's lock (when the caller took them) goes as that pod's push ends; a name with no
    // push to run here (no endpoint) at once.
    for (name, _) in &targets {
        locks.expect(name);
    }
    locks.release_idle();
    let total = targets.len();
    println!(
        "Backing up {total} pod(s) over SSH (current branch; main/master skipped; {}s budget each)…",
        BACKUP_TIMEOUT.as_secs()
    );
    let jobs = targets
        .into_iter()
        .map(|(name, target)| {
            let cmd = backup::backup_command(&repo_path, key.as_deref(), &msg_for(&name), origin.as_deref());
            (name, target, cmd)
        })
        .collect();
    // The run record (`arena backups`): started now, each pod's outcome as it ended.
    let run = crate::backups::Run::start(cfg, Job::Git, total + missing.len(), sel.named);
    let mut t = backup_fleet(&remote, jobs, &repo_path, |line| println!("{line}"), |name| locks.done(name)).await;
    let unreached: Vec<(String, String)> = missing.iter().map(|n| (n.clone(), "no SSH endpoint after 10 minutes running".to_string())).collect();
    run.finish(t.record_events(&unreached), t.summary(missing.len()));
    t.failed.extend(missing);
    println!(
        "\nDone: {} pushed, {} unchanged, {} skipped (main/master), {} failed.",
        t.pushed,
        t.unchanged,
        t.skipped,
        t.failed.len()
    );
    // The pods on main/master, after the counts — the line the operator acts on (never a failure).
    let fleet_wide = !sel.named && t.pushed + t.unchanged == 0;
    on_protected_lines(&Naming::from_config(cfg), &t.on_protected, fleet_wide).iter().for_each(|l| say(l));
    unpushed.extend(t.unpushed);
    if !t.failed.is_empty() {
        return Err(BackupFailed { pods: t.failed }.into());
    }
    Ok(())
}

/// `pods backup`'s git step failed for these pods — whichever way (before the push: no repo, ssh
/// refused, a `set -e` exit; or at it) — said as `N pod(s) failed to back up`. It names them so
/// the step that finishes an operation left in progress ([`resolve::Settled::git`]) can take back
/// exactly the pods it finished and backed up, and no other failure (review ai-review claude #3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackupFailed {
    pub pods: Vec<String>,
}

impl std::fmt::Display for BackupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} pod(s) failed to back up", self.pods.len())
    }
}

impl std::error::Error for BackupFailed {}

/// `pods backup`: git-push the selected pods' current branches, then (unless `no_pull`)
/// rsync their homes — both over the SAME selection, resolved once by the caller. Under
/// `no_pull`, the pods whose push failed are rsynced all the same ([`copy_unpushed`]), and each
/// says where its files went; the run still fails (a copy is no git backup).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_full_backup(
    // How we reach pods for the git push: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    no_pull: bool,
    message: Option<String>,
    dry_run: bool,
    yes: bool,
    // The rsync the pull runs: [`RSYNC`] for real, a recording stub in tests.
    rsync: &str,
) -> Result<()> {
    if sel.pods.is_empty() {
        println!("(no pods to back up)");
        return Ok(());
    }
    let scope = format!("the ARENA tree of {} pod(s) ({})", sel.pods.len(), sel.names());
    let what = if no_pull {
        format!("commit + push {scope}, each on its current branch (main/master skipped)")
    } else {
        // Show the real destination subfolder (base/<wNdM>/<pod>) so it's clear where
        // the rsync lands; fall back to just the base if the wNdM label can't be
        // computed (e.g. ARENA_START_DATE unset).
        let base = local_backup_dir(cfg);
        let (big_root, _) = pull_roots(cfg, &base);
        let dest = match resolve_week_day(cfg, None, None) {
            Ok((w, d)) => format!("{base}/w{w}d{d}/<pod> (snapshot) + {big_root}/<pod> (all files)"),
            Err(_) => base,
        };
        format!("commit + push {scope} (git), then rsync the home(s) to {dest}")
    };
    if !dry_run && !confirm(yes, &format!("Will {what}."))? {
        println!("aborted.");
        return Ok(());
    }
    // 1) git push, then 2) rsync file backup (unless --no-pull). The file backup is
    // INDEPENDENT of git, so a git failure on one pod (e.g. a missing repo, or a pod
    // sitting on main) must NOT skip the rsync for the whole fleet. Capture the git
    // result, always run the pull, then surface the git error at the end. Under --no-pull the
    // pods whose push failed are pulled all the same: never zero copies.
    // Each pod's backup lock for its git push (whose SSH session must not overlap a reaper
    // probe of the pod, where it would count as activity), released as that pod's push ends
    // (RR3); the pull then takes each again for that pod's own rsyncs. A name another backup
    // holds for the whole wait is left out of both, named.
    let dir = local_backup_dir(cfg);
    let (sel, mut locks) = if dry_run {
        (sel.clone(), PodLocks::default())
    } else {
        let names: Vec<String> = sel.pods.iter().map(|p| p.name.clone()).collect();
        let locks = take_pod_locks(&dir, &names, POD_LOCK_WAIT).await;
        let kept = Selected { pods: sel.pods.iter().filter(|p| !locks.busy.contains(&p.name)).cloned().collect(), named: sel.named };
        (kept, locks)
    };
    for b in &locks.busy {
        println!("✗ {b}: {}", busy_line(b, POD_LOCK_WAIT));
    }
    // Left out of both steps: recorded so for each (`arena backups`).
    crate::backups::note_pods(
        cfg,
        locks
            .busy
            .iter()
            .flat_map(|b| {
                let why = busy_line(b, POD_LOCK_WAIT);
                let copy = (!no_pull).then(|| (b.clone(), crate::backups::event(Job::Copy, Outcome::Failed, why.clone())));
                std::iter::once((b.clone(), crate::backups::event(Job::Git, Outcome::Failed, why))).chain(copy)
            })
            .collect(),
    );
    let mut unpushed = Vec::new();
    let git_result = if sel.pods.is_empty() {
        Ok(())
    } else {
        handle_backup(remote.clone(), cfg, !dry_run, message.clone(), &sel, &mut locks, &mut unpushed).await
    };
    locks.held.clear();
    // The pods with an operation in progress (BUSY): a look at each first, which paces their copy
    // and, after it, finishes the ones left alone long enough (`resolve`).
    let busy = resolve::Busy::look(&remote, cfg, &dir, &sel, &unpushed).await;
    if !no_pull && !sel.pods.is_empty() {
        println!();
        let args = PullRun { label: None, max_size: None, remote_path: None, no_git: false, no_big: false, dry_run, yes: true };
        let pulled = pull_pods(&remote, cfg, &dir, args, &sel, rsync, LockMode::Take(POD_LOCK_WAIT)).await;
        let settled = busy.settle(&remote, cfg, &pulled, true, message.as_deref()).await;
        print_fallback_copies(&settled.unsettled(&unpushed), &pulled);
        settled.print();
        return both_failures(settled.git(git_result), pulled.and_then(Pulled::result), &locks.busy);
    }
    if no_pull && dry_run {
        println!(
            "(a pod whose git push fails at the push itself, or whose origin isn't the configured repo, is then copied into \
             {dir} by rsync — even under --no-pull)"
        );
    }
    let copying = busy.to_copy(&unpushed);
    let pulled = copy_unpushed(&remote, cfg, &dir, &sel, &copying, rsync, LockMode::Take(POD_LOCK_WAIT)).await;
    let settled = busy.settle(&remote, cfg, &pulled, false, message.as_deref()).await;
    print_fallback_copies(&settled.unsettled(&copying), &pulled);
    settled.print();
    both_failures(settled.git(git_result), pulled.and_then(Pulled::result), &locks.busy)
}

/// A pod whose git push failed must never be left with no copy at all: live, the */15 cron runs
/// `pods backup --no-pull`, so three pods GitHub refused over a pasted key had no backup for ~20
/// hours but their local commits. So the pods in `unpushed` that failed at the push itself — a
/// key GitHub refuses, a big file, the network: the work is there, not on GitHub — are rsynced
/// into `dir` with the pull's own machinery (both tiers), in the same run, under each pod's lock
/// (`locking`: taken here, or already the caller's). So are the pods whose `origin` isn't the
/// configured repo, whose work reaches no git remote at all ([`Unpushed::copied_instead`]). A pod
/// that never got that far (no repo, ssh refused, a timeout) isn't: it fails the same way every
/// run, and its plain failure line says so. Nothing to copy: `Ok` and empty.
pub(crate) async fn copy_unpushed(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    dir: &str,
    sel: &Selected,
    unpushed: &[Unpushed],
    rsync: &str,
    locking: LockMode,
) -> Result<Pulled> {
    let pods: Vec<arena_core::Pod> =
        sel.pods.iter().filter(|p| unpushed.iter().any(|u| u.copied_instead() && u.pod == p.name)).cloned().collect();
    if pods.is_empty() {
        return Ok(Pulled::default());
    }
    println!(
        "\nCopying the {} pod(s) git didn't back up into {dir} instead (rsync, both tiers): {}",
        pods.len(),
        pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    let args = PullRun { label: None, max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
    pull_pods(remote, cfg, dir, args, &Selected { pods, named: true }, rsync, locking).await
}

/// Where `pod`'s copy ([`copy_unpushed`]) went — `Err`: why it failed (its rsyncs, or the whole
/// pull's refusal).
pub(crate) fn copy_outcome(pulled: &Result<Pulled>, pod: &str) -> std::result::Result<String, String> {
    match pulled {
        Ok(p) => p.copied_to(pod).ok_or_else(|| p.failed.iter().filter(|(q, _)| q == pod).map(|(_, w)| w.as_str()).collect::<Vec<_>>().join(", ")),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// One line per pod copied instead of its git backup ([`Unpushed::copied_instead`]): why, and
/// where its files went instead — or that the copy failed too, and why (both failures, on the
/// pod's own line).
pub(crate) fn fallback_lines(unpushed: &[Unpushed], pulled: &Result<Pulled>) -> Vec<String> {
    unpushed
        .iter()
        .filter(|u| u.copied_instead())
        .map(|u| {
            let copy = match copy_outcome(pulled, &u.pod) {
                Ok(to) => format!(" → files copied to {to}"),
                Err(why) => format!("; the fallback copy failed too: {why}"),
            };
            // Nothing was committed (a wrong origin, an operation in progress): no push to fail.
            let failed = if u.wrong_origin || u.busy.is_some() { "not backed up to git" } else { "git push failed" };
            format!("✗ {}: {failed} ({}){copy}", u.pod, u.why)
        })
        .collect()
}

fn print_fallback_copies(unpushed: &[Unpushed], pulled: &Result<Pulled>) {
    let lines = fallback_lines(unpushed, pulled);
    if !lines.is_empty() {
        println!("\n{}", lines.join("\n"));
    }
}

/// `pods backup`'s result: the git push's failure and the copy's, both when both failed (a
/// fallback copy is no git backup: the run still fails), then the pods another backup held.
fn both_failures(git: Result<()>, pull: Result<()>, busy: &[String]) -> Result<()> {
    match (git, pull) {
        (Err(g), Err(p)) => anyhow::bail!("{g:#}; {p:#}"),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => return Err(e),
        (Ok(()), Ok(())) => {}
    }
    if !busy.is_empty() {
        anyhow::bail!("{} pod(s) not backed up — another backup held them: {}", busy.len(), busy.join(", "));
    }
    Ok(())
}

/// The rsync binary `pull`/`backup` run (from PATH).
pub(crate) const RSYNC: &str = "rsync";

/// What a fleet `backup` did, per outcome.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BackupTally {
    pub pushed: usize,
    pub unchanged: usize,
    pub skipped: usize,
    /// The pods that failed, by name (in finishing order), whichever way.
    pub failed: Vec<String>,
    /// The pods whose git push failed — for the copy the backup makes of them instead.
    pub unpushed: Vec<Unpushed>,
    /// The `skipped` pods, each with the protected branch it's on (`(name, branch)`): said in
    /// one line after the run ([`on_protected_lines`]), not one per pod.
    pub on_protected: Vec<(String, String)>,
    /// The `skipped` pods with work on that branch no backup saves (`UNCOMMITTED`: an operation in
    /// progress, changes to tracked files, commits no remote has, untracked files): what
    /// `set-branch --hard` copies before its reset ([`switch_guarded`]). `pods backup` leaves them
    /// as it always has — skipped, not a failure, and not said: the cron's output is unchanged.
    pub uncommitted: Vec<Unpushed>,
    /// Every pod's outcome, in finishing order — for the run record (`arena backups`).
    pub events: Vec<(String, Outcome, String)>,
}

impl BackupTally {
    /// Its events for the run record, plus `failed` pods it never reached (`(pod, why)`: no
    /// endpoint, its lock busy).
    pub(crate) fn record_events(&self, failed: &[(String, String)]) -> Vec<(String, arena_core::backups::record::Event)> {
        let ev = |o, d: &str| crate::backups::event(Job::Git, o, d);
        self.events.iter().map(|(p, o, d)| (p.clone(), ev(*o, d))).chain(failed.iter().map(|(p, why)| (p.clone(), ev(Outcome::Failed, why)))).collect()
    }

    /// The run in a few words: `1 pushed, 2 no changes, 41 on main/master, 1 failed`.
    pub(crate) fn summary(&self, more_failed: usize) -> String {
        format!(
            "{} pushed, {} no changes, {} on main/master, {} failed",
            self.pushed,
            self.unchanged,
            self.skipped,
            self.failed.len() + more_failed
        )
    }
}

/// A pod whose git backup failed this run: why (a few words), the branch it left unpushed (when
/// the backup got far enough to say), whether it failed at the push itself
/// ([`arena_core::backup::BackupReport::failed_at_push`]), and whether its `origin` isn't the
/// configured repo (`WRONG_ORIGIN`: nothing was committed or pushed). Those two are the failures
/// `pods backup` copies by rsync instead ([`copy_unpushed`]). A pod with no repo, or ssh refused,
/// fails the same way every run and isn't. `busy`: what was in progress on the branch (`BUSY`: a
/// merge, …), so nothing was committed — a failure at the push for the copy, said as what it is;
/// for a pod on a branch no backup commits, the work there no backup saves (`UNCOMMITTED`: the
/// same words, or `changes`, `commits`, `untracked`), which a `--hard` switch copies first
/// ([`BackupTally::uncommitted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unpushed {
    pub pod: String,
    pub why: String,
    pub branch: Option<String>,
    pub at_push: bool,
    pub wrong_origin: bool,
    pub busy: Option<String>,
}

impl Unpushed {
    /// Whether `pods backup` copies this pod by rsync instead of its git backup. A failure at the
    /// push, as before. A wrong `origin` too: its work reaches no git remote at all, and fails the
    /// same way every run until `pods setup`, so without a copy it would have none anywhere but
    /// on the pod (the */15 cron runs `--no-pull`). It is a cohort pod the backup reached, its
    /// repo there: unlike a pod with no repo or no ssh, the copy can work.
    fn copied_instead(&self) -> bool {
        self.at_push || self.wrong_origin
    }
}

/// Why a pod's call failed, in a few words: its exit and the most telling line it wrote to
/// stderr ([`telling_line`]), or why there was no answer (`timed out after 300s`, …).
pub(crate) fn failure_why(call: &PodCall) -> String {
    match call {
        Ok(out) => match telling_line(&out.stderr) {
            Some(line) => format!("{}: {line}", out.exit_label()),
            None => out.exit_label(),
        },
        Err(why) => why.clone(),
    }
}

/// The stderr line that says what went wrong: the first one that starts (trimmed) like git's or
/// ssh's own verdict — `ERROR:`, `fatal:`, `error:`, `remote: error`, `! [rejected]`,
/// `! [remote rejected]`, `ssh:` — or that holds ssh's `Permission denied (publickey`, else the
/// last non-empty line. The last line alone is often the tail of a longer message (live: a push to a missing
/// repo gave `and the repository exists.`, the end of git's advice, where its first line said
/// `ERROR: Repository not found.`). ssh's own line comes before git's `fatal: Could not read
/// from remote repository.` and names the cause that one hides: `git@github.com: Permission
/// denied (publickey).` (live: the deploy key gone), `ssh: connect to host … timed out`. Only
/// ssh's wording: a `warning: could not open directory …: Permission denied` from `git add`
/// says nothing about a failed push (review fb2 R5). `None` for an empty stderr. Pure.
pub(crate) fn telling_line(stderr: &str) -> Option<&str> {
    const VERDICTS: &[&str] = &["ERROR:", "fatal:", "error:", "remote: error", "! [rejected]", "! [remote rejected]", "ssh:"];
    let lines = || stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    lines()
        .find(|l| VERDICTS.iter().any(|v| l.starts_with(v)) || l.contains("Permission denied (publickey"))
        .or_else(|| lines().next_back())
}

/// Is `branch` one `pods backup` never commits on (`backup.sh`'s SKIP: main, master, a
/// detached HEAD)?
pub(crate) fn is_protected_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "HEAD")
}

/// The one line for this cohort's pods sitting on a protected branch (`(name, branch)`), which
/// no backup commits until `init-branches` gives each its day branch — the daily
/// `set-branch main --hard` leaves the whole fleet there. One line, not one SKIP per pod, and it
/// says what ends it. `fleet_wide`: the run named no pods and found none on a branch backups
/// commit on (the morning reset), so the fix is the plain `arena pods init-branches`; otherwise
/// it names these pods, since a fleet-wide init would also move a pod off a bespoke branch.
///
/// Not a failure (the exit stays 0): the pods are where the operator put them, their work stays
/// on disk, and the window between the reset and `init-branches` is routine every morning — a
/// non-zero exit there would be a daily false alarm that teaches the operator to ignore the one
/// that means a push or a pod broke. The cron's exit status goes nowhere anyway (its line
/// appends to `~/.local/state/arena-cron/backup.log`); this line, and `set-branch`'s reminder at the reset itself,
/// are what make the gap loud.
pub(crate) fn on_protected_line(pods: &[(String, String)], fleet_wide: bool) -> Option<String> {
    if pods.is_empty() {
        return None;
    }
    let branches: std::collections::BTreeSet<&str> = pods.iter().map(|(_, b)| b.as_str()).collect();
    let on = branches.into_iter().map(|b| if b == "HEAD" { "a detached HEAD" } else { b }).collect::<Vec<_>>().join(" or ");
    let cmd = if fleet_wide {
        "arena pods init-branches".to_string()
    } else {
        // Sorted: the pods finish in any order.
        let mut names: Vec<&str> = pods.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        format!("arena pods init-branches {}", names.join(" "))
    };
    let n = pods.len();
    Some(format!("⚠ {n} cohort pod{} on {on}: not backed up until `{cmd}`", if n == 1 { "" } else { "s" }))
}

/// What `pods backup` says, after its run, about the pods it found on a protected branch
/// (`(name, branch)`): this cohort's in one line ([`on_protected_line`]); a staff or off-list
/// box (only ever there because it was named) one line each, as before — `init-branches`
/// is not its fix, and it isn't counted with the cohort's. `fleet_wide` as for
/// [`on_protected_line`].
pub(crate) fn on_protected_lines(naming: &Naming, pods: &[(String, String)], fleet_wide: bool) -> Vec<String> {
    let (cohort, mut other): (Vec<_>, Vec<_>) = pods.iter().cloned().partition(|(n, _)| naming.is_cohort(n));
    other.sort();
    let mut out: Vec<String> = other.iter().map(|(n, b)| format!("⊘ {n} (skipped — on protected branch {b})")).collect();
    out.extend(on_protected_line(&cohort, fleet_wide));
    out
}

/// Run each pod's backup command (`(name, target, cmd)`) concurrently over `remote`, each
/// within [`BACKUP_TIMEOUT`], classifying its output by the sentinel it printed and
/// emitting one `[done/total]` line per pod as it finishes — with what the run kept out of
/// git (files over GitHub's limit, nested repos) or adopted into it; a pod on a protected
/// branch goes into the tally instead, for one line after the run. A non-zero exit, a
/// timeout or a crashed task is a failure; the others still finish. A pod whose unpushed
/// commits already carry a file over GitHub's limit fails with the exact commands that fix it
/// (`repo` is the checkout they run in), never as the same opaque push rejection every tick;
/// one GitHub refuses over a key the redaction doesn't know says which kind and where. Every
/// failed pod is in the tally's `unpushed`, for the copy its caller makes instead.
pub(crate) async fn backup_fleet(
    remote: &Arc<dyn Remote>,
    jobs: Vec<(String, SshTarget, String)>,
    repo: &str,
    mut emit: impl FnMut(&str),
    // Each pod's name as its push ends (its backup lock is released there).
    mut ended: impl FnMut(&str),
) -> BackupTally {
    use arena_core::backup::{self, parse_backup_report};
    let mut t = BackupTally::default();
    exec_each_pod(remote, jobs, BACKUP_TIMEOUT, |done, total, name, call| {
        let report = call.as_ref().map(|out| parse_backup_report(&out.stdout)).unwrap_or_default();
        let notes = report.notes();
        let also = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
        let line = match &call {
            Ok(out) if out.success => match &report.outcome {
                Some((backup::BACKUP_PUSHED, branch)) => {
                    t.pushed += 1;
                    t.events.push((name.clone(), Outcome::Pushed, format!("→ {branch}")));
                    format!("[{done}/{total}] ✓ {name} -> {branch}{also}")
                }
                Some((backup::BACKUP_NO_CHANGES, branch)) => {
                    t.unchanged += 1;
                    t.events.push((name.clone(), Outcome::NoChanges, format!("on {branch}")));
                    format!("[{done}/{total}] = {name} (no changes, on {branch}){also}")
                }
                Some((backup::BACKUP_SKIPPED, branch)) => {
                    t.skipped += 1;
                    t.events.push((name.clone(), Outcome::NotSaved, format!("on {}", if branch == "HEAD" { "a detached HEAD" } else { branch.as_str() })));
                    // Said once for all of them after the run (`on_protected_lines`).
                    t.on_protected.push((name.to_string(), branch.clone()));
                    if let Some(why) = report.uncommitted_why() {
                        let (pod, branch, busy) = (name.clone(), Some(branch.clone()), report.uncommitted.clone());
                        t.uncommitted.push(Unpushed { pod, why, branch, at_push: true, wrong_origin: false, busy });
                    }
                    String::new()
                }
                _ => {
                    t.pushed += 1;
                    t.events.push((name.clone(), Outcome::Pushed, String::new()));
                    format!("[{done}/{total}] ✓ {name} (done){also}")
                }
            },
            // A failure the script explained (a big file or a key GitHub refuses, a heal cut short,
            // an operation in progress).
            Ok(out) if report.failure(repo, name, out.code).is_some() => {
                t.failed.push(name.clone());
                let why = report.failure_brief(out.code).unwrap_or_else(|| failure_why(&call));
                let wrong_origin = report.wrong_origin_why(out.code).is_some();
                let busy = report.in_progress(out.code).map(String::from);
                t.events.push((name.clone(), Outcome::Failed, why.clone()));
                t.unpushed.push(Unpushed { pod: name.clone(), why, branch: report.branch.clone(), at_push: report.failed_at_push(), wrong_origin, busy });
                format!("[{done}/{total}] ✗ {name}: {}", report.failure(repo, name, out.code).unwrap_or_default())
            }
            _ => {
                t.failed.push(name.clone());
                let at_push = report.failed_at_push();
                t.events.push((name.clone(), Outcome::Failed, failure_why(&call)));
                t.unpushed.push(Unpushed { pod: name.clone(), why: failure_why(&call), branch: report.branch.clone(), at_push, wrong_origin: false, busy: None });
                failure_line(done, total, name, &call)
            }
        };
        if !line.is_empty() {
            emit(&line);
        }
        ended(name);
    })
    .await;
    t
}
