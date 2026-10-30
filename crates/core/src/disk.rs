//! The control box's disk: the machine arena runs on, which every copy of a pod lands on — the
//! backup's file tiers (`pods pull`, `pods backup`'s pull, its fallback copy of a pod git didn't
//! back up and its BUSY pods' copies), `pods baseline`, replace/migrate's local staging and the
//! reaper's last copy. Live (2026-10-30) prod's box sat at 96 % (3.1 GB free of 75 GB) while
//! root's */15 backup cron copied pods onto it with nothing checking, and a plain `pods backup`
//! of a GPU image home had filled it to 100 % once (2026-10-20).
//!
//! So every copy onto it goes through one judge ([`judge`], and [`Ledger`] for a run's many):
//! it starts only when what the filesystem has free, less what this run's earlier copies there
//! may take, what other arena runs' copies under way hold ([`Reservations`]) and what this one may
//! take, stays at or above the floor (`CONTROL_MIN_FREE_GB`, default 5 GB). A copy's size is what
//! arena measured where it measures one (replace/migrate's staging: an rsync dry run); otherwise a
//! pod's first copy counts as [`FIRST_COPY`] and a later one, which writes only what changed, as
//! [`LATER_COPY`] — later only once that pod was copied whole there ([`was_copied`]). A skip
//! happens before anything is written, and is a failure of its run. An estimate isn't a bound, so
//! the floor is also a hard line while a copy writes: under it, the copy is stopped
//! ([`Skip::under_floor_now`]; the watch is the CLI's).
//!
//! Also here: what arena leaves behind on the box ([`find_leftovers`]: a replace/migrate staging
//! copy kept after a copy that failed or was stopped, an interrupted baseline capture, rsync temp
//! files a killed pull left) — listed, never removed by a look, since a staging copy can be the
//! only copy of someone's work —, the warning levels ([`level`]), the biggest of arena's dirs on a
//! low filesystem ([`biggest`]), and the record of skips the daily health check reports
//! ([`record_skips`]). The rules and words are pure; the I/O (a `statvfs`, a bounded walk, a
//! small state file) sits beside them, simple enough to test against a temp dir.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// The setting: what every copy onto this box must leave free on its filesystem, in GB.
pub const FLOOR_KEY: &str = "CONTROL_MIN_FREE_GB";
/// [`FLOOR_KEY`] unset: 5 GB — room for the box's own logs, the proxy and the cohort's
/// dashboards on a disk shared with them, once a copy is done.
pub const DEFAULT_FLOOR_GB: u64 = 5;
const MAX_FLOOR_GB: u64 = 100_000;
/// A gigabyte as `df -H` and the settings count it (10⁹ bytes).
pub const GB: u64 = 1_000_000_000;
/// A pod's first copy into a destination, when nothing measured it: about a fresh GPU image's
/// home, at the high end (live 4: a RunPod image home took ~2.1 GB; live: a 1.4 GB pod wrote 2.8 GB
/// over both tiers; live 2026-10-30: a RunPod A4500 pod's 3.9 GB home, 3.0 GB a tier).
pub const FIRST_COPY: u64 = 3 * GB;
/// A later copy onto an earlier one of the same pod, when nothing measured it: rsync writes only
/// what changed (a snapshot hard-links the rest to the pod's previous one), usually far less.
pub const LATER_COPY: u64 = GB / 10;

/// [`FLOOR_KEY`] in bytes: a whole number of GB, `0..=100000` (`0`: only the copy itself must
/// fit), unset = [`DEFAULT_FLOOR_GB`]. `Err` names the key and the rule.
pub fn floor_bytes(cfg: &Config) -> Result<u64, String> {
    match cfg.get(FLOOR_KEY).map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(DEFAULT_FLOOR_GB * GB),
        Some(v) => v.parse::<u64>().ok().filter(|n| *n <= MAX_FLOOR_GB).map(|n| n * GB).ok_or_else(|| {
            let shown: String = v.chars().filter(|c| !c.is_control()).take(24).collect();
            format!("{FLOOR_KEY} must be a whole number of GB, 0..={MAX_FLOOR_GB} (got `{shown}`)")
        }),
    }
}

/// The free space a copy must leave, and the setting that says so (named in a skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floor {
    pub bytes: u64,
    pub key: &'static str,
}

impl Floor {
    /// `CONTROL_MIN_FREE_GB`'s floor.
    pub fn control(bytes: u64) -> Floor {
        Floor { bytes, key: FLOOR_KEY }
    }

    /// The stricter of two floors: the reaper keeps its own `REAP_MIN_FREE_GB` (room for a
    /// pod's last copy) and never goes under the box's.
    pub fn max(self, other: Floor) -> Floor {
        if other.bytes > self.bytes {
            other
        } else {
            self
        }
    }
}

/// What a copy may add to the disk, as the judge counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Measured before the copy (replace/migrate's staging: an rsync dry run).
    Measured(u64),
    /// Not measured, and no copy of this pod there yet: [`FIRST_COPY`].
    First,
    /// Not measured, onto an earlier copy of this pod: [`LATER_COPY`].
    Later,
    /// Counted in the floor itself — the reaper's `REAP_MIN_FREE_GB` is the room for its copy.
    InFloor,
}

impl Size {
    pub fn bytes(self) -> u64 {
        match self {
            Size::Measured(b) => b,
            Size::First => FIRST_COPY,
            Size::Later => LATER_COPY,
            Size::InFloor => 0,
        }
    }
}

/// What one copy needs (its parts on one filesystem added up), in bytes and in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub bytes: u64,
    words: String,
}

impl Need {
    /// The parts of one copy that write to the same filesystem (a pod's two tiers): their sizes
    /// added, said by kind.
    pub fn of(parts: &[Size]) -> Need {
        let bytes = parts.iter().map(|s| s.bytes()).fold(0u64, u64::saturating_add);
        let all = |f: fn(&Size) -> bool| !parts.is_empty() && parts.iter().all(f);
        let how = if all(|s| matches!(s, Size::Measured(_))) {
            "measured"
        } else if all(|s| matches!(s, Size::First)) {
            "a first copy, not measured"
        } else if all(|s| matches!(s, Size::Later)) {
            "what changed since the last copy, not measured"
        } else {
            "not measured"
        };
        Need { bytes, words: format!("~{}, {how}", gb(bytes)) }
    }
}

impl From<Size> for Need {
    fn from(s: Size) -> Need {
        Need::of(&[s])
    }
}

/// One filesystem, read: its device, where it's mounted, what an unprivileged writer may still
/// use (`statvfs`'s `f_bavail`, as `df` shows it) and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fs {
    pub dev: u64,
    pub mount: PathBuf,
    pub free: u64,
    pub total: u64,
}

/// Why a copy wasn't started ([`judge`]): said on the pod's line, `skipped: …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// The space it found and why that isn't enough (`only 3.1 GB free on / (floor 5 GB)`).
    pub reason: String,
    /// What to do about it.
    advice: String,
    /// The copy was stopped while it wrote ([`Skip::under_floor_now`]), not skipped before.
    pub stopped: bool,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stopped {
            write!(f, "stopped: {} — what it copied so far stays; {}", self.reason, self.advice)
        } else {
            write!(f, "skipped: {} — nothing copied; {}", self.reason, self.advice)
        }
    }
}

impl Skip {
    /// The free space couldn't be read: never copy blind.
    pub fn unknown(path: &Path, why: &str) -> Skip {
        Skip { reason: format!("free space on {} unknown ({why})", path.display()), advice: "check that disk".into(), stopped: false }
    }

    /// What other arena runs' copies under way hold couldn't be read, or this run's couldn't be
    /// noted for them ([`Reservations`]): never count free space another run may be counting.
    pub fn others_unknown(why: &str) -> Skip {
        Skip {
            reason: format!("other arena runs' copies onto this box unknown ({why})"),
            advice: "check the state dir (`arena config which`), then re-run".into(),
            stopped: false,
        }
    }

    /// A copy stopped while it wrote: the filesystem went under the floor (a copy writes more
    /// than its estimate — a new big file — or something else filled the disk meanwhile).
    pub fn under_floor_now(fs: &Fs, floor: Floor) -> Skip {
        let mount = fs.mount.display();
        Skip {
            reason: format!("only {} free on {mount}{WHILE_COPYING}(floor {}, {})", gb(fs.free), gb(floor.bytes), floor.key),
            advice: format!("free space on {mount} (`arena config check` names arena's biggest dirs there)"),
            stopped: true,
        }
    }
}

/// What a stopped copy's reason says ([`Skip::under_floor_now`]), as the skip record tells one.
const WHILE_COPYING: &str = " while copying ";

/// The judge (pure): a copy needing `need` may start on `fs` when what's free, less `already`
/// (what this run's earlier copies and other runs' copies under way there may take) and `need`,
/// stays at or above `floor`.
/// `Err` says how far off it is, for the copy's line — under the floor, with what it would take
/// in all (an operator can't tell how much to free from the free space alone: review #42).
pub fn judge(fs: &Fs, already: u64, need: &Need, floor: Floor) -> Result<(), Skip> {
    let total = floor.bytes.saturating_add(already).saturating_add(need.bytes);
    if fs.free >= total {
        return Ok(());
    }
    let mount = fs.mount.display();
    let reason = if fs.free < floor.bytes {
        let head = format!("only {} free on {mount} (floor {}, {})", gb(fs.free), gb(floor.bytes), floor.key);
        if need.bytes == 0 && already == 0 {
            format!("{head}: {} short of the floor before any copy", gb(floor.bytes - fs.free))
        } else {
            let others = if already > 0 { format!(" + ~{} for other copies under way", gb(already)) } else { String::new() };
            format!("{head}: this copy ({}){others} + the floor = {} needed", need.words, gb(total))
        }
    } else {
        let others = if already > 0 { format!(" after ~{} for other copies under way", gb(already)) } else { String::new() };
        format!(
            "{} free on {mount}, and this copy ({}){others} would leave less than the floor ({}, {})",
            gb(fs.free),
            need.words,
            gb(floor.bytes),
            floor.key
        )
    };
    Err(Skip { reason, advice: format!("free space on {mount} (`arena config check` names arena's biggest dirs there)"), stopped: false })
}

/// One run's copies onto this box, judged together: each filesystem read once, and what every
/// copy admitted may take held against it — with what other runs' copies under way hold
/// ([`Reservations`]) — so ten first copies can't each see the same free space (pure but for the
/// readings handed in).
#[derive(Debug, Clone)]
pub struct Ledger {
    floor: Floor,
    /// What other runs' copies under way may still take, per device.
    others: BTreeMap<u64, u64>,
    /// Per device: its reading (the first one) and what this run's admitted copies may take.
    held: BTreeMap<u64, (Fs, u64)>,
}

impl Ledger {
    pub fn new(floor: Floor) -> Ledger {
        Ledger::with_others(floor, BTreeMap::new())
    }

    /// A ledger that counts what other runs hold per device ([`Reservations::by_dev`]) as taken.
    pub fn with_others(floor: Floor, others: BTreeMap<u64, u64>) -> Ledger {
        Ledger { floor, others, held: BTreeMap::new() }
    }

    /// What this run's admitted copies may take, per device (what it reserves).
    pub fn taken(&self) -> BTreeMap<u64, u64> {
        self.held.iter().map(|(dev, (_, b))| (*dev, *b)).filter(|(_, b)| *b > 0).collect()
    }

    /// Admit one copy — its parts (`(reading of where it writes, size)`: a pod's tiers) all, or
    /// none: a pod is never half copied because one tier fits and the other doesn't. A filesystem
    /// already read this run keeps its first reading.
    pub fn admit(&mut self, parts: &[(Fs, Size)]) -> Result<(), Skip> {
        let mut by_dev: BTreeMap<u64, (Fs, Vec<Size>)> = BTreeMap::new();
        for (fs, size) in parts {
            by_dev.entry(fs.dev).or_insert_with(|| (fs.clone(), Vec::new())).1.push(*size);
        }
        let mut take = Vec::new();
        for (dev, (fs, sizes)) in by_dev {
            let (fs, mine) = self.held.get(&dev).cloned().unwrap_or((fs, 0));
            let others = self.others.get(&dev).copied().unwrap_or(0);
            let need = Need::of(&sizes);
            judge(&fs, mine.saturating_add(others), &need, self.floor)?;
            take.push((dev, fs, mine.saturating_add(need.bytes)));
        }
        for (dev, fs, held) in take {
            self.held.insert(dev, (fs, held));
        }
        Ok(())
    }
}

/// How close a filesystem is to the floor: under it, every copy onto it is skipped; under twice
/// it, the next big one may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Near,
    Below,
}

/// [`Level`] of `free` bytes against `floor` (pure). A zero floor is never near.
pub fn level(free: u64, floor: u64) -> Level {
    if free < floor {
        Level::Below
    } else if free < floor.saturating_mul(2) {
        Level::Near
    } else {
        Level::Ok
    }
}

/// `3.1 GB` (one decimal, whole GB plain: `5 GB`, `0 GB`), `34 MB`, `12 KB` — sizes as the floor's
/// unit counts them.
pub fn gb(b: u64) -> String {
    if b.is_multiple_of(GB) {
        format!("{} GB", b / GB)
    } else if b >= GB / 10 {
        format!("{:.1} GB", b as f64 / GB as f64)
    } else if b >= 1_000_000 {
        format!("{} MB", b / 1_000_000)
    } else {
        format!("{} KB", b.div_ceil(1000))
    }
}

/// `40 s`, `12 min`, `5 h`, `16 days` — how long ago, in its biggest unit.
pub fn age_words(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 2 * 86_400 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86_400),
    }
}

// ---------------------------------------------------------------------------------------
// Readings
// ---------------------------------------------------------------------------------------

/// Read the filesystem `path` is on — or will be, once created: the nearest existing ancestor's
/// (a relative path against the working directory). `Err` when not even that can be read.
#[cfg(unix)]
pub fn reading(path: &Path) -> Result<Fs, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    let mut at: &Path = &abs;
    let meta = loop {
        match std::fs::metadata(at) {
            Ok(m) => break m,
            Err(e) => match at.parent() {
                Some(p) => at = p,
                None => return Err(e.to_string()),
            },
        }
    };
    let c = std::ffi::CString::new(at.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: statvfs(3) on a NUL-terminated path into a zeroed struct we own.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let frsize = st.f_frsize as u64;
    let dev = meta.dev();
    // The mount point: the highest ancestor still on this device.
    let mut mount = std::fs::canonicalize(at).unwrap_or_else(|_| at.to_path_buf());
    while let Some(parent) = mount.parent() {
        if std::fs::metadata(parent).ok().map(|m| m.dev()) != Some(dev) {
            break;
        }
        mount = parent.to_path_buf();
    }
    Ok(Fs { dev, mount, free: (st.f_bavail as u64).saturating_mul(frsize), total: (st.f_blocks as u64).saturating_mul(frsize) })
}

/// No `statvfs` off unix: unknown, which every copy reads as "not copying blind".
#[cfg(not(unix))]
pub fn reading(_path: &Path) -> Result<Fs, String> {
    Err("free space isn't readable on this platform".into())
}

// ---------------------------------------------------------------------------------------
// What arena leaves on the box
// ---------------------------------------------------------------------------------------

/// The name replace/migrate's via-local staging of pod `<id>` gets under the temp dir.
pub const STAGING_PREFIX: &str = "arena-replace-";

/// The via-local copy's staging dir for source pod `src_id` under `root` (`$TMPDIR`).
pub fn staging_dir(root: &Path, src_id: &str) -> PathBuf {
    root.join(format!("{STAGING_PREFIX}{src_id}"))
}

/// Take `dir`'s lock (an exclusive `flock` on the directory, held while the returned file
/// lives): replace/migrate hold their staging dir's for the whole copy, and a cleanup holds it
/// while it removes one, so a second run of the same pod can't write into it, nothing removes
/// it under a run, and [`find_leftovers`] can tell one in use from one left behind. `Err` with
/// `WouldBlock` when another run holds it; with `NotFound` when the dir is gone — or another
/// one stands at that path — by the time the lock is taken (a cleanup removed it meanwhile): the
/// lock held must be the dir's at that path, so create it and lock it again.
#[cfg(unix)]
pub fn lock_dir(dir: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let f = std::fs::File::open(dir)?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, format!("{} is in use by another arena run", dir.display())))
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let (held, now) = (f.metadata()?, std::fs::metadata(dir));
    match now {
        Ok(m) if (m.dev(), m.ino()) == (held.dev(), held.ino()) => Ok(f),
        _ => Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} was removed while being locked", dir.display()))),
    }
}

#[cfg(not(unix))]
pub fn lock_dir(dir: &Path) -> std::io::Result<std::fs::File> {
    let f = std::fs::File::open(dir)?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, format!("{} is in use by another arena run", dir.display())))
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Whether another run holds `dir`'s lock ([`lock_dir`]) right now. One that can't be opened
/// or locked here isn't.
pub fn in_use(dir: &Path) -> bool {
    matches!(lock_dir(dir), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// What a walk over a tree found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeUse {
    /// On disk (allocated blocks), a hard-linked file counted once per walk.
    pub bytes: u64,
    /// The walk stopped at its entry cap: `bytes` is a lower bound.
    pub capped: bool,
    /// When anything in it last changed on this box (the newest ctime, unix secs) — not its
    /// mtimes, which rsync copies from the pod.
    pub changed: Option<u64>,
}

/// How many entries one look at the box walks at most ([`biggest`], [`find_leftovers`]).
pub const WALK_CAP: u64 = 2_000_000;

/// Walk `root` (not following links, not leaving its filesystem), up to `*budget` entries,
/// counting each hard-linked inode once across every walk sharing `seen`.
#[cfg(unix)]
pub fn tree_use(root: &Path, budget: &mut u64, seen: &mut HashSet<(u64, u64)>) -> TreeUse {
    use std::os::unix::fs::MetadataExt;
    let mut t = TreeUse::default();
    let Ok(top) = std::fs::symlink_metadata(root) else { return t };
    let dev = top.dev();
    let mut add = |m: &std::fs::Metadata, t: &mut TreeUse| {
        if m.nlink() <= 1 || seen.insert((m.dev(), m.ino())) {
            t.bytes = t.bytes.saturating_add(m.blocks().saturating_mul(512));
        }
        let c = u64::try_from(m.ctime()).unwrap_or(0);
        t.changed = Some(t.changed.map_or(c, |x| x.max(c)));
    };
    add(&top, &mut t);
    if !top.is_dir() {
        return t;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if *budget == 0 {
                t.capped = true;
                return t;
            }
            *budget -= 1;
            let Ok(m) = e.metadata() else { continue };
            add(&m, &mut t);
            if m.is_dir() && m.dev() == dev {
                stack.push(e.path());
            }
        }
    }
    t
}

#[cfg(not(unix))]
pub fn tree_use(_root: &Path, _budget: &mut u64, _seen: &mut HashSet<(u64, u64)>) -> TreeUse {
    TreeUse::default()
}

/// The biggest of `dirs` on device `dev` (others skipped), at most `limit`, biggest first — as
/// `du` counts them in one walk (a file hard-linked between snapshots counted once, in the
/// first dir walked), within [`WALK_CAP`] entries in all.
#[cfg(unix)]
pub fn biggest(dirs: &[PathBuf], dev: u64, limit: usize) -> Vec<(PathBuf, TreeUse)> {
    use std::os::unix::fs::MetadataExt;
    let mut budget = WALK_CAP;
    let mut seen = HashSet::new();
    let mut out: Vec<(PathBuf, TreeUse)> = dirs
        .iter()
        .filter(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.dev() == dev))
        .map(|d| (d.clone(), tree_use(d, &mut budget, &mut seen)))
        .filter(|(_, t)| t.bytes > 0)
        .collect();
    out.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    out
}

#[cfg(not(unix))]
pub fn biggest(_dirs: &[PathBuf], _dev: u64, _limit: usize) -> Vec<(PathBuf, TreeUse)> {
    Vec::new()
}

/// What made a [`Leftover`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeftoverKind {
    /// replace/migrate's via-local staging of pod `source` (its id): its home, kept after a copy
    /// that failed (so a re-run continues from it) or was stopped, or when a later attempt copied
    /// directly — or by a master build from before 2026-10-19 (#3: the one prod ran on 10-14),
    /// which never removed it.
    Staging { source: String },
    /// An interrupted `pods baseline` capture (its partial dir, by process `pid`).
    Baseline { pid: u32 },
    /// rsync temp files a pull stopped hard (SIGKILL, a crash) left in a tier's `.rsync-tmp/`.
    RsyncTemp,
}

/// Something arena left on this box, found by [`find_leftovers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leftover {
    pub path: PathBuf,
    #[serde(flatten)]
    pub kind: LeftoverKind,
    pub bytes: u64,
    /// The size is a lower bound (the walk stopped at its cap).
    pub capped: bool,
    /// When it last changed here (unix secs).
    pub changed: Option<u64>,
    /// A run is using it right now (its lock is held, its process alive).
    pub in_use: bool,
}

/// rsync temp files unchanged this long are a stopped pull's, not a running one's (a pull's
/// budget is at most a day, and a running one writes to them).
pub const RSYNC_TEMP_STALE_SECS: u64 = 86_400;

/// What [`find_leftovers`] found, and where it couldn't look (a dir it couldn't list, other than
/// one that isn't there): with any of those, "nothing found" isn't "nothing there".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub leftovers: Vec<Leftover>,
    pub unread: Vec<String>,
}

/// Look for what arena leaves on this box (read-only; nothing is ever removed here):
/// `<tmp>/arena-replace-*` (replace/migrate's staging), and in each backup root `<root>/baseline/
/// w*/.fleet.partial-<pid>` (an interrupted `pods baseline`) and `<root>/.rsync-tmp`,
/// `<root>/*/.rsync-tmp` holding files unchanged for [`RSYNC_TEMP_STALE_SECS`] (a pull stopped
/// hard). Sorted by path.
pub fn find_leftovers(tmp: &Path, backup_roots: &[PathBuf], now: u64) -> Found {
    let mut budget = WALK_CAP;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let unread = std::cell::RefCell::new(Vec::new());
    // A dir's subdirs; one that isn't there has none, one that can't be listed is said.
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        match std::fs::read_dir(dir) {
            Ok(es) => es.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                unread.borrow_mut().push(format!("{}: {e}", dir.display()));
                Vec::new()
            }
        }
    };
    for path in subdirs(tmp) {
        let Some(source) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(STAGING_PREFIX)) else { continue };
        if source.is_empty() {
            continue;
        }
        let t = tree_use(&path, &mut budget, &mut seen);
        let busy = in_use(&path);
        out.push(Leftover { kind: LeftoverKind::Staging { source: source.to_string() }, bytes: t.bytes, capped: t.capped, changed: t.changed, in_use: busy, path });
    }
    let mut roots: Vec<&PathBuf> = backup_roots.iter().collect();
    roots.sort();
    roots.dedup();
    for root in roots {
        for week in subdirs(&root.join(crate::pull::BASELINE_DIR)) {
            for path in subdirs(&week) {
                let Some(pid) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(".fleet.partial-")).and_then(|p| p.parse::<u32>().ok())
                else {
                    continue;
                };
                let t = tree_use(&path, &mut budget, &mut seen);
                let alive = Path::new(&format!("/proc/{pid}")).exists();
                out.push(Leftover { kind: LeftoverKind::Baseline { pid }, bytes: t.bytes, capped: t.capped, changed: t.changed, in_use: alive, path });
            }
        }
        let temps = std::iter::once(root.join(crate::pull::TEMP_DIR)).chain(subdirs(root).into_iter().map(|d| d.join(crate::pull::TEMP_DIR)));
        for path in temps {
            let holds = match std::fs::read_dir(&path) {
                Ok(mut es) => es.next().is_some(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    unread.borrow_mut().push(format!("{}: {e}", path.display()));
                    false
                }
            };
            if !holds {
                continue;
            }
            let t = tree_use(&path, &mut budget, &mut seen);
            if t.changed.is_some_and(|c| now.saturating_sub(c) >= RSYNC_TEMP_STALE_SECS) {
                out.push(Leftover { kind: LeftoverKind::RsyncTemp, bytes: t.bytes, capped: t.capped, changed: t.changed, in_use: false, path });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Found { leftovers: out, unread: unread.into_inner() }
}

/// A word for a shell line: single-quoted unless it's plain (and not an option).
fn sh_word(s: &str) -> String {
    if !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+@:,=".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// A path for a shell line ([`sh_word`]).
fn sh(p: &Path) -> String {
    sh_word(&p.display().to_string())
}

impl Leftover {
    /// `3.5 GB, last written 16 days ago` (pure).
    pub fn facts(&self, now: u64) -> String {
        let size = if self.capped { format!("over {}", gb(self.bytes)) } else { gb(self.bytes) };
        match self.changed {
            Some(c) => format!("{size}, last written {} ago", age_words(now.saturating_sub(c))),
            None => size,
        }
    }

    /// What it is, in words (pure).
    pub fn what(&self) -> String {
        match &self.kind {
            LeftoverKind::Staging { source } => format!(
                "pod {source}'s home, staged by `pods replace`/`migrate` and kept after a copy that failed or was \
                 stopped, or by a later attempt that copied directly (or by a release from before 2026-10-19, which \
                 never removed it)"
            ),
            LeftoverKind::Baseline { pid } => format!("an interrupted `pods baseline` capture (process {pid})"),
            LeftoverKind::RsyncTemp => "rsync temp files a pull stopped hard left behind".into(),
        }
    }

    /// How to look at it, then remove it — printed, never run (pure).
    pub fn commands(&self) -> (String, String) {
        let p = sh(&self.path);
        (format!("du -sh {p}; ls -la {p}"), format!("rm -rf {p}"))
    }

    /// What to check before removing it (pure): a staging copy can be the only copy of a pod's
    /// work, if the copy failed and that pod is gone since.
    pub fn caution(&self) -> Option<String> {
        match &self.kind {
            LeftoverKind::Staging { source } => Some(format!(
                "it may be the only copy of that pod's work: first check that {source} still exists (`arena pods list \
                 --no-probe | grep -F -- {}`) or that its work is on the pod that replaced it, or in LOCAL_BACKUP_DIR",
                sh_word(source)
            )),
            _ => None,
        }
    }

    /// The rows `config check` and the health check print for it (pure).
    pub fn lines(&self, now: u64) -> Vec<String> {
        if self.in_use {
            return vec![format!("◷ {} — {}; in use now by a running arena", self.path.display(), self.facts(now))];
        }
        let (look, remove) = self.commands();
        let mut out = vec![format!("⚠ {} — {}: {}", self.path.display(), self.facts(now), self.what())];
        out.extend(self.caution().map(|c| format!("  └ {c}")));
        out.push(format!("  └ look: {look}"));
        out.push(format!("  └ remove: {remove}"));
        out
    }
}

// ---------------------------------------------------------------------------------------
// Which pods have a whole copy here (what makes a copy a "later" one)
// ---------------------------------------------------------------------------------------

/// The hidden dir at a backup's snapshot root that marks the pods copied whole into it (every
/// tier and pass of one run went through): `<root>/.arena-copied/<pod>`. Beside the labels, like
/// `.arena-locks`: never a label, never inside a pod's tree (so never archived or restored).
pub const COPIED_DIR: &str = ".arena-copied";

/// Pod `pod`'s marker under the snapshot root `base`.
pub fn copied_marker(base: &Path, pod: &str) -> PathBuf {
    base.join(COPIED_DIR).join(pod)
}

/// Mark `pod` as copied whole into `base` (its marker's mtime: when, last).
pub fn mark_copied(base: &Path, pod: &str) -> std::io::Result<()> {
    let marker = copied_marker(base, pod);
    if let Some(dir) = marker.parent() {
        crate::snapshot::create_private_dir(dir)?;
    }
    std::fs::write(&marker, b"")
}

/// Whether `pod` was ever copied whole into `base`: only then is a copy onto what's there a later
/// one (an interrupted first copy leaves files, but most of the pod still to come).
pub fn was_copied(base: &Path, pod: &str) -> bool {
    copied_marker(base, pod).is_file()
}

/// Start marking whole copies in the backup at `base`, once, before a run's copies: a backup from
/// before arena marked them (no [`COPIED_DIR`] yet) gets every pod with a snapshot there marked —
/// as arena took them until then (files there: a later copy). Without that, the upgrade would turn
/// every pod's next copy into a first one (~6 GB a pod over both tiers), and a box with room for
/// the fleet's changes but not for that would skip them all. Made aside and renamed into place,
/// so a run stopped halfway marks none; a backup with no pods yet gets an empty one (from then on,
/// only whole copies mark). `Ok(n)`: how many pods it marked (0: marking had started already, or
/// there's no backup at `base`).
pub fn seed_copied(base: &Path) -> std::io::Result<usize> {
    let dir = base.join(COPIED_DIR);
    if dir.exists() || !base.is_dir() {
        return Ok(0);
    }
    let mut pods = std::collections::BTreeSet::new();
    for label in std::fs::read_dir(base)?.flatten() {
        if label.file_name().to_str().and_then(crate::restore::parse_label).is_none() {
            continue;
        }
        for pod in std::fs::read_dir(label.path()).into_iter().flatten().flatten() {
            let name = pod.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && pod.file_type().is_ok_and(|t| t.is_dir()) {
                pods.insert(name);
            }
        }
    }
    let aside = base.join(format!("{COPIED_DIR}.new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&aside);
    crate::snapshot::create_private_dir(&aside)?;
    let made = pods.iter().try_for_each(|p| std::fs::write(aside.join(p), b"")).and_then(|()| std::fs::rename(&aside, &dir));
    match made {
        Ok(()) => Ok(pods.len()),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&aside);
            // Another run started marking meanwhile: theirs stands.
            if dir.exists() {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// What other runs' copies may still take (one box, several arena runs at once)
// ---------------------------------------------------------------------------------------

/// The reservations file in the fleet's state dir: what each arena run's admitted copies onto this
/// box may still write, per device, until they end — so a cron backup and an operator's pull at
/// the same time can't each count the same free space.
pub const RESERVATIONS_FILE: &str = "disk-reservations.json";
const RESERVATIONS_VERSION: u32 = 1;
/// A reservation this old is a run's that's long over (a pod's copy may take a day at most).
pub const RESERVATION_MAX_AGE_SECS: u64 = 26 * 3600;

/// One run's copies on one device: who holds it (`pid`, and `token` for that admission), how
/// much, since when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub pid: u32,
    pub token: u64,
    pub dev: u64,
    pub bytes: u64,
    pub at: u64,
}

/// The reservations ([`RESERVATIONS_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservations {
    pub version: u32,
    pub held: Vec<Reservation>,
}

impl Default for Reservations {
    fn default() -> Self {
        Reservations { version: RESERVATIONS_VERSION, held: Vec::new() }
    }
}

impl Reservations {
    /// Load: missing = none; one that can't be parsed, or another version's = none plus a warning
    /// (arena writes it atomically, so that's another version's: rewritten with this run's alone).
    /// `Err`: it can't be read at all — what other runs hold is unknown.
    pub fn load(path: &Path) -> Result<(Reservations, Option<String>), String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(match serde_json::from_str::<Reservations>(&text) {
                Ok(r) if r.version == RESERVATIONS_VERSION => (r, None),
                Ok(r) => (Reservations::default(), Some(format!("warning: replacing {} (version {})", path.display(), r.version))),
                Err(e) => (Reservations::default(), Some(format!("warning: replacing {} ({e})", path.display()))),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Reservations::default(), None)),
            Err(e) => Err(format!("can't read {} ({e})", path.display())),
        }
    }

    /// Drop what no run holds any more: its process gone (`alive`), or older than
    /// [`RESERVATION_MAX_AGE_SECS`] at `now` (pure but for `alive`).
    pub fn prune(&mut self, now: u64, alive: impl Fn(u32) -> bool) {
        self.held.retain(|r| now.saturating_sub(r.at) < RESERVATION_MAX_AGE_SECS && alive(r.pid));
    }

    /// What they hold, per device (pure).
    pub fn by_dev(&self) -> BTreeMap<u64, u64> {
        let mut out = BTreeMap::new();
        for r in &self.held {
            let b: &mut u64 = out.entry(r.dev).or_default();
            *b = b.saturating_add(r.bytes);
        }
        out
    }

    /// Hold `taken` (per device) for admission `(pid, token)` from `now` (pure).
    pub fn hold(&mut self, pid: u32, token: u64, taken: &BTreeMap<u64, u64>, now: u64) {
        self.held.extend(taken.iter().map(|(dev, bytes)| Reservation { pid, token, dev: *dev, bytes: *bytes, at: now }));
    }

    /// Let admission `(pid, token)`'s go (pure).
    pub fn release(&mut self, pid: u32, token: u64) {
        self.held.retain(|r| (r.pid, r.token) != (pid, token));
    }

    /// Save atomically, readable by all (pids, devices and sizes, nothing secret: another user's
    /// arena sharing the state root must read it to count these, or it copies nothing).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(self).unwrap_or_default();
        json.push('\n');
        crate::snapshot::write_atomic(path, json.as_bytes(), 0o644)
    }
}

/// Whether process `pid` is an arena run still (its command line names arena): a reservation's
/// holder that isn't — gone, or the pid reused — holds nothing. Where `/proc` can't say (none
/// here, or not readable): yes, and the reservation goes with its age.
pub fn arena_alive(pid: u32) -> bool {
    if !Path::new("/proc/self").exists() {
        return true;
    }
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmd) => cmd.windows(5).any(|w| w == b"arena"),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

// ---------------------------------------------------------------------------------------
// The record of skips (for the daily health check)
// ---------------------------------------------------------------------------------------

/// The skip record's file in the fleet's state dir (`crate::snapshot::state_file_path_for`).
pub const SKIPS_FILE: &str = "disk-skips.json";
const SKIPS_VERSION: u32 = 1;
/// A record whose last skip is this old is dropped at the next write.
pub const FORGET_SKIPS_AFTER_SECS: u64 = 7 * 86_400;
/// A skip after a gap this long starts a new run of skips (count and first time reset).
pub const SKIP_WINDOW_SECS: u64 = 86_400;
/// How long [`record_skips`] waits for the state dir's lock (as the BUSY record does).
pub const RECORD_LOCK_WAIT: Duration = Duration::from_secs(10);

/// One pod's copies of one kind skipped for space: since when, how often, the last reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipEntry {
    pub pod: String,
    /// `copy` (pull, backup, fallback and BUSY copies), `staging`, `reap`, `baseline`.
    pub what: String,
    pub first: u64,
    pub last: u64,
    pub count: u64,
    /// The last skip's reason.
    pub why: String,
}

/// The record ([`SKIPS_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skips {
    pub version: u32,
    pub entries: Vec<SkipEntry>,
}

impl Default for Skips {
    fn default() -> Self {
        Skips { version: SKIPS_VERSION, entries: Vec::new() }
    }
}

impl Skips {
    /// Load tolerantly: missing = none; unreadable or another version = none plus a warning.
    pub fn load(path: &Path) -> (Skips, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Skips>(&text) {
                Ok(s) if s.version == SKIPS_VERSION => (s, None),
                Ok(s) => (Skips::default(), Some(format!("warning: ignoring {} (version {})", path.display(), s.version))),
                Err(e) => (Skips::default(), Some(format!("warning: ignoring {} ({e})", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Skips::default(), None),
            Err(e) => (Skips::default(), Some(format!("warning: can't read {} ({e})", path.display()))),
        }
    }

    /// Add `new` skips (`(pod, what, why)`) at `now`, dropping records [`FORGET_SKIPS_AFTER_SECS`]
    /// old (pure).
    pub fn add(&mut self, new: &[(String, String, String)], now: u64) {
        self.entries.retain(|e| now.saturating_sub(e.last) < FORGET_SKIPS_AFTER_SECS);
        for (pod, what, why) in new {
            match self.entries.iter_mut().find(|e| e.pod == *pod && e.what == *what) {
                Some(e) => {
                    if now.saturating_sub(e.last) >= SKIP_WINDOW_SECS {
                        e.first = now;
                        e.count = 0;
                    }
                    e.last = now;
                    e.count += 1;
                    e.why = why.clone();
                }
                None => self.entries.push(SkipEntry { pod: pod.clone(), what: what.clone(), first: now, last: now, count: 1, why: why.clone() }),
            }
        }
        self.entries.sort_by(|a, b| (&a.pod, &a.what).cmp(&(&b.pod, &b.what)));
    }

    /// The records with a skip in the [`SKIP_WINDOW_SECS`] before `now`.
    pub fn recent(&self, now: u64) -> Vec<&SkipEntry> {
        self.entries.iter().filter(|e| now.saturating_sub(e.last) < SKIP_WINDOW_SECS).collect()
    }
}

/// `14:02 UTC` (`t` on `now`'s day), else `2026-10-18 14:02 UTC`.
fn when(t: u64, now: u64) -> String {
    let hm = format!("{:02}:{:02} UTC", t % 86_400 / 3600, t % 3600 / 60);
    if t / 86_400 == now / 86_400 {
        hm
    } else {
        format!("{} {hm}", crate::schedule::ymd_string((t / 86_400) as i64))
    }
}

impl SkipEntry {
    /// `✗ alpha: copy skipped 12× (since 09:00 UTC, last 10:15 UTC) — only 3.1 GB free on / (floor 5 GB, …)`.
    /// One last stopped while it wrote says so, and that it's tried again: each run starts it anew
    /// — rsync keeps what it copied, a file it was writing starts over — and the watch stops it
    /// again at the floor, until there's room (review #42: left so, rather than remembering what
    /// it needed; this line, failing the health check, is how the operator hears of it).
    pub fn line(&self, now: u64) -> String {
        let times = if self.count == 1 {
            format!("at {}", when(self.last, now))
        } else {
            format!("{}× (since {}, last {})", self.count, when(self.first, now), when(self.last, now))
        };
        if self.why.contains(WHILE_COPYING) {
            let verb = if self.count == 1 { "stopped" } else { "skipped or stopped" };
            return format!(
                "✗ {}: {} {verb} {times} — {}; each run tries it again (from where it stopped) until there's room",
                self.pod, self.what, self.why
            );
        }
        format!("✗ {}: {} skipped {times} — {}", self.pod, self.what, self.why)
    }
}

/// Create the state dir holding `file` (owner-only) and take its lock
/// ([`crate::snapshot::lock_dir_within`], tried for up to `wait`): what the skip record and the
/// reservations are rewritten under.
pub fn lock_state_for(file: &Path, wait: Duration) -> std::io::Result<Option<std::fs::File>> {
    let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir)?;
    crate::snapshot::lock_dir_within(dir, wait)
}

/// Add `new` skips (`(pod, what, why)`) to the record at `path`, under the state dir's lock
/// (tried for up to `wait`), saved atomically (owner-only). `Ok` carries the load warning, if
/// the old file had to be ignored.
pub fn record_skips(path: &Path, new: &[(String, String, String)], now: u64, wait: Duration) -> std::io::Result<Option<String>> {
    let lock = lock_state_for(path, wait)?;
    let (mut skips, warning) = Skips::load(path);
    skips.add(new, now);
    let mut json = serde_json::to_string_pretty(&skips).unwrap_or_default();
    json.push('\n');
    let saved = crate::snapshot::write_atomic(path, json.as_bytes(), 0o600);
    drop(lock);
    saved.map(|()| warning)
}

#[cfg(test)]
mod tests;
