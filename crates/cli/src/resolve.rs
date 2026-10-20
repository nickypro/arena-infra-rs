//! `pods backup`'s step for the pods whose backup said BUSY (an operation in progress, nothing
//! committed): a read-only look at each, under its lock; their copy paced by it (part 2: only
//! when nothing since the operation started is recorded — or the recorded copy is gone, or of
//! another backup dir —, the work tree changed, or the last copy is an hour old); then,
//! `BUSY_AUTO_RESOLVE_HOURS` on and the pod quiet that long with a whole copy holding its state,
//! the operation finished on the pod and the pod backed up again — in the same run, under its
//! lock. The decisions are core's (`arena_core::resolve`); this is the I/O, beside
//! the backup flow in main.rs rather than in it: `pods backup` calls [`Busy::look`] after its git
//! step and [`Busy::settle`] after its copy.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arena_core::backup::{self, parse_backup_report};
use arena_core::remote::{describe_error, Remote};
use arena_core::resolve::{self as core_resolve, BusyCopies, BusyCopy, CopiedNow, CopyWhy, Hold, Look};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::{each_pod, exec_each_pod, failure_why, take_pod_locks, BackupFailed, Pulled, Selected, Unpushed, BACKUP_TIMEOUT, POD_LOCK_WAIT};

/// How long a look may take: a `git ls-files` and a `stat` of the checkout.
const LOOK_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a pod's lock is waited for, for its look: one another backup holds (a pull) means
/// no look — the pod is then copied as before, and not finished this run.
const LOOK_LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long a resolve may take (its look, a checkout of each conflicted path, the commit).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(300);

/// One BUSY pod of the run.
struct BusyPod {
    name: String,
    target: SshTarget,
    /// Its backup's failure, in a few words ([`Unpushed::why`]).
    why: String,
    /// What the look before the copy saw (`None`: it failed, or another backup held the lock).
    look: Option<Look>,
    /// Why it's copied this tick (`None`: not — unchanged since its recorded copy, under an hour).
    copy: Option<CopyWhy>,
}

/// The BUSY pods of a `pods backup` run, looked at, each one's copy decided.
pub(crate) struct Busy {
    /// `BUSY_AUTO_RESOLVE_HOURS` (`None`: off, or invalid — said).
    hours: Option<u64>,
    /// The copy record ([`core_resolve::BUSY_COPIES_FILE`]; `None`: no state dir — every BUSY pod
    /// is copied each tick, as before).
    record: Option<PathBuf>,
    copies: BusyCopies,
    pods: Vec<BusyPod>,
    /// The backup dir (its per-pod locks).
    dir: String,
    /// This machine's clock at the look.
    now: u64,
}

/// What [`Busy::settle`] did.
#[derive(Default)]
pub(crate) struct Settled {
    /// The lines to say after the copies' (`print`).
    lines: Vec<String>,
    /// The pods finished and then backed up: no longer failures.
    done: Vec<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `14:02 UTC` (`at` today, by `now`), else `2026-10-18 14:02 UTC`.
fn at_words(at: u64, now: u64) -> String {
    let hm = format!("{:02}:{:02} UTC", at % 86_400 / 3600, at % 3600 / 60);
    if at / 86_400 == now / 86_400 {
        hm
    } else {
        format!("{} {hm}", arena_core::schedule::ymd_string((at / 86_400) as i64))
    }
}

impl Busy {
    /// The run's BUSY pods (`unpushed` with `busy`): each looked at, read-only, under its lock
    /// (a short wait: one another backup holds is copied as before and not finished); the copy
    /// record loaded, each pod's record trusted only while its copy is still there
    /// ([`core_resolve::copy_still_there`]); each one's copy decided
    /// ([`core_resolve::copy_needed`]). Nothing BUSY: nothing done.
    pub(crate) async fn look(remote: &Arc<dyn Remote>, cfg: &Config, dir: &str, sel: &Selected, unpushed: &[Unpushed]) -> Busy {
        let hours = core_resolve::auto_resolve_hours(cfg.get(core_resolve::AUTO_RESOLVE_KEY)).unwrap_or_else(|e| {
            eprintln!("warning: {e} — no operation left in progress is finished this run");
            None
        });
        let busy: Vec<(String, SshTarget)> = sel
            .named_targets(cfg)
            .unwrap_or_default()
            .into_iter()
            .filter(|(n, _)| unpushed.iter().any(|u| u.busy.is_some() && u.pod == *n))
            .collect();
        let record = arena_core::snapshot::state_file_path_for(cfg, core_resolve::BUSY_COPIES_FILE).ok();
        let mut out = Busy { hours, record, copies: BusyCopies::default(), pods: Vec::new(), dir: dir.to_string(), now: now_secs() };
        if busy.is_empty() {
            return out;
        }
        if let Some(path) = &out.record {
            let (copies, warning) = BusyCopies::load(path);
            warning.iter().for_each(|w| eprintln!("{w}"));
            out.copies = copies;
        }
        let names: Vec<String> = busy.iter().map(|(n, _)| n.clone()).collect();
        let locks = take_pod_locks(dir, &names, LOOK_LOCK_WAIT).await;
        let repo = backup::repo_path(cfg);
        let jobs = busy.iter().filter(|(n, _)| !locks.busy.contains(n)).map(|(n, t)| (n.clone(), t.clone(), core_resolve::look_command(&repo))).collect();
        let mut looks: std::collections::HashMap<String, Look> = Default::default();
        exec_each_pod(remote, jobs, LOOK_TIMEOUT, |_, _, name, call| {
            if let Some(look) = call.ok().filter(|o| o.success).and_then(|o| core_resolve::parse_look(&o.stdout)) {
                looks.insert(name.clone(), look);
            }
        })
        .await;
        drop(locks);
        out.now = now_secs();
        out.pods = busy
            .into_iter()
            .map(|(name, target)| {
                let look = looks.remove(&name);
                // An earlier run's copy counts only while it is still there, under this backup dir.
                let last = out.copies.pods.get(&name).filter(|l| core_resolve::copy_still_there(l, &out.dir));
                let copy = core_resolve::copy_needed(look.as_ref(), last, out.now);
                let why = unpushed.iter().find(|u| u.pod == name).map(|u| u.why.clone()).unwrap_or_default();
                BusyPod { name, target, why, look, copy }
            })
            .collect();
        out
    }

    /// The pods `pods backup --no-pull` copies instead of their git backup: `unpushed`'s, less the
    /// BUSY pods whose recorded copy still holds them (unchanged since, under an hour old).
    pub(crate) fn to_copy(&self, unpushed: &[Unpushed]) -> Vec<Unpushed> {
        unpushed.iter().filter(|u| !self.pods.iter().any(|p| p.name == u.pod && p.copy.is_none())).cloned().collect()
    }

    /// After the copy (`pulled`; `all`: the run pulled every pod, BUSY ones included, whatever
    /// [`Busy::to_copy`] said): record each BUSY pod's copy that was whole ([`Pulled::whole`]: every
    /// rsync of both tiers, `.git` pass included) and held its checkout, with where its `.git` is;
    /// then finish each BUSY pod [`core_resolve::hold`] lets through, on a recorded copy that is
    /// still there — under its lock, the resolve and then its backup again (`message`: the
    /// backup's commit message, `pods backup -m`), its record forgotten once finished — and say
    /// what became of each (a revert: that arena won't finish it).
    pub(crate) async fn settle(self, remote: &Arc<dyn Remote>, cfg: &Config, pulled: &anyhow::Result<Pulled>, all: bool, message: Option<&str>) -> Settled {
        let mut out = Settled::default();
        let Busy { hours, record, mut copies, pods, dir, now } = self;
        let remote_path = cfg.get("BACKUP_REMOTE_PATH").filter(|s| !s.is_empty()).unwrap_or_default().to_string();
        let mut changes: Vec<(String, Option<BusyCopy>)> = Vec::new();
        let mut candidates = Vec::new();
        for pod in &pods {
            let tried = all || pod.copy.is_some();
            let copied = match (tried, pulled, &pod.look) {
                (false, ..) => CopiedNow::No,
                (true, Ok(p), Some(look)) if p.whole.contains(&pod.name) => match core_resolve::copy_place(&remote_path, look, &pod.target.user) {
                    Some(place) => {
                        let label = p.snapshot_root.rsplit('/').next().unwrap_or_default();
                        let to = format!("{}/{}", p.snapshot_root, pod.name);
                        let git = std::path::Path::new(&to).join(place).join(".git").display().to_string();
                        let copy = BusyCopy {
                            op: look.op_key(),
                            print: look.print.clone(),
                            newest: look.newest,
                            at: now,
                            to,
                            label: format!("{label}/{}", pod.name),
                            git,
                        };
                        copies.pods.insert(pod.name.clone(), copy.clone());
                        changes.push((pod.name.clone(), Some(copy)));
                        CopiedNow::Yes
                    }
                    None => CopiedNow::Failed,
                },
                (true, ..) => CopiedNow::Failed,
            };
            if !tried {
                // Not copied this tick: said so, with the copy that still holds it.
                if let Some(last) = copies.pods.get(&pod.name) {
                    out.lines.push(format!(
                        "✗ {}: not backed up to git ({}) — copied at {} to {}, unchanged since",
                        pod.name,
                        pod.why,
                        at_words(last.at, now),
                        last.to
                    ));
                }
            }
            let last = copies.pods.get(&pod.name).filter(|l| core_resolve::copy_still_there(l, &dir));
            match core_resolve::hold(hours, pod.look.as_ref(), copied, last) {
                Ok(last) => candidates.push((pod.name.clone(), pod.target.clone(), pod.look.clone(), last.clone())),
                Err(Hold::Never(what)) if what == "revert" => {
                    out.lines.push(format!("⊘ {}: not finished automatically — {}", pod.name, core_resolve::REVERT_WORDS));
                }
                Err(_) => {}
            }
        }
        save(record.clone(), changes, now).await;
        let Some(hours) = hours.filter(|_| !candidates.is_empty()) else { return out };
        // Finish each, under its lock: the resolve looks again and goes ahead only if nothing
        // changed since the look this copy followed; then the backup, as any run's.
        let names: Vec<String> = candidates.iter().map(|(n, ..)| n.clone()).collect();
        let locks = take_pod_locks(&dir, &names, POD_LOCK_WAIT).await;
        for b in &locks.busy {
            out.lines.push(format!("⊘ {b}: not finished this run — another backup of it is running"));
        }
        let repo = backup::repo_path(cfg);
        let key = cfg.get("GIT_SSH_KEY_REMOTE").map(String::from);
        let origin = backup::configured_repo(cfg);
        let jobs = candidates
            .into_iter()
            .filter(|(n, ..)| !locks.busy.contains(n))
            .filter_map(|(name, target, look, last)| {
                let look = look?;
                let resolve = core_resolve::resolve_command(&repo, hours, &look.op_key(), &look.print, &last.label);
                // The backup's own message, as `pods backup` gave it.
                let msg = message.map_or_else(|| format!("arena backup {name}"), String::from);
                let again = backup::backup_command(&repo, key.as_deref(), &msg, origin.as_deref());
                let remote = remote.clone();
                let job = async move {
                    let resolved = remote.exec(&target, &resolve, Some(RESOLVE_TIMEOUT)).await.map_err(|e| describe_error(&e));
                    let finished = matches!(&resolved, Ok(o) if o.success && core_resolve::parse_resolve(&o.stdout).resolved.is_some());
                    let backed = if finished { Some(remote.exec(&target, &again, Some(BACKUP_TIMEOUT)).await.map_err(|e| describe_error(&e))) } else { None };
                    (resolved, backed, last)
                };
                Some((name, job))
            })
            .collect();
        let mut finished = Vec::new();
        each_pod(jobs, |_, _, name, result| {
            let line = match result {
                Ok((resolved, backed, last)) => finished_line(name, hours, &repo, resolved, backed, &last, &mut finished),
                Err(why) => format!("✗ {name}: finishing its operation in progress failed: {why}"),
            };
            out.lines.push(line);
        })
        .await;
        drop(locks);
        let forget: Vec<(String, Option<BusyCopy>)> = finished.iter().map(|n| (n.clone(), None)).collect();
        save(record, forget, now).await;
        out.done = finished;
        out
    }
}

/// Record `changes` in the copy record at `path` (none: nothing written), said on stderr when it
/// can't be — the BUSY pods are then copied again next run. Off the async runtime: the record
/// waits (bounded) for the state dir's lock ([`core_resolve::record_busy_copies`]).
async fn save(path: Option<PathBuf>, changes: Vec<(String, Option<BusyCopy>)>, now: u64) {
    let Some(path) = path.filter(|_| !changes.is_empty()) else { return };
    let shown = path.display().to_string();
    let wrote = tokio::task::spawn_blocking(move || core_resolve::record_busy_copies(&path, &changes, now, core_resolve::RECORD_LOCK_WAIT)).await;
    match wrote {
        Ok(Ok(warning)) => warning.iter().for_each(|w| eprintln!("{w}")),
        Ok(Err(e)) => eprintln!("warning: couldn't record the BUSY pods' copies in {shown} ({e}) — they're copied again next run"),
        Err(e) => eprintln!("warning: couldn't record the BUSY pods' copies in {shown} ({e}) — they're copied again next run"),
    }
}

/// A finished pod's line, from its resolve and its backup after — `done` gains the pod when
/// both went through.
fn finished_line(
    name: &str,
    hours: u64,
    repo: &str,
    resolved: crate::PodCall,
    backed: Option<crate::PodCall>,
    last: &BusyCopy,
    done: &mut Vec<String>,
) -> String {
    let report = resolved.as_ref().map(|o| core_resolve::parse_resolve(&o.stdout)).unwrap_or_default();
    if let Some(why) = &report.refused {
        return format!("⊘ {name}: not finished automatically — {why}");
    }
    if report.resolved.is_none() {
        let saved = report.saved.as_ref().map_or_else(String::new, |s| format!("; the conflicted files as they were are in {s} on the pod"));
        let step = report.failed.as_ref().map_or_else(|| failure_why(&resolved), |s| format!("at {s}"));
        return format!("✗ {name}: finishing its operation in progress failed ({step}){saved} — the copy {} has it as it was", last.to);
    }
    let note = core_resolve::resolved_note(&report, hours, &last.to);
    let Some(backed) = backed else { return format!("✗ {name}: {note}, but its backup wasn't run") };
    let b = backed.as_ref().map(|o| parse_backup_report(&o.stdout)).unwrap_or_default();
    match (&backed, &b.outcome) {
        (Ok(o), Some((backup::BACKUP_PUSHED | backup::BACKUP_NO_CHANGES, branch))) if o.success => {
            done.push(name.to_string());
            let notes = b.notes();
            let also = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
            format!("✓ {name} -> {branch} — {note}{also}")
        }
        (Ok(o), _) => {
            let why = b.failure(repo, name, o.code).unwrap_or_else(|| failure_why(&backed));
            format!("✗ {name}: {note}, but its backup then failed: {why}")
        }
        (Err(why), _) => format!("✗ {name}: {note}, but its backup then failed: {why}"),
    }
}

impl Settled {
    /// Say what [`Busy::settle`] did, after the copies' lines.
    pub(crate) fn print(&self) {
        if !self.lines.is_empty() {
            println!();
            self.lines.iter().for_each(|l| crate::say(l));
        }
    }

    /// `unpushed` less the pods finished and backed up (their fallback line would only repeat the
    /// failure their own line ended).
    pub(crate) fn unsettled(&self, unpushed: &[Unpushed]) -> Vec<Unpushed> {
        unpushed.iter().filter(|u| !self.done.contains(&u.pod)).cloned().collect()
    }

    /// The git step's result once finished pods are backed up: its failure ([`BackupFailed`],
    /// which names every pod that failed — before its push or at it, any way) less the pods
    /// finished and backed up since; none left — `Ok`. Any other failure, or nothing finished:
    /// `git` as it was. The count is never rebuilt from anything but those names: a pod that
    /// failed some other way stays a failure.
    pub(crate) fn git(&self, git: anyhow::Result<()>) -> anyhow::Result<()> {
        let Err(e) = git else { return Ok(()) };
        let Some(failed) = e.downcast_ref::<BackupFailed>().filter(|_| !self.done.is_empty()) else { return Err(e) };
        let left: Vec<String> = failed.pods.iter().filter(|p| !self.done.contains(p)).cloned().collect();
        if left.is_empty() {
            Ok(())
        } else {
            Err(BackupFailed { pods: left }.into())
        }
    }
}

#[cfg(test)]
mod tests;
