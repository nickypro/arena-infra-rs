//! What gets backed up, where and when — and how it's doing: one status model for every backup
//! arena makes, which `arena backups` prints and the dashboard's backups screen (`b`) draws.
//!
//! The backups each leave their facts in a different place: root's crontab says which run and
//! how often; the git backup and the home copy leave a run record ([`record`], `backups.json` in
//! the state dir) — before it, only a cron log without times; a whole copy marks
//! `<LOCAL_BACKUP_DIR>/.arena-copied/<pod>`; the archive keeps its own sync state
//! (`archive/sync-*.json`); a backup under way holds a lock (the cron line's `flock`, each pod's
//! `.arena-locks/<pod>.lock`, the archive's tier locks — read from `/proc/locks`, never taken:
//! [`locks`]); copies skipped for room are in `disk-skips.json`; the filesystems are judged by
//! the disk guard ([`crate::disk::level`]). [`read`] gathers those ([`Inputs`]), [`judge`] — pure,
//! table-tested — turns them into a [`Status`]: per job whether it's set up and how often, its
//! last run and result, when it's next due, whether it's running now; per pod its last git save,
//! home copy and archive copy, failing ones first, then the oldest; and where each copy lives with
//! its free space. Anything it can't read is **unknown, with why** — never "ok" and never
//! "never".

pub mod locks;
pub mod record;
pub mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive::state::{ArchiveHost, BigState, SnapState};
use crate::archive::Tier;
use crate::config::Config;
use crate::cron::{Kind, Schedule};
use crate::disk::{self, Fs, Level, SkipEntry};
use record::{Outcome, Record};

/// A backup job with a status of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Job {
    /// `pods backup`'s git step: commit + push each pod's branch to the cohort repo.
    Git,
    /// The home copy: `pods pull`, and `pods backup`'s own pull (or its copy of a pod git didn't
    /// back up) — rsync into `LOCAL_BACKUP_DIR`.
    Copy,
    /// `archive sync --tier snap`: the dated snapshots onto the archive VM.
    ArchiveSnap,
    /// `archive sync --tier big`: the all-files tier onto the archive VM.
    ArchiveBig,
}

impl Job {
    pub const ALL: [Job; 4] = [Job::Git, Job::Copy, Job::ArchiveSnap, Job::ArchiveBig];

    /// Its name on the screen and in `arena backups`.
    pub fn name(self) -> &'static str {
        match self {
            Job::Git => "git backup",
            Job::Copy => "home copy",
            Job::ArchiveSnap => "archive: snapshots",
            Job::ArchiveBig => "archive: all files",
        }
    }
}

/// How something is doing — worst first, so sorting puts failures at the top. `Off`: not set up
/// (said with what sets it up), which no pod is judged by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Health {
    Fail,
    Warn,
    Unknown,
    Ok,
    Off,
}

impl Health {
    /// The one mark each gets in text: `✗`, `⚠`, `?`, `✓`, `-`.
    pub fn mark(self) -> &'static str {
        match self {
            Health::Fail => "✗",
            Health::Warn => "⚠",
            Health::Unknown => "?",
            Health::Ok => "✓",
            Health::Off => "-",
        }
    }
}

// ---------------------------------------------------------------------------------------
// The inventory: every backup kind, what starts it, where its copy lands, what it records
// ---------------------------------------------------------------------------------------

/// One kind of backup (or a path that moves backed-up work), for the docs and `arena backups
/// --inventory`: `(kind, what starts it, where the copy lands, what record it leaves)`. The docs'
/// table is this one, rendered ([`render::inventory_table`]; a test keeps them equal).
pub const INVENTORY: &[(&str, &str, &str, &str)] = &[
    (
        "git backup (`pods backup`)",
        "the cron's backup line, every 15 min (`cron install --backup`); `pods backup`; the dashboard's `a` → backup",
        "the cohort repo on GitHub (`ARENA_REPO_OWNER/NAME`), each pod's current branch (`autocommit-<prefix>-wNdM-<pod>`; never main/master)",
        "`backups.json` (state dir); the cron log; the cron's `backup.lock`; the pod's `.arena-locks/<pod>.lock`",
    ),
    (
        "home copy (`pods pull`)",
        "each cron backup without `--no-pull` (`cron install --backup --pull`); `pods pull`; `pods backup` by hand; a pod git couldn't back up (even under `--no-pull`)",
        "`LOCAL_BACKUP_DIR/<wNdM>/<pod>/` (files under `BACKUP_MAX_SIZE`, hard-linked day to day) and `<all-files tier>/<pod>/` (`LOCAL_BACKUP_BIG_DIR`, else `LOCAL_BACKUP_DIR/big`)",
        "`backups.json`; `LOCAL_BACKUP_DIR/.arena-copied/<pod>` (last whole copy); `disk-skips.json` (skipped for room); the pod's lock",
    ),
    (
        "archive (`archive sync`)",
        "the cron's two archive lines: snapshots at :07/:22/:37/:52, all files at :40 (`cron install --archive`)",
        "the archive VM's `snap` and `big` volumes (`<prefix>-archive`), read-only for participants over sftp",
        "`archive/sync-snap.json`, `sync-big.json` and their locks (state dir); `archive.log`",
    ),
    (
        "fleet baseline (`pods baseline`)",
        "by hand, once a week on a fresh pod",
        "`LOCAL_BACKUP_DIR/baseline/w<N>/fleet/` (what every snapshot hard-links to)",
        "the directory itself",
    ),
    (
        "the reaper's last copy (`pods reap`)",
        "the reap cron line, before it terminates an idle pod (`cron install --reap apply`)",
        "`LOCAL_BACKUP_DIR/reaped/<pod>/…` and the all-files tier's `reaped/`",
        "`reap.json` (state dir); `reap.log`",
    ),
    (
        "BUSY copy",
        "`pods backup`, for a pod with a merge or rebase in progress (not committed)",
        "the home copy's place, as above",
        "`busy-copies.json` (state dir)",
    ),
    (
        "`set-branch --hard`'s save",
        "a hard branch switch, before it discards anything",
        "on the pod: `.git/arena-discarded/<time>/`, and a copy into `LOCAL_BACKUP_DIR` first",
        "the pod's line; `refs/arena-discarded/…` on the pod",
    ),
    (
        "replace/migrate/copy-home staging",
        "`pods replace` / `pods migrate` / `pods copy-home` copying a home via this box (only when the direct machine-to-machine copy can't run)",
        "`$TMPDIR/arena-replace-<pod id>` (removed once the copy lands)",
        "listed by `config check` and `teardown --check`",
    ),
    (
        "`pods restore`",
        "by hand: a backup back onto a pod",
        "the pod; the files it replaced in `~/.arena-restore/<time>/` (or under `/workspace`)",
        "the pod",
    ),
];

/// What no backup takes, for the docs and the screen's last line.
pub const NOT_COVERED: &[&str] = &[
    "dot-directories other than `.claude/` and `.git` (`.cache`, `.config`, …), venvs, `site-packages`, `node_modules`, Hugging Face caches",
    "files over 95 MB stay out of git (the home copy has them); a home copy's snapshot keeps files under `BACKUP_MAX_SIZE` (the all-files tier keeps the rest)",
    "pods that are stopped or unreachable when the cron runs, and staff (`@`) boxes unless named",
    "pods on main/master: no git backup until `arena pods init-branches` (the home copy still runs)",
    "this box's own state (`ARENA_STATE_DIR`: the archive's keys, the records) — back it up yourself",
];

// ---------------------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------------------

/// A pod the status is asked about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodIn {
    pub name: String,
    /// One of this cohort's (a staff box isn't backed up by the cron: never "late").
    pub cohort: bool,
    /// Running (`Some(false)`: stopped — its backups pause; `None`: not known).
    pub up: Option<bool>,
    /// Its repo's last commit, as the dashboard read it from the pod (the SAVED column), for a
    /// pod no backup has recorded yet.
    pub last_commit: Option<u64>,
}

/// A cron line's log, as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFacts {
    pub path: String,
    pub bytes: u64,
    pub modified: Option<u64>,
}

/// One place a copy lives, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreIn {
    pub what: String,
    pub path: String,
    pub fs: Result<Fs, String>,
}

/// The archive, as found in the state dir.
// One per status read: boxing its states would only cost every match a deref.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ArchiveIn {
    /// No `archive.json`: `archive up` never ran for this prefix (as this user).
    None,
    /// The state dir couldn't be read.
    Unreadable(String),
    Up { host: Box<ArchiveHost>, snap: Result<SnapState, String>, big: Result<BigState, String> },
}

/// Which locks are held now ([`locks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The backup line's `flock` (`None`: no backup line).
    pub cron: Option<Result<bool, String>>,
    /// The pods whose backup lock is held.
    pub pods: Result<BTreeSet<String>, String>,
    /// Each archive tier's sync lock.
    pub archive: BTreeMap<Job, Result<bool, String>>,
    /// The record's runs under way: their pid still alive.
    pub alive: BTreeSet<u32>,
}

/// Everything [`judge`] reads — what [`read`] found.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: u64,
    /// The box's UTC offset (cron's schedules are in its zone).
    pub offset: i64,
    pub prefix: String,
    /// The config this run reads (to compare with the cron line's `--config`).
    pub config_path: Option<String>,
    /// The backup line's `--config`, and whether it's the same file as `config_path`.
    pub cron_config: Option<(String, bool)>,
    /// `crontab -l` (`Ok("")`: none).
    pub crontab: Result<String, String>,
    pub record: Result<Option<Record>, String>,
    pub record_path: String,
    pub pods: Vec<PodIn>,
    /// `LOCAL_BACKUP_DIR`.
    pub backup_dir: String,
    /// Each `.arena-copied/<pod>`'s mtime: its last whole copy.
    pub copied: Result<BTreeMap<String, u64>, String>,
    /// Each pod's newest `wNdM` snapshot there.
    pub labels: Result<BTreeMap<String, String>, String>,
    pub archive: ArchiveIn,
    pub held: Held,
    /// Each installed backup line's log.
    pub logs: BTreeMap<Kind, Result<LogFacts, String>>,
    pub storage: Vec<StoreIn>,
    /// `CONTROL_MIN_FREE_GB` in bytes.
    pub floor: Result<u64, String>,
    /// The copies skipped for room in the last day.
    pub skips: Result<Vec<SkipEntry>, String>,
    /// `owner/name` of the cohort repo (`None`: not configured).
    pub repo: Option<String>,
}

/// Whether a job is set up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Setup {
    /// A crontab line runs it: its schedule as written, and how often in words.
    Cron { schedule: String, every: String },
    /// Not set up: why, and the command that sets it up.
    Missing { why: String, fix: String },
    /// Can't tell (the crontab unreadable).
    Unknown { why: String },
}

/// A job's last run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastRun {
    pub started: u64,
    pub finished: u64,
    pub ok: bool,
    /// In a few words.
    pub summary: String,
}

/// One job's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobStatus {
    pub job: Job,
    pub name: &'static str,
    pub setup: Setup,
    pub last: Option<LastRun>,
    /// Why there's no last run to show (none recorded, the record unreadable, …).
    pub last_unknown: Option<String>,
    /// When it's next due (UNIX s).
    pub next: Option<u64>,
    /// Running now: since when (UNIX s; `Some(0)`: running, since unknown).
    pub running_since: Option<u64>,
    pub health: Health,
    /// What makes it so (a failure, late, not set up, unknown).
    pub why: Option<String>,
    pub log: Option<String>,
}

/// One pod's last backup of one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cell {
    pub health: Health,
    /// When it last went through.
    pub ok_at: Option<u64>,
    /// When it was last tried.
    pub tried_at: Option<u64>,
    /// Short: `pushed`, `no changes`, `3.3 GB · w3d5`, `w3d5`, `-`.
    pub text: String,
    pub why: Option<String>,
    /// Judged by (set up, or recorded): counts for the pod's age.
    pub counts: bool,
}

impl Cell {
    fn new(health: Health, text: impl Into<String>) -> Cell {
        Cell { health, ok_at: None, tried_at: None, text: text.into(), why: None, counts: true }
    }

    fn off() -> Cell {
        Cell { counts: false, ..Cell::new(Health::Off, "-") }
    }

    fn why(mut self, why: impl Into<String>) -> Cell {
        self.why = Some(why.into());
        self
    }
}

/// One pod's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PodStatus {
    pub name: String,
    /// One of the pods asked about (else only in the record: a staff box named once, a pod gone).
    pub listed: bool,
    pub git: Cell,
    pub copy: Cell,
    pub archive: Cell,
    /// The worst of its cells.
    pub health: Health,
    /// Its worst cell's reason, named: `home copy: rsync exit 24 …`.
    pub why: Option<String>,
    /// The oldest of its last good backups (sorting; `None`: one never went through).
    pub oldest: Option<u64>,
    /// Its backup lock is held: a backup of it is running now.
    pub running: bool,
    /// Its recorded history, newest first.
    pub history: Vec<record::Event>,
    /// Its newest snapshot label on this box.
    pub label: Option<String>,
}

/// One place copies live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreStatus {
    pub what: String,
    /// Where: a path, a repo, a VM.
    pub place: String,
    pub free: Option<u64>,
    pub total: Option<u64>,
    pub health: Health,
    pub text: String,
}

/// Something under way now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningNow {
    pub what: String,
    pub since: Option<u64>,
}

/// The whole status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub generated_at: u64,
    pub prefix: String,
    pub jobs: Vec<JobStatus>,
    pub running: Vec<RunningNow>,
    /// Why "running now" can't be told, if it can't.
    pub running_unknown: Option<String>,
    pub pods: Vec<PodStatus>,
    pub storage: Vec<StoreStatus>,
    /// Copies skipped for room in the last day.
    pub skips: Vec<String>,
    pub notes: Vec<String>,
    /// The worst of the jobs and pods.
    pub health: Health,
}

// ---------------------------------------------------------------------------------------
// Judging
// ---------------------------------------------------------------------------------------

/// `12 min ago`, `3 h ago`, `16 days ago`, `just now` (pure).
pub fn ago(now: u64, t: u64) -> String {
    match now.saturating_sub(t) {
        s if s < 60 => "just now".into(),
        s => format!("{} ago", disk::age_words(s)),
    }
}

/// `in 4 min`, `in 2 h` (pure).
pub fn until(now: u64, t: u64) -> String {
    match t.saturating_sub(now) {
        s if s < 60 => "in under a minute".into(),
        s => format!("in {}", disk::age_words(s)),
    }
}

/// How late a scheduled job's last run may be before it's late: twice its longest gap, plus ten
/// minutes for a run's own length.
fn late_after(gap: u64) -> u64 {
    gap.saturating_mul(2) + 600
}

/// The crontab's backup lines: each kind's first line (managed or hand-added).
fn lines_by_kind(crontab: &str) -> BTreeMap<Kind, String> {
    let all = crate::cron::arena_lines(crontab);
    let mut out = BTreeMap::new();
    for l in all.managed.iter().chain(&all.unmanaged) {
        if let Some(k) = Kind::of(l) {
            out.entry(k).or_insert_with(|| l.clone());
        }
    }
    out
}

/// A line's schedule, read (`None`: unreadable — said by the caller).
fn schedule_of(line: &str) -> Option<Schedule> {
    let (text, _) = crate::cron::line_parts(line)?;
    Schedule::parse(&text).ok()
}

/// What a job's line sets up ([`Setup`]), and its schedule.
fn setup_of(job: Job, crontab: &Result<String, String>, archive: &ArchiveIn) -> (Setup, Option<Schedule>) {
    if matches!(job, Job::ArchiveSnap | Job::ArchiveBig) && matches!(archive, ArchiveIn::None) {
        return (Setup::Missing { why: "no backup archive".into(), fix: "arena archive up, then arena cron install --archive".into() }, None);
    }
    let lines = match crontab {
        Ok(t) => lines_by_kind(t),
        Err(why) => return (Setup::Unknown { why: format!("can't read the crontab ({why})") }, None),
    };
    let cron = |line: &String| -> (Setup, Option<Schedule>) {
        match schedule_of(line) {
            Some(s) => (Setup::Cron { schedule: s.text.clone(), every: s.describe() }, Some(s)),
            None => (Setup::Unknown { why: format!("can't read the schedule of `{line}`") }, None),
        }
    };
    match job {
        Job::Git => match lines.get(&Kind::Backup) {
            Some(l) => cron(l),
            None => (Setup::Missing { why: "no backup line in the crontab".into(), fix: "arena cron install --backup --pull".into() }, None),
        },
        Job::Copy => match lines.get(&Kind::Backup) {
            Some(l) if l.split_whitespace().any(|w| w == "--no-pull") => (
                Setup::Missing {
                    why: "the backup line runs `--no-pull`: only a pod git couldn't back up is copied".into(),
                    fix: "arena cron install --backup --pull".into(),
                },
                None,
            ),
            Some(l) => match cron(l) {
                (Setup::Cron { schedule, every }, s) => (Setup::Cron { schedule, every: format!("with each git backup ({every})") }, s),
                other => other,
            },
            None => (Setup::Missing { why: "no backup line in the crontab".into(), fix: "arena cron install --backup --pull".into() }, None),
        },
        Job::ArchiveSnap | Job::ArchiveBig => {
            let kind = if job == Job::ArchiveSnap { Kind::ArchiveSnap } else { Kind::ArchiveBig };
            match lines.get(&kind) {
                Some(l) => cron(l),
                None => (Setup::Missing { why: "the archive is up but no cron line pushes to it".into(), fix: "arena cron install --archive".into() }, None),
            }
        }
    }
}

/// The record's pods on main (`NotSaved`) in its last git run.
fn not_saved_in_last_run(rec: &Record) -> Vec<String> {
    let Some(run) = rec.runs.get(&Job::Git) else { return Vec::new() };
    let mut v: Vec<String> = rec
        .pods
        .iter()
        .filter(|(_, p)| p.git.as_ref().is_some_and(|e| e.outcome == Outcome::NotSaved && e.at >= run.started && e.at <= run.finished))
        .map(|(n, _)| n.clone())
        .collect();
    v.sort();
    v
}

/// Names, shortened to `a, b, c (+4 more)`.
fn names(v: &[String], max: usize) -> String {
    if v.len() <= max {
        v.join(", ")
    } else {
        format!("{} (+{} more)", v[..max].join(", "), v.len() - max)
    }
}

/// One of the record's jobs (git, copy): its row.
fn record_job(i: &Inputs, job: Job, setup: Setup, sched: Option<&Schedule>) -> JobStatus {
    let mut s = JobStatus { job, name: job.name(), setup, last: None, last_unknown: None, next: None, running_since: None, health: Health::Ok, why: None, log: None };
    let log = i.logs.get(&Kind::Backup);
    s.log = log.and_then(|l| l.as_ref().ok()).map(|l| l.path.clone());
    s.next = sched.and_then(|sc| sc.next_after(i.now, i.offset));
    let rec = match &i.record {
        Ok(r) => r.as_ref(),
        Err(why) => {
            s.last_unknown = Some(format!("the run record can't be read: {why}"));
            None
        }
    };
    if let Some(rec) = rec {
        let alive: Vec<&record::Running> = rec.running.iter().filter(|r| r.job == job && i.held.alive.contains(&r.pid)).collect();
        s.running_since = alive.iter().map(|r| r.started).min();
        if let Some(run) = rec.runs.get(&job) {
            s.last = Some(LastRun { started: run.started, finished: run.finished, ok: run.ok, summary: run.summary.clone() });
        }
    }
    // The cron's lock held with no recorded run of the line under way (git or copy): an arena
    // that keeps no record runs it — said on the git backup's row, the line's first step.
    let recorded = rec.is_some_and(|r| r.running.iter().any(|x| matches!(x.job, Job::Git | Job::Copy) && i.held.alive.contains(&x.pid)));
    if job == Job::Git && !recorded && matches!(i.held.cron, Some(Ok(true))) && matches!(s.setup, Setup::Cron { .. }) {
        s.running_since = Some(0);
    }
    if s.last.is_none() && s.last_unknown.is_none() {
        let written = log.and_then(|l| l.as_ref().ok()).and_then(|l| l.modified).map(|t| format!(" — its cron log was last written {}", ago(i.now, t))).unwrap_or_default();
        s.last_unknown = Some(format!("none recorded yet (this record starts with this arena version){written}"));
    }
    // The verdict, worst first.
    let (health, why) = match (&s.setup, &s.last) {
        (Setup::Unknown { why }, _) => (Health::Unknown, Some(why.clone())),
        (_, Some(l)) if !l.ok => (
            Health::Fail,
            Some(match rec.and_then(|r| r.runs.get(&job)) {
                Some(run) if !run.failed.is_empty() => format!("{} failed: {}", run.failed.len(), names(&run.failed, 4)),
                _ => "the last run failed".into(),
            }),
        ),
        (Setup::Missing { why, fix }, _) => (Health::Off, Some(format!("not set up ({why}) — `{fix}`"))),
        (Setup::Cron { every, .. }, Some(l)) if s.running_since.is_none() && sched.and_then(|sc| sc.longest_gap(i.now, i.offset)).is_some_and(|g| i.now.saturating_sub(l.finished) > late_after(g)) => {
            (Health::Warn, Some(format!("late: the last run ended {} ({every})", ago(i.now, l.finished))))
        }
        (_, None) => (Health::Unknown, s.last_unknown.clone()),
        _ => (Health::Ok, None),
    };
    s.health = health;
    s.why = why;
    // Pods on main: their work isn't in git (not a failure of the run).
    if job == Job::Git && s.health == Health::Ok {
        if let Some(rec) = rec {
            let on_main = not_saved_in_last_run(rec);
            if !on_main.is_empty() {
                s.health = Health::Warn;
                s.why = Some(format!("{} on main/master — not saved to git until `arena pods init-branches`", on_main.len()));
            }
        }
    }
    s
}

/// An archive tier's row.
fn archive_job(i: &Inputs, job: Job, setup: Setup, sched: Option<&Schedule>) -> JobStatus {
    let mut s = JobStatus { job, name: job.name(), setup, last: None, last_unknown: None, next: None, running_since: None, health: Health::Ok, why: None, log: None };
    s.log = i.logs.get(&if job == Job::ArchiveSnap { Kind::ArchiveSnap } else { Kind::ArchiveBig }).and_then(|l| l.as_ref().ok()).map(|l| l.path.clone());
    s.next = sched.and_then(|sc| sc.next_after(i.now, i.offset));
    if let Some(Ok(true)) = i.held.archive.get(&job) {
        s.running_since = Some(0);
    }
    let (run, err, failing) = match &i.archive {
        ArchiveIn::None => {
            if let Setup::Missing { why, fix } = &s.setup {
                s.why = Some(format!("not set up ({why}) — `{fix}`"));
            }
            s.health = Health::Off;
            return s;
        }
        ArchiveIn::Unreadable(why) => {
            s.health = Health::Unknown;
            s.why = Some(format!("the archive's state can't be read: {why}"));
            s.last_unknown = s.why.clone();
            return s;
        }
        ArchiveIn::Up { host, snap, big } => {
            if !host.is_up() {
                s.health = Health::Fail;
                s.why = Some("the archive VM is down (`arena archive down` ran) — `arena archive up`".into());
            }
            let read = if job == Job::ArchiveSnap {
                snap.as_ref().map(|st| (st.last_run, st.last_error.clone(), st.pods_failing.clone()))
            } else {
                big.as_ref().map(|st| (st.last_run, st.last_error.clone(), st.pods_failing.clone()))
            };
            match read {
                Ok(r) => r,
                Err(why) => {
                    s.health = Health::Unknown;
                    s.why = Some(format!("its sync state can't be read: {why}"));
                    s.last_unknown = s.why.clone();
                    return s;
                }
            }
        }
    };
    match run {
        Some(r) => {
            let summary = if r.ok {
                "pushed".to_string()
            } else if r.all_failed {
                "every push failed".to_string()
            } else {
                "some pushes failed".to_string()
            };
            s.last = Some(LastRun { started: r.started, finished: r.finished, ok: r.ok, summary });
        }
        None => s.last_unknown = Some("no push yet".into()),
    }
    if s.health != Health::Ok {
        return s;
    }
    let (health, why) = match (&s.setup, &s.last) {
        (_, Some(l)) if !l.ok => (Health::Fail, Some(err.map_or_else(|| "the last push failed".to_string(), |e| format!("{}: {}", e.class, e.message)))),
        (Setup::Unknown { why }, _) => (Health::Unknown, Some(why.clone())),
        (Setup::Missing { why, fix }, _) => (Health::Off, Some(format!("not set up ({why}) — `{fix}`"))),
        _ if !failing.is_empty() => {
            let pods: Vec<String> = failing.keys().cloned().collect();
            (Health::Warn, Some(format!("{} pod(s) failing: {}", pods.len(), names(&pods, 4))))
        }
        (Setup::Cron { every, .. }, Some(l)) if s.running_since.is_none() && sched.and_then(|sc| sc.longest_gap(i.now, i.offset)).is_some_and(|g| i.now.saturating_sub(l.finished) > late_after(g)) => {
            (Health::Warn, Some(format!("late: the last push ended {} ({every})", ago(i.now, l.finished))))
        }
        (_, None) => (Health::Unknown, s.last_unknown.clone()),
        _ => (Health::Ok, None),
    };
    s.health = health;
    s.why = why;
    s
}

/// A pod's git cell.
fn git_cell(i: &Inputs, pod: &PodIn, gap: Option<u64>) -> Cell {
    let rec = match &i.record {
        Err(why) => return Cell::new(Health::Unknown, "?").why(format!("the run record can't be read: {why}")),
        Ok(r) => r.as_ref().and_then(|r| r.pods.get(&pod.name)),
    };
    let Some(p) = rec else {
        let text = pod.last_commit.map_or_else(|| "no record".to_string(), |t| format!("commit {}", render::short_age(i.now.saturating_sub(t))));
        return Cell { tried_at: None, ..Cell::new(Health::Unknown, text).why("no git backup recorded for it yet") };
    };
    let Some(e) = &p.git else {
        return Cell::new(Health::Unknown, "no record").why("no git backup recorded for it yet");
    };
    let mut c = Cell { ok_at: p.git_ok, tried_at: Some(e.at), ..Cell::new(Health::Ok, e.outcome.words()) };
    match e.outcome {
        Outcome::NotSaved => {
            c.health = Health::Warn;
            c.text = "on main".into();
            c.why = Some(format!("{} — not saved to git until `arena pods init-branches {}`", e.detail, pod.name));
        }
        Outcome::Failed => {
            c.health = Health::Fail;
            c.text = "failed".into();
            c.why = Some(e.detail.clone());
        }
        _ => {}
    }
    stale(i, pod, c, gap)
}

/// A pod's home-copy cell.
fn copy_cell(i: &Inputs, pod: &PodIn, gap: Option<u64>, set_up: bool) -> Cell {
    let label = i.labels.as_ref().ok().and_then(|l| l.get(&pod.name)).cloned();
    let rec = match &i.record {
        Err(why) => return Cell::new(Health::Unknown, label.unwrap_or_else(|| "?".into())).why(format!("the run record can't be read: {why}")),
        Ok(r) => r.as_ref().and_then(|r| r.pods.get(&pod.name)).and_then(|p| p.copy.clone().map(|e| (e, p.copy_ok))),
    };
    let marker = i.copied.as_ref().ok().and_then(|m| m.get(&pod.name)).copied();
    let mut c = match rec {
        Some((e, ok_at)) => {
            let size = e.bytes.map(disk::gb);
            let text = match (&size, e.outcome) {
                (_, Outcome::Failed) => "failed".to_string(),
                (Some(s), _) => format!("{s}{}", label.as_ref().map(|l| format!(" · {l}")).unwrap_or_default()),
                (None, _) => label.clone().unwrap_or_else(|| "copied".into()),
            };
            let mut c = Cell { ok_at, tried_at: Some(e.at), ..Cell::new(Health::Ok, text) };
            if e.outcome == Outcome::Failed {
                c.health = Health::Fail;
                c.why = Some(e.detail.clone());
            }
            c
        }
        // No record yet: the last whole copy's marker is a record of its own.
        None => match marker {
            Some(t) => Cell { ok_at: Some(t), tried_at: Some(t), ..Cell::new(Health::Ok, label.clone().unwrap_or_else(|| "copied".into())) },
            None if !set_up && label.is_none() => return Cell::off(),
            None => {
                let why = match (&i.copied, &label) {
                    (Err(e), _) => format!("can't read the copies' markers: {e}"),
                    (_, Some(l)) => format!("no copy recorded; its newest snapshot here is {l}"),
                    _ => "no copy of it recorded or found here".into(),
                };
                Cell::new(Health::Unknown, label.clone().unwrap_or_else(|| "?".into())).why(why)
            }
        },
    };
    // The record says it went through; the backup dir can't be read now: what's there is unknown
    // (review r3 #1) — the last success kept.
    if c.health == Health::Ok {
        if let Some(e) = i.labels.as_ref().err().or(i.copied.as_ref().err()) {
            c.health = Health::Unknown;
            c.why = Some(format!("the backup dir can't be read: {e}"));
        }
    }
    stale(i, pod, c, gap)
}

/// A cell gone quiet: a cohort pod that's up, with a job on a schedule, not backed up for twice
/// its longest gap — late.
fn stale(i: &Inputs, pod: &PodIn, mut c: Cell, gap: Option<u64>) -> Cell {
    let (Some(gap), Some(tried)) = (gap, c.tried_at) else { return c };
    if c.health != Health::Ok || !pod.cohort || pod.up == Some(false) {
        return c;
    }
    let since = c.ok_at.unwrap_or(tried);
    if i.now.saturating_sub(since) > late_after(gap) {
        c.health = Health::Warn;
        c.why = Some(format!("not backed up for {} (due every {})", disk::age_words(i.now.saturating_sub(since)), disk::age_words(gap)));
    }
    c
}

/// A pod's archive cell: on each tier the archive pushes on a schedule (`gaps`: its longest gap,
/// by job), whether the pod is there at all and how fresh — missing is unknown, older than twice
/// the tier's gap (for a cohort pod that's up) late — then a push failing for it, worst (review r2
/// #2: an old copy read as fine for ever, and either tier's stood in for both). A staff box isn't
/// archived: off.
fn archive_cell(i: &Inputs, pod: &PodIn, gaps: &BTreeMap<Job, u64>) -> Cell {
    let (snap, big) = match &i.archive {
        ArchiveIn::None => return Cell::off(),
        ArchiveIn::Unreadable(why) => return Cell::new(Health::Unknown, "?").why(format!("the archive's state can't be read: {why}")),
        ArchiveIn::Up { snap, big, .. } => (snap, big),
    };
    let (snap, big) = match (snap, big) {
        (Ok(s), Ok(b)) => (s, b),
        (Err(why), _) | (_, Err(why)) => return Cell::new(Health::Unknown, "?").why(format!("the archive's sync state can't be read: {why}")),
    };
    // The newest label on the archive holding this pod's copy, and when its push ended.
    let mut labels: Vec<(&String, &crate::archive::state::LabelRecord)> = snap.labels.iter().filter(|(_, r)| r.pods_ok.contains(&pod.name)).collect();
    labels.sort_by_key(|(l, _)| (crate::restore::parse_label(l), (*l).clone()));
    let newest = labels.last();
    let big_at = big.pods_ok_at.get(&pod.name).copied();
    let snap_at = newest.map(|(_, r)| r.ok_finished.max(r.last_tried)).filter(|t| *t > 0);
    if newest.is_none() && big_at.is_none() && !pod.cohort {
        return Cell::off();
    }
    let ok_at = match (snap_at, big_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let text = match (newest, big_at) {
        (Some((l, _)), Some(_)) => format!("{l} + all files"),
        (Some((l, _)), None) => (*l).clone(),
        (None, Some(_)) => "all files".into(),
        (None, None) => "-".into(),
    };
    let mut c = Cell { ok_at, tried_at: ok_at, ..Cell::new(Health::Ok, text) };
    let failing: Vec<String> = [snap.pods_failing.get(&pod.name).map(|w| format!("snapshots: {w}")), big.pods_failing.get(&pod.name).map(|w| format!("all files: {w}"))]
        .into_iter()
        .flatten()
        .collect();
    if !failing.is_empty() {
        c.health = Health::Fail;
        c.why = Some(failing.join("; "));
        return c;
    }
    // Each scheduled tier: there, and fresh.
    let judged = pod.cohort && pod.up != Some(false);
    let mut verdicts: Vec<(Health, String)> = Vec::new();
    for (job, at, tier) in [(Job::ArchiveSnap, snap_at, "snapshots"), (Job::ArchiveBig, big_at, "all files")] {
        let Some(gap) = gaps.get(&job) else { continue };
        match at {
            None => verdicts.push((Health::Unknown, format!("not on the archive's {tier} yet"))),
            Some(t) if judged && i.now.saturating_sub(t) > late_after(*gap) => verdicts.push((
                Health::Warn,
                format!("{tier}: last pushed {} ago (due every {})", disk::age_words(i.now.saturating_sub(t)), disk::age_words(*gap)),
            )),
            Some(_) => {}
        }
    }
    if verdicts.is_empty() && newest.is_none() && big_at.is_none() {
        verdicts.push((Health::Unknown, "not on the archive yet".into()));
    }
    if let Some(worst) = verdicts.iter().map(|(h, _)| *h).min() {
        c.health = worst;
        c.why = Some(verdicts.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join("; "));
    }
    c
}

/// Judge it all (pure).
pub fn judge(i: &Inputs) -> Status {
    let mut notes = Vec::new();
    let mut jobs = Vec::new();
    let mut gaps: BTreeMap<Job, u64> = BTreeMap::new();
    for job in Job::ALL {
        let (setup, sched) = setup_of(job, &i.crontab, &i.archive);
        if let (Setup::Cron { .. }, Some(s)) = (&setup, &sched) {
            if let Some(g) = s.longest_gap(i.now, i.offset) {
                gaps.insert(job, g);
            }
        }
        jobs.push(match job {
            Job::Git | Job::Copy => record_job(i, job, setup, sched.as_ref()),
            _ => archive_job(i, job, setup, sched.as_ref()),
        });
    }
    // The cron's backup line and this run reading different configs: the line's records (and its
    // state dir) may be another's.
    if let (Some((theirs, false)), Some(mine)) = (&i.cron_config, &i.config_path) {
        notes.push(format!("the cron's backup line runs with --config {theirs}; this reads {mine} — if their state dirs differ, its runs aren't in this record"));
    }
    if let Ok(Some(rec)) = &i.record {
        for r in rec.running.iter().filter(|r| !i.held.alive.contains(&r.pid)) {
            notes.push(format!("a {} run started {} (pid {}) ended without finishing — killed or crashed", r.job.name(), ago(i.now, r.started), r.pid));
        }
    }

    // Running now.
    let mut running = Vec::new();
    let mut running_unknown = None;
    for j in &jobs {
        match j.running_since {
            Some(0) if matches!(j.job, Job::Git | Job::Copy) => running.push(RunningNow { what: "the cron's backup (pods backup)".into(), since: None }),
            Some(0) => running.push(RunningNow { what: format!("{} (archive sync)", j.name), since: None }),
            Some(t) => running.push(RunningNow { what: j.name.to_string(), since: Some(t) }),
            None => {}
        }
    }
    running.dedup();
    let busy: BTreeSet<String> = match &i.held.pods {
        Ok(p) => p.clone(),
        Err(why) => {
            running_unknown = Some(why.clone());
            BTreeSet::new()
        }
    };
    if let Some(Err(why)) = &i.held.cron {
        running_unknown.get_or_insert_with(|| why.clone());
    }
    if !busy.is_empty() {
        let v: Vec<String> = busy.iter().cloned().collect();
        running.push(RunningNow { what: format!("backing up now: {}", names(&v, 6)), since: None });
    }

    // Pods: the ones asked about, and the record's others seen in the last three days.
    let mut pods_in: Vec<(PodIn, bool)> = i.pods.iter().map(|p| (p.clone(), true)).collect();
    if let Ok(Some(rec)) = &i.record {
        for (name, p) in &rec.pods {
            if !pods_in.iter().any(|(q, _)| q.name == *name) && i.now.saturating_sub(p.last_at()) < 3 * 86_400 {
                pods_in.push((PodIn { name: name.clone(), ..PodIn::default() }, false));
            }
        }
    }
    let copy_set_up = matches!(jobs[1].setup, Setup::Cron { .. });
    let mut pods: Vec<PodStatus> = pods_in
        .into_iter()
        .map(|(p, listed)| {
            let git = git_cell(i, &p, gaps.get(&Job::Git).copied());
            let copy = copy_cell(i, &p, gaps.get(&Job::Copy).copied(), copy_set_up);
            let archive = archive_cell(i, &p, &gaps);
            let cells = [("git", &git), ("home copy", &copy), ("archive", &archive)];
            let worst = cells.iter().filter(|(_, c)| c.health != Health::Off).min_by_key(|(_, c)| c.health);
            let health = worst.map_or(Health::Off, |(_, c)| c.health);
            let why = worst.and_then(|(n, c)| c.why.as_ref().map(|w| format!("{n}: {w}")));
            let oldest = cells.iter().filter(|(_, c)| c.counts && c.health != Health::Off).map(|(_, c)| c.ok_at).min().flatten();
            let history = i.record.as_ref().ok().and_then(|r| r.as_ref()).and_then(|r| r.pods.get(&p.name)).map(|r| r.history.clone()).unwrap_or_default();
            let label = i.labels.as_ref().ok().and_then(|l| l.get(&p.name)).cloned();
            PodStatus { running: busy.contains(&p.name), name: p.name, listed, git, copy, archive, health, why, oldest, history, label }
        })
        .collect();
    pods.sort_by(|a, b| (a.health, a.oldest, &a.name).cmp(&(b.health, b.oldest, &b.name)));

    // Storage: where each copy lives, judged by the disk guard.
    let mut storage: Vec<StoreStatus> = Vec::new();
    let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
    for s in &i.storage {
        match &s.fs {
            Ok(fs) => {
                if let Some(&row) = seen.get(&fs.dev) {
                    // Same filesystem as one above: one row says it.
                    let row = &mut storage[row];
                    row.what = format!("{} + {}", row.what, s.what);
                    row.place = format!("{}, {}", row.place, s.path);
                    continue;
                }
                seen.insert(fs.dev, storage.len());
                let (health, words) = match &i.floor {
                    Ok(floor) => match disk::level(fs.free, *floor) {
                        Level::Ok => (Health::Ok, String::new()),
                        Level::Near => (Health::Warn, format!(" — under twice the floor ({}, CONTROL_MIN_FREE_GB): the next big copy may be skipped", disk::gb(*floor))),
                        Level::Below => (Health::Fail, format!(" — UNDER the floor ({}): every copy onto it is skipped", disk::gb(*floor))),
                    },
                    Err(why) => (Health::Unknown, format!(" — {why}")),
                };
                storage.push(StoreStatus {
                    what: s.what.clone(),
                    place: s.path.clone(),
                    free: Some(fs.free),
                    total: Some(fs.total),
                    health,
                    text: format!("{} free of {} on {}{words}", disk::gb(fs.free), disk::gb(fs.total), fs.mount.display()),
                });
            }
            Err(why) => storage.push(StoreStatus { what: s.what.clone(), place: s.path.clone(), free: None, total: None, health: Health::Unknown, text: format!("free space unknown ({why})") }),
        }
    }
    storage.push(match &i.repo {
        Some(r) => StoreStatus {
            what: "git".into(),
            place: format!("github.com/{r}"),
            free: None,
            total: None,
            health: Health::Ok,
            text: format!("each pod's branch (autocommit-{}-wNdM-<pod>)", i.prefix),
        },
        None => StoreStatus {
            what: "git".into(),
            place: "-".into(),
            free: None,
            total: None,
            health: Health::Unknown,
            text: "no cohort repo configured (ARENA_REPO_OWNER / ARENA_REPO_NAME): a pod's origin isn't checked".into(),
        },
    });
    match &i.archive {
        ArchiveIn::Up { host, .. } => storage.push(StoreStatus {
            what: "archive".into(),
            place: if host.is_up() { format!("{}-archive ({}, {})", i.prefix, host.ipv4, host.location) } else { format!("{}-archive (down)", i.prefix) },
            free: None,
            total: None,
            health: if host.is_up() { Health::Ok } else { Health::Fail },
            text: format!("snapshots {} GB, all files {} GB volumes — free space: `arena archive status`", host.snap_gb, host.big_gb),
        }),
        ArchiveIn::Unreadable(why) => storage.push(StoreStatus { what: "archive".into(), place: "?".into(), free: None, total: None, health: Health::Unknown, text: why.clone() }),
        ArchiveIn::None => {}
    }
    let skips = match &i.skips {
        Ok(v) => v.iter().map(|e| e.line(i.now)).collect(),
        Err(why) => vec![format!("? copies skipped for room: unknown ({why})")],
    };

    let health = jobs.iter().map(|j| j.health).chain(pods.iter().map(|p| p.health)).chain(storage.iter().map(|s| s.health)).filter(|h| *h != Health::Off).min().unwrap_or(Health::Off);
    Status { generated_at: i.now, prefix: i.prefix.clone(), jobs, running, running_unknown, pods, storage, skips, notes, health }
}

// ---------------------------------------------------------------------------------------
// Reading what's there
// ---------------------------------------------------------------------------------------

/// `crontab -l`, as text: no crontab at all is an empty one; anything else that fails is `Err`.
pub fn read_crontab() -> Result<String, String> {
    let out = std::process::Command::new("crontab")
        .arg("-l")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("can't run crontab -l: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("no crontab for") {
        Ok(String::new())
    } else {
        Err(format!("crontab -l: {}", err.trim()))
    }
}

/// `LOCAL_BACKUP_DIR` (default `./backup`).
pub fn backup_dir(cfg: &Config) -> String {
    cfg.get("LOCAL_BACKUP_DIR").filter(|s| !s.is_empty()).unwrap_or("./backup").to_string()
}

/// The all-files tier's root: `LOCAL_BACKUP_BIG_DIR`, else `<LOCAL_BACKUP_DIR>/big`.
pub fn big_root(cfg: &Config) -> String {
    let big = cfg.get("LOCAL_BACKUP_BIG_DIR").map(str::trim).filter(|d| !d.is_empty());
    crate::pull::big_root(&backup_dir(cfg), big)
}

/// Each `.arena-copied/<pod>`'s mtime under `base`. Missing = none yet; any read that fails —
/// the folder, an entry, its time — is `Err` (review r3 #1: never a quiet "none").
fn read_copied(base: &Path) -> Result<BTreeMap<String, u64>, String> {
    let dir = base.join(disk::COPIED_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = BTreeMap::new();
    for e in entries {
        let e = e.map_err(|e| format!("{}: {e}", dir.display()))?;
        let at = e.metadata().and_then(|m| m.modified()).map_err(|err| format!("{}: {err}", e.path().display()))?;
        out.insert(e.file_name().to_string_lossy().into_owned(), at.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    }
    Ok(out)
}

/// Each pod's newest `wNdM` snapshot under `base`: one listing of the labels, one of each.
/// Missing = none yet; a listing that fails is `Err`, as [`read_copied`].
fn read_labels(base: &Path) -> Result<BTreeMap<String, String>, String> {
    let entries = match std::fs::read_dir(base) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("{}: {e}", base.display())),
    };
    let mut labels: Vec<((u32, u32), String)> = Vec::new();
    for e in entries {
        let l = e.map_err(|e| format!("{}: {e}", base.display()))?.file_name().to_string_lossy().into_owned();
        if let Some(key) = crate::restore::parse_label(&l) {
            labels.push((key, l));
        }
    }
    labels.sort();
    let mut out = BTreeMap::new();
    for (_, l) in labels {
        let dir = base.join(&l);
        for pod in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let pod = pod.map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = pod.file_name().to_string_lossy().into_owned();
            // An entry whose type can't be read fails the listing, like a listing that
            // fails: skipped, its pod would just look copy-less (review codex r4).
            let is_dir = pod.file_type().map_err(|e| format!("{}: {e}", pod.path().display()))?.is_dir();
            if !name.starts_with('.') && is_dir {
                out.insert(name, l.clone());
            }
        }
    }
    Ok(out)
}

/// The archive as found in the state dir.
fn read_archive(cfg: &Config) -> ArchiveIn {
    use crate::archive::state::{load_big, load_host, load_snap, ArchivePaths};
    let paths = match ArchivePaths::for_config(cfg) {
        Ok(p) => p,
        Err(why) => return ArchiveIn::Unreadable(why),
    };
    match load_host(&paths.host_file()) {
        Ok(None) => ArchiveIn::None,
        Err(why) => ArchiveIn::Unreadable(why),
        Ok(Some(host)) => ArchiveIn::Up { host: Box::new(host), snap: load_snap(&paths.sync_state(Tier::Snap)), big: load_big(&paths.sync_state(Tier::Big)) },
    }
}

/// A log's facts.
fn read_log(path: &str) -> Result<LogFacts, String> {
    let m = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    Ok(LogFacts { path: path.into(), bytes: m.len(), modified })
}

/// Read everything [`judge`] needs: the crontab (`crontab`: what `crontab -l` gave — the caller
/// reads it, so a test passes its own), the run record, the backup dir's markers and labels, the
/// archive's state, the locks held, the logs, the filesystems. `pods`: the pods to report (the
/// dashboard's rows, or the cohort's list). Blocking I/O, bounded and local (no network, no
/// pod): a caller in async code runs it off its runtime.
pub fn read(cfg: &Config, config_path: Option<&str>, crontab: Result<String, String>, pods: Vec<PodIn>, now: u64) -> Inputs {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let record_path = crate::snapshot::state_file_path_for(cfg, record::RECORD_FILE);
    let (record, record_path) = match &record_path {
        Ok(p) => (Record::load(p), p.display().to_string()),
        Err(why) => (Err(why.clone()), "?".into()),
    };
    let dir = backup_dir(cfg);
    let base = std::path::absolute(&dir).unwrap_or_else(|_| PathBuf::from(&dir));
    let lines = crontab.as_ref().map(|t| lines_by_kind(t)).unwrap_or_default();
    let held_now = locks::held();
    let is_held = |p: &Path| held_now.as_ref().map_err(Clone::clone).and_then(|h| locks::is_held(p, h));
    // The backup line's lock: its `flock` file as the line names it.
    let cron = lines.get(&Kind::Backup).map(|l| match crate::cron::lock_target(l) {
        Some(lock) => is_held(Path::new(lock)),
        None => Ok(false),
    });
    let pods_held = match &held_now {
        Err(why) => Err(why.clone()),
        Ok(h) => {
            let names: BTreeSet<String> = pods.iter().map(|p| p.name.clone()).chain(record.as_ref().ok().and_then(|r| r.as_ref()).into_iter().flat_map(|r| r.pods.keys().cloned())).collect();
            Ok(names.into_iter().filter(|n| locks::is_held(&crate::pull::pod_lock_path(&dir, n), h).unwrap_or(false)).collect())
        }
    };
    let mut archive_held = BTreeMap::new();
    if let Ok(paths) = crate::archive::state::ArchivePaths::for_config(cfg) {
        for (job, tier) in [(Job::ArchiveSnap, Tier::Snap), (Job::ArchiveBig, Tier::Big)] {
            archive_held.insert(job, is_held(&paths.sync_lock(tier)));
        }
    }
    let alive = record.as_ref().ok().and_then(|r| r.as_ref()).map(|r| r.running.iter().map(|x| x.pid).filter(|p| disk::arena_alive(*p)).collect()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let logs = lines
        .iter()
        .filter(|(k, _)| matches!(k, Kind::Backup | Kind::ArchiveSnap | Kind::ArchiveBig))
        .map(|(k, l)| {
            let path = crate::cron::log_target(l).map(String::from).unwrap_or_else(|| crate::cron::log(&home, k.job()));
            (*k, read_log(&path))
        })
        .collect();
    let big = big_root(cfg);
    let mut storage = vec![StoreIn { what: "snapshots".into(), path: base.display().to_string(), fs: disk::reading(&base) }];
    let big_abs = std::path::absolute(&big).unwrap_or_else(|_| PathBuf::from(&big));
    storage.push(StoreIn { what: "all files".into(), path: big_abs.display().to_string(), fs: disk::reading(&big_abs) });
    let skips = match crate::snapshot::state_file_path_for(cfg, disk::SKIPS_FILE) {
        Ok(p) => {
            let (s, warning) = disk::Skips::load(&p);
            match warning {
                Some(w) => Err(w),
                None => Ok(s.recent(now).into_iter().cloned().collect()),
            }
        }
        Err(why) => Err(why),
    };
    let cron_config = lines.get(&Kind::Backup).and_then(|l| crate::cron::config_target(l)).zip(config_path).map(|(theirs, mine)| {
        let canon = |p: &str| std::fs::canonicalize(p).ok();
        let same = theirs == mine || canon(theirs).is_some_and(|a| Some(a) == canon(mine));
        (theirs.to_string(), same)
    });
    Inputs {
        now,
        offset: crate::cron::local_offset(now),
        prefix,
        config_path: config_path.map(String::from),
        cron_config,
        crontab,
        record,
        record_path,
        pods,
        backup_dir: dir,
        copied: read_copied(&base),
        labels: read_labels(&base),
        archive: read_archive(cfg),
        held: Held { cron, pods: pods_held, archive: archive_held, alive },
        logs,
        storage,
        floor: disk::floor_bytes(cfg),
        skips,
        repo: crate::backup::configured_repo(cfg),
    }
}

/// This cohort's pods by the config's list (`MACHINE_NAME_LIST`, staff `@` entries left out) —
/// what `arena backups` reports when it isn't given the fleet's listing.
pub fn cohort_pods(cfg: &Config) -> Vec<PodIn> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    cfg.machine_names
        .iter()
        .filter(|e| !crate::naming::is_absolute(e))
        .map(|e| PodIn { name: crate::naming::qualify(prefix, e), cohort: true, ..PodIn::default() })
        .collect()
}

#[cfg(test)]
mod tests;
