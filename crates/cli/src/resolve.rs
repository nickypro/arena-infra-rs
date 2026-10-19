//! `pods backup`'s step for the pods whose backup said BUSY (an operation in progress, nothing
//! committed): a read-only look at each, under its lock, before their copy; then,
//! `BUSY_AUTO_RESOLVE_HOURS` on and the pod quiet that long with this run's copy holding its
//! state, the operation finished on the pod and the pod backed up again — in the same run, under
//! its lock. The decisions are core's (`arena_core::resolve`); this is the I/O, beside the backup
//! flow in main.rs rather than in it: `pods backup` calls [`Busy::look`] after its git step and
//! [`Busy::settle`] after its copy.

use std::sync::Arc;
use std::time::Duration;

use arena_core::backup::{self, parse_backup_report};
use arena_core::remote::{describe_error, Remote};
use arena_core::resolve::{self as core_resolve, CopiedNow, Look};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::{each_pod, exec_each_pod, failure_why, take_pod_locks, Pulled, Selected, Unpushed, BACKUP_TIMEOUT, POD_LOCK_WAIT};

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
    /// What the look before the copy saw (`None`: it failed, or another backup held the lock).
    look: Option<Look>,
}

/// The BUSY pods of a `pods backup` run, looked at.
pub(crate) struct Busy {
    /// `BUSY_AUTO_RESOLVE_HOURS` (`None`: off, or invalid — said).
    hours: Option<u64>,
    pods: Vec<BusyPod>,
    /// The backup dir (its per-pod locks).
    dir: String,
}

/// What [`Busy::settle`] did.
#[derive(Default)]
pub(crate) struct Settled {
    /// The lines to say after the copies' (`print`).
    lines: Vec<String>,
    /// The pods finished and then backed up: no longer failures.
    done: Vec<String>,
}

impl Busy {
    /// The run's BUSY pods (`unpushed` with `busy`): each looked at, read-only, under its lock
    /// (a short wait: one another backup holds is copied as before and not finished). Nothing
    /// BUSY: nothing done.
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
        let mut out = Busy { hours, pods: Vec::new(), dir: dir.to_string() };
        if busy.is_empty() {
            return out;
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
        out.pods = busy.into_iter().map(|(name, target)| BusyPod { look: looks.remove(&name), name, target }).collect();
        out
    }

    /// After the copy (`pulled`, every BUSY pod's): finish each BUSY pod [`core_resolve::hold`]
    /// lets through — this run's copy done and holding its checkout — under its lock, the resolve
    /// and then its backup again (`message`: the backup's commit message, `pods backup -m`), and
    /// say what became of each.
    pub(crate) async fn settle(self, remote: &Arc<dyn Remote>, cfg: &Config, pulled: &anyhow::Result<Pulled>, message: Option<&str>) -> Settled {
        let mut out = Settled::default();
        let Busy { hours, pods, dir } = self;
        let remote_path = cfg.get("BACKUP_REMOTE_PATH").filter(|s| !s.is_empty()).unwrap_or_default().to_string();
        let mut candidates = Vec::new();
        for pod in &pods {
            // This run's copy: where it is (the snapshot), as the commit names it (`<wNdM>/<pod>`).
            let copy = match (pulled, &pod.look) {
                (Ok(p), Some(look)) if !p.failed.iter().any(|(q, _)| *q == pod.name) && core_resolve::copy_holds(&remote_path, look, &pod.target.user) => {
                    let label = p.snapshot_root.rsplit('/').next().unwrap_or_default();
                    Some((format!("{}/{}", p.snapshot_root, pod.name), format!("{label}/{}", pod.name)))
                }
                _ => None,
            };
            let copied = if copy.is_some() { CopiedNow::Yes } else { CopiedNow::Failed };
            if let (Ok(()), Some(copy)) = (core_resolve::hold(hours, pod.look.as_ref(), copied), copy) {
                candidates.push((pod.name.clone(), pod.target.clone(), pod.look.clone(), copy));
            }
        }
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
            .filter_map(|(name, target, look, (to, label))| {
                let look = look?;
                let resolve = core_resolve::resolve_command(&repo, hours, &look.op_key(), &look.print, &label);
                // The backup's own message, as `pods backup` gave it.
                let msg = message.map_or_else(|| format!("arena backup {name}"), String::from);
                let again = backup::backup_command(&repo, key.as_deref(), &msg, origin.as_deref());
                let remote = remote.clone();
                let job = async move {
                    let resolved = remote.exec(&target, &resolve, Some(RESOLVE_TIMEOUT)).await.map_err(|e| describe_error(&e));
                    let finished = matches!(&resolved, Ok(o) if o.success && core_resolve::parse_resolve(&o.stdout).resolved.is_some());
                    let backed = if finished { Some(remote.exec(&target, &again, Some(BACKUP_TIMEOUT)).await.map_err(|e| describe_error(&e))) } else { None };
                    (resolved, backed, to)
                };
                Some((name, job))
            })
            .collect();
        let mut finished = Vec::new();
        each_pod(jobs, |_, _, name, result| {
            let line = match result {
                Ok((resolved, backed, to)) => finished_line(name, hours, &repo, resolved, backed, &to, &mut finished),
                Err(why) => format!("✗ {name}: finishing its operation in progress failed: {why}"),
            };
            out.lines.push(line);
        })
        .await;
        drop(locks);
        out.done = finished;
        out
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
    to: &str,
    done: &mut Vec<String>,
) -> String {
    let report = resolved.as_ref().map(|o| core_resolve::parse_resolve(&o.stdout)).unwrap_or_default();
    if let Some(why) = &report.refused {
        return format!("⊘ {name}: not finished automatically — {why}");
    }
    if report.resolved.is_none() {
        let saved = report.saved.as_ref().map_or_else(String::new, |s| format!("; the conflicted files as they were are in {s} on the pod"));
        let step = report.failed.as_ref().map_or_else(|| failure_why(&resolved), |s| format!("at {s}"));
        return format!("✗ {name}: finishing its operation in progress failed ({step}){saved} — the copy {to} has it as it was");
    }
    let note = core_resolve::resolved_note(&report, hours, to);
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

    /// The git step's result once finished pods are backed up: the failures `unpushed` counted
    /// less those; none left — `Ok`. Nothing finished: `git` as it was.
    pub(crate) fn git(&self, git: anyhow::Result<()>, unpushed: &[Unpushed]) -> anyhow::Result<()> {
        if self.done.is_empty() || git.is_ok() {
            return git;
        }
        match unpushed.iter().filter(|u| !self.done.contains(&u.pod)).count() {
            0 => Ok(()),
            n => Err(anyhow::anyhow!("{n} pod(s) failed to back up")),
        }
    }
}

#[cfg(test)]
mod tests;
