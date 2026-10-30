//! The backups' run record, `backups.json` in the fleet's state dir
//! ([`crate::snapshot::state_file_path_for`]): what each git backup and home copy did, per pod —
//! the one fact no backup left anywhere a status could read it (the cron log has no times; the
//! per-pod lock and `.arena-copied/<pod>` say nothing of a failure). Written by `pods backup`
//! (its git step, its pull and its copy of a pod git didn't back up), `pods pull` and the
//! dashboard's backup action, best effort: a record that can't be written is a warning, never a
//! failed backup. Read by `arena backups` and the dashboard's backups screen
//! ([`super::read`]).
//!
//! Small by construction: per pod its last git backup and home copy, when each last went
//! through, and a short history in which a run of the same result is one entry (`no changes ×12,
//! 09:00–12:00`); each job's last finished run and the runs under way (by pid, so one killed
//! reads as interrupted). Rewritten atomically under the state dir's lock, as the disk skips are
//! ([`crate::disk::record_skips`]).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Job;

/// The record's file name in the fleet's state dir.
pub const RECORD_FILE: &str = "backups.json";
/// Its format.
pub const RECORD_VERSION: u32 = 1;
/// History entries kept per pod (each a run of one result).
pub const HISTORY_LEN: usize = 20;
/// A pod with nothing recorded for this long is dropped at the next write (renamed, gone).
pub const FORGET_POD_AFTER_SECS: u64 = 30 * 86_400;

/// What one backup of one pod came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Git: committed and pushed.
    Pushed,
    /// Git: nothing new to commit, the branch already on GitHub.
    NoChanges,
    /// Git: on `main`/`master`/a detached HEAD, which no backup commits — not saved to git until
    /// `pods init-branches` (not a failure of the run: the morning reset leaves the fleet there).
    NotSaved,
    /// Home copy: every rsync of the pod went through.
    Copied,
    /// Either: it failed (`detail` says why).
    Failed,
}

impl Outcome {
    /// It saved the pod (or found nothing new to save).
    pub fn ok(self) -> bool {
        matches!(self, Outcome::Pushed | Outcome::NoChanges | Outcome::Copied)
    }

    /// In a sentence: `pushed`, `no changes`, `not saved (protected branch)`, `copied`, `failed`.
    pub fn words(self) -> &'static str {
        match self {
            Outcome::Pushed => "pushed",
            Outcome::NoChanges => "no changes",
            Outcome::NotSaved => "not saved to git",
            Outcome::Copied => "copied",
            Outcome::Failed => "failed",
        }
    }
}

/// One pod's backup: when, which job, what came of it. In the history, a run of the same job and
/// result (a failure: and reason) is one entry — `since` its first, `times` how many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub job: Job,
    pub outcome: Outcome,
    /// What the line said: the branch it pushed (`→ autocommit-…`), the copy's size and label
    /// (`w3d5: 1.8 GB in 6754 files, 12 MB new`), or why it failed.
    #[serde(default)]
    pub detail: String,
    /// A copy's size on disk (the snapshot's files), when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Folded: the first of the run (`None`: a single one). A run is the job's own: a git
    /// backup's folds over the copies between them ([`PodRecord::add`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Folded: how many (`1` when single).
    #[serde(default = "one")]
    pub times: u32,
}

fn one() -> u32 {
    1
}

impl Event {
    pub fn new(at: u64, job: Job, outcome: Outcome, detail: impl Into<String>) -> Event {
        Event { at, job, outcome, detail: detail.into(), bytes: None, since: None, times: 1 }
    }

    pub fn with_bytes(mut self, bytes: Option<u64>) -> Event {
        self.bytes = bytes;
        self
    }

    /// Whether `next` continues this one's run in the history: the same job and result, and for a
    /// failure the same reason (a new reason is news).
    fn folds(&self, next: &Event) -> bool {
        self.job == next.job && self.outcome == next.outcome && (next.outcome != Outcome::Failed || self.detail == next.detail)
    }
}

/// One pod's backups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodRecord {
    /// Its last git backup, and when one last went through (pushed or nothing new).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ok: Option<u64>,
    /// Its last home copy, and when one last went through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_ok: Option<u64>,
    /// Newest first, runs folded ([`Event::folds`]), at most [`HISTORY_LEN`].
    #[serde(default)]
    pub history: Vec<Event>,
}

impl PodRecord {
    /// When anything was last recorded for it.
    pub fn last_at(&self) -> u64 {
        self.history.first().map_or(0, |e| e.at).max(self.git.as_ref().map_or(0, |e| e.at)).max(self.copy.as_ref().map_or(0, |e| e.at))
    }

    /// Add `e` (pure): the job's last, its last success, and the history. An event older than
    /// the job's last (a run that started earlier and ended later than another, both recording)
    /// never replaces it: it only moves the last success forward, and takes its place in the
    /// history by time.
    pub fn add(&mut self, e: Event) {
        let (last, ok) = match e.job {
            Job::Git => (&mut self.git, &mut self.git_ok),
            _ => (&mut self.copy, &mut self.copy_ok),
        };
        if e.outcome.ok() && ok.is_none_or(|t| e.at > t) {
            *ok = Some(e.at);
        }
        if last.as_ref().is_some_and(|l| e.at < l.at) {
            return self.add_history(e);
        }
        *last = Some(e.clone());
        // The job's own newest entry: the git backup's and the copy's runs interleave, each
        // folded on its own (`no changes ×12` beside `copied ×12`).
        let e = match self.history.iter().position(|h| h.job == e.job) {
            Some(i) if self.history[i].folds(&e) && self.history[i].at <= e.at => {
                let h = self.history.remove(i);
                Event { since: Some(h.since.unwrap_or(h.at)), times: h.times.saturating_add(1), ..e }
            }
            _ => e,
        };
        self.add_history(e);
    }

    /// Put `e` in the history alone (pure), by time, newest first — what a copy that isn't the
    /// configured backup (another `--dir`, `--no-big`, a label of its own) is recorded as: the pod's
    /// history says it happened, and its backup's freshness is left alone.
    pub fn add_history(&mut self, e: Event) {
        let at = self.history.iter().position(|h| h.at <= e.at).unwrap_or(self.history.len());
        self.history.insert(at, e);
        self.history.truncate(HISTORY_LEN);
    }
}

/// A finished run of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub started: u64,
    pub finished: u64,
    pub pid: u32,
    /// Every pod it took up went through (a pod on `main` doesn't fail it, as `pods backup`'s
    /// exit says).
    pub ok: bool,
    /// How many pods it took up.
    pub pods: u32,
    /// The ones that failed.
    #[serde(default)]
    pub failed: Vec<String>,
    /// The run in a few words: `1 pushed, 2 no changes, 41 on main, 0 failed`.
    #[serde(default)]
    pub summary: String,
}

/// A run under way: started, not finished — its pid alive, or killed (then it's said as
/// interrupted, and dropped at the next write).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub job: Job,
    pub pid: u32,
    pub started: u64,
    pub pods: u32,
}

/// `backups.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    /// Each job's last finished run.
    #[serde(default)]
    pub runs: BTreeMap<Job, RunRecord>,
    #[serde(default)]
    pub running: Vec<Running>,
    #[serde(default)]
    pub pods: BTreeMap<String, PodRecord>,
}

impl Default for Record {
    fn default() -> Self {
        Record { version: RECORD_VERSION, runs: BTreeMap::new(), running: Vec::new(), pods: BTreeMap::new() }
    }
}

impl Record {
    /// Read the record: missing = nothing recorded yet (`Ok(None)`); unreadable, not JSON or
    /// another version = `Err(why)` — said as unknown, never as "never".
    pub fn load(path: &Path) -> Result<Option<Record>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("can't read {}: {e}", path.display())),
        };
        match serde_json::from_str::<Record>(&text) {
            Ok(r) if r.version == RECORD_VERSION => Ok(Some(r)),
            Ok(r) => Err(format!("{} is version {} (this arena reads {RECORD_VERSION})", path.display(), r.version)),
            Err(e) => Err(format!("{} isn't readable: {e}", path.display())),
        }
    }

    /// A run of `job` started (pure): noted with its pid, so one killed reads as interrupted.
    pub fn start(&mut self, job: Job, pid: u32, pods: u32, now: u64) {
        self.running.retain(|r| !(r.job == job && r.pid == pid));
        self.running.push(Running { job, pid, started: now, pods });
    }

    /// A run of `job` finished (pure): its events added to each pod, the run recorded, its
    /// "running" entry gone; pods with nothing recorded for [`FORGET_POD_AFTER_SECS`] dropped.
    pub fn finish(&mut self, job: Job, pid: u32, now: u64, events: Vec<(String, Event)>, summary: String) {
        let started = self.running.iter().find(|r| r.job == job && r.pid == pid).map_or(now, |r| r.started);
        self.running.retain(|r| !(r.job == job && r.pid == pid));
        let mut failed: Vec<String> = events.iter().filter(|(_, e)| e.outcome == Outcome::Failed).map(|(p, _)| p.clone()).collect();
        failed.sort();
        failed.dedup();
        let mut pods: Vec<&String> = events.iter().map(|(p, _)| p).collect();
        pods.sort();
        pods.dedup();
        let run = RunRecord { started, finished: now, pid, ok: failed.is_empty(), pods: pods.len() as u32, failed, summary };
        for (pod, e) in events {
            self.pods.entry(pod).or_default().add(e);
        }
        self.runs.insert(job, run);
        self.pods.retain(|_, p| now.saturating_sub(p.last_at()) < FORGET_POD_AFTER_SECS);
    }

    /// Add events outside a run of their own (a pod a run left out: its lock busy) — pure.
    pub fn add_events(&mut self, events: Vec<(String, Event)>) {
        for (pod, e) in events {
            self.pods.entry(pod).or_default().add(e);
        }
    }

    /// Add events to the pods' histories alone ([`PodRecord::add_history`]) — pure.
    pub fn add_history(&mut self, events: Vec<(String, Event)>) {
        for (pod, e) in events {
            self.pods.entry(pod).or_default().add_history(e);
        }
    }

    /// Drop the "running" entries whose process is gone (pure, given `alive`).
    pub fn prune_running(&mut self, alive: impl Fn(u32) -> bool) {
        self.running.retain(|r| alive(r.pid));
    }
}

/// Change the record at `path` with `f`, under the state dir's lock (tried for up to `wait`),
/// saved atomically, owner-only. A record that can't be read is started afresh (said in the
/// `Ok` warning) rather than blocking every later run's record. Best effort for the caller: an
/// `Err` is a warning, never a failed backup.
pub fn update(path: &Path, wait: std::time::Duration, f: impl FnOnce(&mut Record)) -> std::io::Result<Option<String>> {
    // `None`: no lock to be had there (no `flock`): the atomic write alone, as the skips do.
    let lock = crate::disk::lock_state_for(path, wait)?;
    let (mut record, warning) = match Record::load(path) {
        Ok(r) => (r.unwrap_or_default(), None),
        Err(why) => (Record::default(), Some(format!("warning: starting the backups record afresh: {why}"))),
    };
    record.prune_running(crate::disk::arena_alive);
    f(&mut record);
    let mut json = serde_json::to_string_pretty(&record).map_err(std::io::Error::other)?;
    json.push('\n');
    let saved = crate::snapshot::write_atomic(path, json.as_bytes(), 0o600);
    drop(lock);
    saved.map(|()| warning)
}
