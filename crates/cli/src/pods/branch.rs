//! `pods set-branch` and `pods init-branches`: switch pods' git branch without losing work
//! (pushed or saved first, guarded), and create each pod's autocommit branch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::confirm;
use crate::cron::resolve_week_day;
use crate::pods::backup::{backup_fleet, copy_outcome, copy_unpushed, is_protected_branch, on_protected_line, Unpushed};
use crate::pods::pull::{busy_line, local_backup_dir, switch_unlocked_line, take_pod_locks, LockMode, PodLocks, POD_LOCK_WAIT};
use crate::pods::selection::{say, Selected};
use crate::pods::{exec_each_pod, failure_line, BACKUP_TIMEOUT, BRANCH_TIMEOUT};

/// What `pods set-branch` was asked: the branch, `--hard`, `--dry-run`, `--yes`, and
/// `--init-branches` (init the pods it switched, once every one has had its switch).
pub(crate) struct SetBranchRun<'a> {
    pub branch: &'a str,
    pub hard: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub init_branches: bool,
}

/// How a `set-branch` run ended ([`switch_branch`]), for `--init-branches` to go on from.
enum SwitchEnd {
    /// A dry run: the init's dry run follows, for every selected pod.
    Previewed,
    /// Nothing switched — no pod to switch, or the operator said no: no init either.
    Stopped,
    /// The switches ran: the pods that switched (`landed`, their branch copied first or not), and
    /// the run's verdict ([`Switched::result`]).
    Ran { landed: Vec<String>, verdict: Result<()> },
}

/// `pods set-branch`, then — with `--init-branches` — `pods init-branches` on every pod that
/// switched. A pod that switched is on the branch like the rest, whatever its switch said: one
/// whose push failed (the network, push protection, a non-fast-forward, an operation in progress)
/// and was copied first included — so one participant's failed push never leaves the whole fleet
/// on main, where no backup commits (review pr4; fb2 F2 for a merge in progress). A pod that
/// didn't switch (no copy, its switch failed, the gentle switch refused it, its lock was busy, the
/// wrong origin) is left out: an init would branch its day from where it still is. The run
/// still fails after the init, with set-branch's verdict (naming every pod copied instead of
/// pushed, and every one not switched) and then the init's own.
pub(crate) async fn set_branch_and_init(remote: Arc<dyn Remote>, cfg: &Config, sel: &Selected, how: &SetBranchRun<'_>, rsync: &str) -> Result<()> {
    let (landed, verdict) = match switch_branch(remote.clone(), cfg, sel, how, rsync).await? {
        SwitchEnd::Previewed if how.init_branches => {
            println!();
            return handle_init_branches(remote, cfg, None, None, sel, true, how.yes, rsync).await;
        }
        SwitchEnd::Previewed | SwitchEnd::Stopped => return Ok(()),
        SwitchEnd::Ran { landed, verdict } => (landed, verdict),
    };
    if !how.init_branches {
        return verdict;
    }
    println!();
    let init = match init_selection(sel, &landed) {
        None => {
            say("init-branches NOT run: no pod switched");
            Ok(())
        }
        Some((on, left_out)) => {
            if !left_out.is_empty() {
                say(&format!(
                    "init-branches on the {} pod(s) that switched — not on {}, which didn't (see each one's line above)",
                    on.pods.len(),
                    left_out.join(", ")
                ));
            }
            handle_init_branches(remote, cfg, None, None, &on, false, how.yes, rsync).await
        }
    };
    match (verdict, init) {
        (Ok(()), init) => init,
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(i)) => Err(anyhow::anyhow!("{e}; then init-branches: {i}")),
    }
}

/// The pods `--init-branches` inits after a set-branch run: those of `sel` that switched
/// (`landed`), in selection order, and the names of the rest — left out. `None` when none did.
/// Pure.
fn init_selection(sel: &Selected, landed: &[String]) -> Option<(Selected, Vec<String>)> {
    let (on, out): (Vec<arena_core::Pod>, Vec<arena_core::Pod>) = sel.pods.iter().cloned().partition(|p| landed.contains(&p.name));
    (!on.is_empty()).then(|| (Selected { pods: on, named: sel.named }, out.into_iter().map(|p| p.name).collect()))
}

/// `pods set-branch` alone, as its tests drive it: [`switch_branch`] and its verdict.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_set_branch(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    branch: &str,
    sel: &Selected,
    hard: bool,
    dry_run: bool,
    yes: bool,
    rsync: &str,
) -> Result<()> {
    let how = SetBranchRun { branch, hard, dry_run, yes, init_branches: false };
    match switch_branch(remote, cfg, sel, &how, rsync).await? {
        SwitchEnd::Ran { verdict, .. } => verdict,
        SwitchEnd::Previewed | SwitchEnd::Stopped => Ok(()),
    }
}

/// Switch the selected pods (those with an SSH endpoint) to `how.branch` over SSH.
/// Gentle (fetch+checkout+fast-forward to origin's branch) by default; `how.hard` force-resets
/// to `origin/<branch>`, discarding local commits/changes (saved on the pod first, its line
/// saying where: [`discarded_note`]). Dry-run prints the exact command per pod. Pods run
/// concurrently, each within [`BRANCH_TIMEOUT`] (it used to be serial, so one wedged pod
/// held up every pod after it). `rsync`: the rsync a pod whose branch can't be pushed is copied
/// with ([`RSYNC`], a stub in tests); `remote`: `SshRemote` for real, `FakeRemote` in tests.
async fn switch_branch(remote: Arc<dyn Remote>, cfg: &Config, sel: &Selected, how: &SetBranchRun<'_>, rsync: &str) -> Result<SwitchEnd> {
    let SetBranchRun { branch, hard, dry_run, yes, .. } = *how;
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE");
    // `--hard`'s save is bounded by DISCARD_MAX_GB (a value it can't read stops the run here,
    // before anything is backed up or switched); the gentle switch saves nothing.
    let cmd = if hard {
        let max = arena_core::backup::discard_max_kb(cfg).map_err(anyhow::Error::msg)?;
        arena_core::backup::checkout_command_limited(&repo_path, branch, key, hard, max)
    } else {
        arena_core::backup::checkout_command(&repo_path, branch, key, hard)
    };

    // Unreachable pods are reported — a silently-skipped pod left on a stale/diverged branch
    // (especially under --hard) is exactly what we don't want.
    let targets = sel.named_targets(cfg)?;
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to switch)");
        return Ok(SwitchEnd::Stopped);
    }

    if dry_run {
        println!("{}\n", push_first_note(cfg, hard));
        for (name, t) in &targets {
            println!("# {name}");
            println!("{}\n", t.display_command(&cmd));
        }
        println!("Dry-run only — nothing changed (preview).");
        return Ok(SwitchEnd::Previewed);
    }
    let what = if hard {
        format!("HARD-reset {} pod(s) to 'origin/{branch}' — DISCARDS local commits/changes", targets.len())
    } else {
        format!("switch {} pod(s) to branch '{branch}' (gentle, no reset)", targets.len())
    };
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    // Before the question, so it's read before the answer (and in the run's output with --yes).
    if let Some(line) = switch_unlocked_line(cfg) {
        say(&line);
    }
    if !confirm(yes, &format!("Will {what} (each pod's current branch committed + pushed first): {names}"))? {
        println!("aborted.");
        return Ok(SwitchEnd::Stopped);
    }
    let total = targets.len();
    let doing = format!("to '{branch}'");
    let mut landed: Vec<String> = Vec::new();
    let run = switch_guarded(&remote, cfg, sel, targets, rsync, hard.then_some(branch), |_| cmd.clone(), BRANCH_TIMEOUT, &doing, |name, out, note| {
        // Nested clones the backup adopted, which the new branch doesn't track: put back on
        // disk (staged) so their exercises keep working.
        let restored: Vec<&str> = out.stdout.lines().filter_map(|l| l.trim_end_matches('\r').strip_prefix("RESTORED ")).collect();
        landed.push(name.to_string());
        format!(
            "✓ {name} (on {branch}){}{}{note}{}",
            restored_note(&restored, branch),
            unredacted_note(&out.stdout),
            discarded_note(&out.stdout)
        )
    })
    .await;
    println!("\nswitched {}/{total}", run.ok);
    if let Some(line) = run.ended_line() {
        say(&line);
    }
    if let Some(line) = set_branch_reminder(&Naming::from_config(cfg), branch, &landed, !sel.named) {
        say(&line);
    }
    Ok(SwitchEnd::Ran { landed, verdict: run.result("switch branch") })
}

/// What a dry run of a branch switch says first: the push that comes before it — under each
/// pod's backup lock, or, when none is taken, the one line that says so
/// ([`switch_unlocked_line`]) — and what happens to a pod with a merge in progress, which can't
/// be committed first: copied too, then the gentle switch (and init-branches) refuses to end it,
/// while `hard` (`--hard`) ends it with its uncommitted changes to tracked files (review F5: the
/// note said every such pod "is copied … and then switched") — and copies a pod on a branch no
/// backup commits with work there too, untracked files included: `--hard` overwrites one the new
/// branch tracks (review pr4: the note said untracked files stay).
pub(crate) fn push_first_note(cfg: &Config, hard: bool) -> String {
    let origin = arena_core::backup::configured_repo(cfg).map_or_else(String::new, |o| format!(", nor one whose origin isn't {o}"));
    let unlocked = switch_unlocked_line(cfg);
    let (under_lock, or_busy) = match unlocked {
        Some(_) => ("", ""),
        None => (" — under the pod's backup lock, held until its switch ends", ", or whose lock another backup holds,"),
    };
    let busy = if hard {
        "the --hard switch then ends it, discarding its uncommitted changes to tracked files; so is a pod on main/master or \
         a detached HEAD, which no backup commits, with an operation in progress, changes to tracked files, commits no \
         remote has or untracked files (the --hard switch overwrites any the new branch tracks); on every pod, what the \
         --hard reset discards is saved there first, in .git/arena-discarded/<time>/"
    } else {
        "then NOT switched: the switch refuses to end it"
    };
    let mut note = format!(
        "(first, each pod's current branch is backed up — the `pods backup --no-pull` commit + push, keys redacted{under_lock}; \
         a pod whose push fails is copied into {dir} and then switched, its line saying so; a pod with a merge (cherry-pick, \
         revert, `git am`, rebase, conflicts) in progress can't be committed: it is copied into {dir} too, and {busy}; a pod that \
         can be neither pushed nor copied{or_busy} is NOT switched{origin})",
        dir = local_backup_dir(cfg)
    );
    if let Some(line) = unlocked {
        note.push('\n');
        note.push_str(&line);
    }
    note
}

/// How a guarded branch switch went ([`switch_guarded`]).
#[derive(Debug, Default)]
struct Switched {
    /// Switched (the command succeeded).
    ok: usize,
    /// The switch command itself failed on these.
    failed: Vec<String>,
    /// Not switched: their branch could be neither pushed nor copied, their origin isn't the
    /// configured repo, or another backup held their lock — `(pod, why)`.
    not_switched: Vec<(String, String)>,
    /// Copied, their push having failed, before their switch (in `failed` too when it failed).
    copied: Vec<String>,
    /// Of `copied`, those with work a `--hard` switch discards on purpose — an operation in
    /// progress (`BUSY`), or work on a branch no backup commits (`UNCOMMITTED`: changes, commits
    /// no remote has, untracked files): nothing failed there, so once switched they are switched
    /// like the rest (review fb2 F2), the copy holding that work, said on their line and in
    /// [`Switched::ended_line`].
    ended: Vec<String>,
}

impl Switched {
    /// The command's result: every pod switched after its push, else what went wrong (`verb`:
    /// what a failed switch failed to do — "switch branch", "init branch"). A pod copied because
    /// its push failed and then switched is named here, so the run fails and the operator sees it
    /// — but it did switch: `--init-branches` still inits it ([`set_branch_and_init`]).
    fn result(self, verb: &str) -> Result<()> {
        let mut problems = Vec::new();
        if !self.failed.is_empty() {
            problems.push(format!("{} pod(s) failed to {verb}", self.failed.len()));
        }
        if !self.not_switched.is_empty() {
            // The reasons differ (no push and no copy, the wrong origin, a busy lock): each pod's
            // own line has its why, so this one only counts and names them.
            let names: Vec<&str> = self.not_switched.iter().map(|(n, _)| n.as_str()).collect();
            problems.push(format!(
                "{} pod(s) NOT switched — each one's line says why: {}",
                names.len(),
                names.join(", ")
            ));
        }
        // Copied, then switched — a copied pod whose switch failed (one that refuses to end a
        // merge in progress) is counted with the failures above, never said to be switched; one
        // whose uncommitted work --hard ended on purpose is switched like the rest (`ended`).
        let copied: Vec<&str> =
            self.copied.iter().filter(|p| !self.failed.contains(p) && !self.ended.contains(p)).map(String::as_str).collect();
        if !copied.is_empty() {
            problems.push(format!(
                "{} pod(s) switched with their branch copied, not pushed (see each one's line): {}",
                copied.len(),
                copied.join(", ")
            ));
        }
        if !problems.is_empty() {
            anyhow::bail!("{}", problems.join("; "));
        }
        Ok(())
    }

    /// The pods `--hard` switched after copying the work no backup had saved (`ended`, its switch
    /// done): one line after the run, since they aren't failures — each pod's own line says what
    /// that work was, where its copy went and where on the pod the switch saved what it discarded
    /// (`.git/arena-discarded/<time>/`, which no later copy overwrites — the copy's label is
    /// re-pulled from the reset pod: review ai-review claude #2). `None` when there are none.
    fn ended_line(&self) -> Option<String> {
        let names: Vec<&str> = self.ended.iter().filter(|p| !self.failed.contains(p)).map(String::as_str).collect();
        (!names.is_empty()).then(|| {
            format!(
                "⚠ {} pod(s) had work no backup had saved (an operation in progress, or changes, commits or untracked files on a \
                 branch no backup commits), copied before the --hard switch discarded or overwrote it — what it discarded is also \
                 saved on the pod, in .git/arena-discarded/<time>/, which no later copy overwrites (see each one's line): {}",
                names.len(),
                names.join(", ")
            )
        })
    }
}

/// A branch switch (`set-branch`, `init-branches`) that never leaves a pod's work in a local
/// branch alone (live: the daily `set-branch main --hard` moved three pods off branches GitHub
/// had refused for ~20 hours, one due for host maintenance that afternoon). Each pod's backup
/// lock is taken first and held until its own switch ends, so no other backup (the cron's, a
/// pull, the reaper's) runs on it in between — when LOCAL_BACKUP_DIR is an absolute directory
/// that exists; otherwise none is taken ([`pod_locks_off`]) and the caller says so
/// ([`switch_unlocked_line`]); then the branch it's on is backed up — commit,
/// redact, push (a no-op on `main`/`master` or a branch in sync) — and the pods it pushed are
/// switched. A pod whose push failed at the push itself is copied into LOCAL_BACKUP_DIR under the
/// lock it still holds, then switched, its line saying so ([`not_pushed_note`]); before a `--hard`
/// reset, so is a pod on a branch no backup commits (main, …) with work there a reset could
/// discard or overwrite (`UNCOMMITTED`: [`BackupTally::uncommitted`]; review fb2 F3, pr4). A pod with
/// neither a push nor a copy — the copy failed too, the backup failed before the push, or
/// another backup held its lock for the whole wait — is NOT switched: `--hard` would throw away
/// what nothing has saved. `reset_onto`: the branch a `--hard` switch resets (a pod already on it
/// loses its unpushed commits from the branch — the switch pins them on the pod first, by a ref
/// under refs/arena-discarded/ — and its line says so).
#[allow(clippy::too_many_arguments)]
async fn switch_guarded(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    targets: Vec<(String, SshTarget)>,
    rsync: &str,
    reset_onto: Option<&str>,
    cmd_for: impl Fn(&str) -> String,
    budget: Duration,
    // Where the pods go, for the line before each batch (`Switching 3 pod(s) to 'main' …`).
    doing: &str,
    // A switched pod's line after `[done/total] `: from its name, the switch's output and the
    // note about its push (empty when it was pushed).
    mut ok_line: impl FnMut(&str, &arena_core::ssh::SshOutput, &str) -> String,
) -> Switched {
    let dir = local_backup_dir(cfg);
    let repo = arena_core::backup::repo_path(cfg);
    let key = cfg.get("GIT_SSH_KEY_REMOTE");
    let mut run = Switched::default();
    let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
    let mut locks = take_pod_locks(&dir, &names, POD_LOCK_WAIT).await;
    for b in &locks.busy {
        let why = format!("{} — nothing could be pushed or copied first", busy_line(b, POD_LOCK_WAIT));
        println!("✗ {b} not switched: {why}");
        run.not_switched.push((b.clone(), why));
    }
    let targets: Vec<(String, SshTarget)> = targets.into_iter().filter(|(n, _)| !locks.busy.contains(n)).collect();
    for (n, _) in &targets {
        locks.expect(n);
    }
    locks.release_idle();
    if targets.is_empty() {
        return run;
    }
    println!(
        "Backing up the branch each pod is on before the switch (commit + push; main/master skipped; {}s budget each)…",
        BACKUP_TIMEOUT.as_secs()
    );
    // A pod whose `origin` isn't the configured repo is refused by its backup (nothing pushed),
    // so it is never switched: `init-branches` would push its new branch to that origin, and
    // `set-branch` would fetch (and `--hard` reset to) that repo's branch.
    let origin = arena_core::backup::configured_repo(cfg);
    let jobs = targets
        .iter()
        .map(|(n, t)| (n.clone(), t.clone(), arena_core::backup::backup_command(&repo, key, &format!("arena backup {n}"), origin.as_deref())))
        .collect();
    // Each pod's lock stays held through its push: it goes as that pod's switch ends. Recorded as
    // any git backup is (`arena backups`).
    let record = crate::backups::Run::start(cfg, arena_core::backups::Job::Git, targets.len(), crate::backups::scope_of(sel.named));
    let t = backup_fleet(remote, jobs, &repo, |l| println!("{l}"), |_| {}).await;
    record.finish(t.record_events(&[]), t.summary(0));
    // Before a --hard reset, a pod on a branch no backup commits (main, …) with work there — an
    // operation in progress, changes to tracked files, commits no remote has, untracked files — is
    // copied first like a failed push, .git and all, and not reset if it can't be (review fb2 F3,
    // pr4). A gentle switch or init-branches discards nothing (git refuses to overwrite an
    // untracked file there).
    let mut unpushed = t.unpushed;
    if reset_onto.is_some() {
        unpushed.extend(t.uncommitted);
    }
    let pushed: Vec<(String, SshTarget)> = targets.iter().filter(|(n, _)| !unpushed.iter().any(|u| &u.pod == n)).cloned().collect();
    let none = std::collections::HashMap::new();
    switch_batch(remote, pushed, &cmd_for, budget, doing, &none, &mut ok_line, &mut locks, &mut run).await;
    // The rest: copied first, under the lock each still holds — or not switched at all. A pod
    // with the wrong origin is neither copied nor switched: nothing on it changes here.
    let at_push: Vec<Unpushed> = unpushed.iter().filter(|u| u.at_push).cloned().collect();
    let pulled = copy_unpushed(remote, cfg, &dir, sel, &at_push, rsync, LockMode::Held).await;
    let mut notes = std::collections::HashMap::new();
    for u in &unpushed {
        let copied = if u.at_push {
            copy_outcome(&pulled, &u.pod).map_err(|why| format!("copying its files into {dir} failed too: {why}"))
        } else {
            Err("the backup failed before the push, so nothing was copied either".to_string())
        };
        match copied {
            Ok(to) => {
                notes.insert(u.pod.clone(), not_pushed_note(u, &to, reset_onto));
                run.copied.push(u.pod.clone());
                // --hard ends the merge / discards those changes on purpose, the copy holding them.
                if u.busy.is_some() && reset_onto.is_some() {
                    run.ended.push(u.pod.clone());
                }
            }
            Err(also) => {
                let why = if u.wrong_origin {
                    format!("{} — not backed up to git; run `arena pods setup {}`", u.why, u.pod)
                } else if u.busy.is_some() {
                    format!("its work couldn't be committed ({}) and {also}", u.why)
                } else {
                    format!("its branch couldn't be pushed ({}) and {also}", u.why)
                };
                say(&format!("✗ {} not switched: {why}", u.pod));
                run.not_switched.push((u.pod.clone(), why));
                locks.done(&u.pod);
            }
        }
    }
    let copied: Vec<(String, SshTarget)> = targets.into_iter().filter(|(n, _)| notes.contains_key(n)).collect();
    switch_batch(remote, copied, &cmd_for, budget, doing, &notes, &mut ok_line, &mut locks, &mut run).await;
    run
}

/// One batch of [`switch_guarded`]'s switches, concurrently within `budget`: each pod's line
/// (with its `notes` entry) and its lock released as its switch ends.
#[allow(clippy::too_many_arguments)]
async fn switch_batch(
    remote: &Arc<dyn Remote>,
    pods: Vec<(String, SshTarget)>,
    cmd_for: &impl Fn(&str) -> String,
    budget: Duration,
    doing: &str,
    notes: &std::collections::HashMap<String, String>,
    ok_line: &mut impl FnMut(&str, &arena_core::ssh::SshOutput, &str) -> String,
    locks: &mut PodLocks,
    run: &mut Switched,
) {
    if pods.is_empty() {
        return;
    }
    println!("\nSwitching {} pod(s) {doing} over SSH ({}s budget each)…", pods.len(), budget.as_secs());
    let jobs = pods
        .into_iter()
        .map(|(n, t)| {
            let cmd = cmd_for(&n);
            (n, t, cmd)
        })
        .collect();
    exec_each_pod(remote, jobs, budget, |done, total, name, call| {
        let note = notes.get(name).map_or("", String::as_str);
        match &call {
            Ok(out) if out.success => {
                say(&format!("[{done}/{total}] {}", ok_line(name, out, note)));
                run.ok += 1;
            }
            _ => {
                say(&format!("{}{note}", failure_line(done, total, name, &call)));
                run.failed.push(name.clone());
            }
        }
        locks.done(name);
    })
    .await;
}

/// The loud part of a pod's line when it was switched with its branch copied, not pushed: which
/// branch, why, where its files went — and where its commits are now (`reset_onto`: the branch a
/// `--hard` switch resets; on that branch, its unpushed commits are pinned on the pod by
/// `refs/arena-discarded/<time>-<branch>`, and a detached HEAD's by `refs/arena-discarded/<time>`
/// — review ai-review codex #3: they were left to the reflog, and git's gc after it; a gentle
/// switch from a detached HEAD leaves its commits in the pod's HEAD reflog).
///
/// A pod with an operation in progress (`u.busy`: a merge, …) had nothing committed at all: its
/// line says what is in progress instead of a push's failure. A gentle switch (and init-branches)
/// refuses to end it, on the same line ([`arena_core::backup::checkout_command`]); `--hard`
/// (`reset_onto` set) ends it and discards its uncommitted changes to tracked files with it — said
/// here; the switch saves them on the pod first, and its own line says where ([`discarded_note`]).
/// The copy isn't said to be the only one (review ai-review claude #2: it is the day's snapshot
/// label, which the next pull of that label overwrites from the reset pod). On a branch no backup
/// commits (`UNCOMMITTED`), `u.busy` is that work: changes to tracked files (discarded), commits no
/// remote has (the copy has the `.git`), untracked files — which `checkout -f` and `reset --hard`
/// overwrite wherever the new branch tracks the same path (review pr4: the line said untracked
/// files stay).
pub(crate) fn not_pushed_note(u: &Unpushed, copied_to: &str, reset_onto: Option<&str>) -> String {
    use arena_core::backup::{busy_noun, uncommitted_reason, uncommitted_situation};
    let branch = u.branch.as_deref().unwrap_or("the branch it was on");
    let commits = if reset_onto.is_some() && u.branch.as_deref() == reset_onto {
        format!("the --hard reset moves {branch} to origin's, and its unpushed commits stay on the pod under refs/arena-discarded/<time>-{branch}")
    } else if branch == "HEAD" && reset_onto.is_some() {
        "the commits of its detached HEAD stay on the pod under refs/arena-discarded/<time>".to_string()
    } else if branch == "HEAD" {
        "the commits of its detached HEAD stay on the pod only in its reflog (`git reflog`)".to_string()
    } else {
        "its commits stay on the pod in that branch (switching deletes no branch)".to_string()
    };
    if let Some(what) = &u.busy {
        // On main (…) nothing is ever committed: the work is uncommitted, not "not committed now".
        let protected = u.branch.as_deref().is_some_and(is_protected_branch);
        let on = if branch == "HEAD" { "a detached HEAD" } else { branch };
        let situation = uncommitted_situation(what, on);
        let why = if protected { format!("and {}", uncommitted_reason(what, on)) } else { "so nothing was committed".to_string() };
        let ended = match (reset_onto, what.as_str()) {
            (None, _) => String::new(),
            (Some(_), "changes") => "the --hard switch discards them; ".to_string(),
            (Some(_), "commits") => "that copy has its .git, those commits with it; ".to_string(),
            (Some(onto), "untracked") => {
                format!("the --hard switch overwrites any of them origin/{onto} tracks; ")
            }
            (Some(_), _) => format!(
                "the --hard switch ends {} and discards its uncommitted changes to tracked files; ",
                busy_noun(what)
            ),
        };
        let first = if what == "commits" { "NOT PUSHED FIRST" } else { "NOT COMMITTED FIRST" };
        return format!(" — ⚠ {first}: {situation}, {why}; its files were copied to {copied_to} first, and {ended}{commits}");
    }
    format!(" — ⚠ NOT PUSHED FIRST: branch {branch} ({}); its files were copied to {copied_to} first, and {commits}", u.why)
}

/// What a switch did with the real keys of files the backup committed redacted
/// ([`arena_core::backup::checkout_command`]): set aside (the new branch has its own version of
/// the file) or put back. Empty when there were none.
pub(crate) fn unredacted_note(stdout: &str) -> String {
    let lines = |word: &str| -> Vec<String> {
        stdout.lines().filter_map(|l| l.trim_end_matches('\r').strip_prefix(word).map(String::from)).collect()
    };
    let (kept, back) = (lines("UNREDACTED_KEPT "), lines("UNREDACTED_RESTORED "));
    let mut note = String::new();
    if !kept.is_empty() {
        note.push_str(&format!(
            " — the real key(s) in {} file(s) git has redacted are set aside in .git/arena-unredacted/ (a switch back puts \
             them back): {}",
            kept.len(),
            kept.join(", ")
        ));
    }
    if !back.is_empty() {
        note.push_str(&format!(" — real key(s) back in {} file(s) git has redacted: {}", back.len(), back.join(", ")));
    }
    note
}

/// What `init-branches` left unpushed on the new branch ([`arena_core::backup::parse_init_unpushed`]):
/// commits origin doesn't have, which it never pushes unscanned (review claude #2, FIX9B) — the
/// next backup scans them for keys and pushes them. The branch went to origin at the newest
/// commit origin has, or not at all when origin has none of it. Empty when it pushed the whole
/// branch.
pub(crate) fn unpushed_note(stdout: &str) -> String {
    match arena_core::backup::parse_init_unpushed(stdout) {
        None => String::new(),
        Some((n, Some(at))) => format!(
            " — {n} commit(s) origin doesn't have stay on it on the pod, unpushed (the branch went to origin at {}, the newest \
             commit origin has): the next backup scans them for keys and pushes them",
            at.get(..10).unwrap_or(&at)
        ),
        Some((n, None)) => format!(
            " — not pushed: origin has none of its {n} commit(s); the next backup scans them for keys and pushes the branch"
        ),
    }
}

/// Where a `--hard` switch saved, on the pod, what it then discarded (`DISCARDED_SAVED <path>`:
/// its uncommitted changes, an operation's state, untracked files the reset replaced —
/// [`arena_core::backup::checkout_command`]), for the pod's line: a folder no later save or copy
/// overwrites, with a README on getting it back — and the refs that keep the commits the reset left
/// on no branch from git's gc (`DISCARDED_PINNED <ref>`: review ai-review codex #3), named. Empty
/// when it saved nothing (nothing to discard).
pub(crate) fn discarded_note(stdout: &str) -> String {
    let said = |word: &str| -> Vec<&str> { stdout.lines().filter_map(|l| l.trim_end_matches('\r').strip_prefix(word)).collect() };
    let Some(path) = said("DISCARDED_SAVED ").first().copied() else {
        return String::new();
    };
    let pinned = said("DISCARDED_PINNED ");
    let refs = if pinned.is_empty() { String::new() } else { format!("; its commits kept by {}", pinned.join(", ")) };
    format!(" — what it discarded was saved to {}/ on the pod first (its README says how to get it back{refs})", path.trim_end_matches('/'))
}

/// The per-pod note for the nested repos a switch put back (`RESTORED <path>` lines;
/// `""` when none). What happened to them: the backup adopted these clones (the course repo
/// tracks their files on the autocommit branches only), so the checkout to `branch`, which
/// doesn't track them, deleted their files; the switch then put them back from the old HEAD,
/// staged ([`arena_core::backup::checkout_command`]). On main/master no backup commits;
/// `init-branches` branches off with the index as it is, so the first backup on the day branch
/// commits them. On any other branch the next backup does.
pub(crate) fn restored_note(restored: &[&str], branch: &str) -> String {
    if restored.is_empty() {
        return String::new();
    }
    let n = restored.len();
    let when = if is_protected_branch(branch) { "the first backup on the next day branch" } else { "the next backup" };
    format!(
        " — restored {n} nested repo{} that {branch} doesn't track ({}): on disk and staged; {when} commits them",
        if n == 1 { "" } else { "s" },
        restored.join(", ")
    )
}

/// `set-branch`'s closing reminder when it left this cohort's pods (`landed`: the ones that
/// switched) on a protected branch: from now until `init-branches`, no backup commits their
/// work ([`on_protected_line`]). Said at the moment of the reset, when the operator can act.
/// `None` for any other branch or when no cohort pod landed.
pub(crate) fn set_branch_reminder(naming: &Naming, branch: &str, landed: &[String], fleet_wide: bool) -> Option<String> {
    if !is_protected_branch(branch) {
        return None;
    }
    let on: Vec<(String, String)> =
        landed.iter().filter(|n| naming.is_cohort(n)).map(|n| (n.clone(), branch.to_string())).collect();
    on_protected_line(&on, fleet_wide)
}

/// `pods init-branches`: create each pod's `autocommit-…-wNdM-…` branch and push it
/// upstream, without committing — so a new day's branch exists before backups run. A pod still
/// on another branch (yesterday's, when the daily reset didn't run) has that branch pushed
/// first, as `set-branch` does ([`switch_guarded`]): the new branch starts from it, so a key
/// GitHub refuses in it would refuse the new one too — and switching to a day branch that
/// already exists would leave its commits behind.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_init_branches(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    week: Option<u32>,
    day: Option<u32>,
    // The pods to init, resolved by the caller.
    sel: &Selected,
    dry_run: bool,
    yes: bool,
    // The rsync a pod whose branch can't be pushed is copied with: [`RSYNC`], a stub in tests.
    rsync: &str,
) -> Result<()> {
    let (week, day) = resolve_week_day(cfg, week, day)?;
    let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
    println!("Iteration: w{week}d{day}\n");

    let targets = sel.named_targets(cfg)?;
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint)");
        return Ok(());
    }

    if dry_run {
        println!("{}\n", push_first_note(cfg, false));
        println!("Dry-run — would init the w{week}d{day} branch on {} pod(s):\n", targets.len());
        for (name, t) in &targets {
            let cmd = arena_core::backup::init_branch_command(&bcfg, name);
            println!("# {name}  ->  branch {}", bcfg.branch_for(name));
            println!("{}\n", t.display_command(&cmd));
        }
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }
    // Before the question, so it's read before the answer (and in the run's output with --yes).
    if let Some(line) = switch_unlocked_line(cfg) {
        say(&line);
    }
    if !confirm(yes, &format!(
        "Will create + push the w{week}d{day} autocommit branch on {} pod(s): {}",
        targets.len(),
        targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
    ))? {
        println!("aborted.");
        return Ok(());
    }

    let total = targets.len();
    let doing = format!("to their w{week}d{day} branch");
    let cmd_for = |name: &str| arena_core::backup::init_branch_command(&bcfg, name);
    // A day branch that already exists gets back the real keys set aside on it (the morning's
    // `set-branch main` run twice): said as set-branch says it.
    let run = switch_guarded(&remote, cfg, sel, targets, rsync, None, cmd_for, BRANCH_TIMEOUT, &doing, |name, out, note| {
        format!("✓ {name} -> {}{}{}{note}", bcfg.branch_for(name), unpushed_note(&out.stdout), unredacted_note(&out.stdout))
    })
    .await;
    println!("\nDone: {} initialized, {} failed, {} not switched (of {total}).", run.ok, run.failed.len(), run.not_switched.len());
    run.result("init branch")
}
