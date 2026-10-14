//! The reaper's files in `$ARENA_STATE_DIR/<prefix>/` (spec §5.1): `reap.json` (0600,
//! written atomically and fsynced, loaded STRICTLY), the hold in its own `hold.json` (so a
//! hold can be set or released whatever state `reap.json` is in — RR13), their short lock
//! `reap.json.lock` (held only around a load–modify–save), the run lock `reap.lock` (one run
//! at a time) and the log `reap-log.jsonl` (+ `.1`).
//!
//! Strict means: a missing file is empty state (nothing is authorised); a corrupt, unreadable
//! or other-version file is an error, never read as empty and never overwritten — its bytes
//! stay exactly as they were (spec I9).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::judge::{SentMark, WriteAheadProof};
use super::{FrozenDirs, ReapState, Terminating, Trigger};

pub const STATE_FILE: &str = "reap.json";
pub const HOLD_FILE: &str = "hold.json";
pub const STATE_LOCK: &str = "reap.json.lock";
pub const RUN_LOCK: &str = "reap.lock";
pub const LOG_FILE: &str = "reap-log.jsonl";

/// How long a writer waits for the state lock (it's only ever held for a load–modify–save).
pub const STATE_LOCK_WAIT: Duration = Duration::from_secs(30);

/// How long a lock that is "busy" is tried again before it's called busy (the run lock here,
/// the per-pod backup locks in the CLI). A lock just released can still be held for a moment
/// by a child process forked (by another thread) while it was held, until that child's
/// `exec` closes its inherited copy — without this grace, that moment reads as "another run
/// is in progress" (a skipped tick) or "another backup holds the pod" (a skipped look).
pub const LOCK_GRACE: Duration = Duration::from_secs(2);

/// Why the state couldn't be used.
#[derive(Debug)]
pub enum StoreError {
    /// The file is there but can't be read (permissions, I/O) — `owner` = its uid, when known.
    Unreadable { path: PathBuf, error: String, owner: Option<u32> },
    /// The file isn't a version-1 state file.
    Corrupt { path: PathBuf, error: String },
    /// The lock couldn't be taken (or the directory created).
    Lock { path: PathBuf, error: String },
    /// Writing failed (disk full, permissions, wrong owner).
    Write { path: PathBuf, error: String, owner: Option<u32> },
    /// The change itself was refused.
    Refused(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let owner = |o: &Option<u32>| o.map(|u| format!(" (the file is owned by uid {u})")).unwrap_or_default();
        match self {
            StoreError::Unreadable { path, error, owner: o } => write!(f, "can't read {}: {error}{}", path.display(), owner(o)),
            StoreError::Corrupt { path, error } => write!(
                f,
                "{p} isn't a reap state file this arena can read ({error}) — left untouched. Nothing is reaped while it \
                 stays; `arena pods reap hold`/`release` still work. To start over: `mv {p} {p}.bad-$(date -u \
                 +%Y%m%dT%H%M%SZ)` — the reaper then has no policies (nothing is reaped until `reap set` again; \
                 `arena pods reap log` shows what was set)",
                p = path.display()
            ),
            StoreError::Lock { path, error } => write!(f, "can't lock {}: {error}", path.display()),
            StoreError::Write { path, error, owner: o } => write!(f, "can't write {}: {error}{}", path.display(), owner(o)),
            StoreError::Refused(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The state directory (`$ARENA_STATE_DIR/<prefix>/`) and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub dir: PathBuf,
}

#[cfg(unix)]
fn owner_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn owner_of(_path: &Path) -> Option<u32> {
    None
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    pub fn hold_path(&self) -> PathBuf {
        self.dir.join(HOLD_FILE)
    }

    /// Load strictly (see the module doc), the hold from its own file.
    pub fn load(&self) -> Result<ReapState, StoreError> {
        let path = self.state_path();
        let mut state = match std::fs::read_to_string(&path) {
            Ok(text) => ReapState::parse(&text).map_err(|error| StoreError::Corrupt { path, error })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReapState::default(),
            Err(e) => return Err(StoreError::Unreadable { owner: owner_of(&path), path, error: e.to_string() }),
        };
        state.hold = self.read_hold()?.or_else(|| state.legacy_hold.take());
        Ok(state)
    }

    /// The hold (`hold.json`), strictly: none without the file; one that can't be read is an
    /// error (a run then doesn't act), never "no hold".
    pub fn read_hold(&self) -> Result<Option<super::Hold>, StoreError> {
        let path = self.hold_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => super::Hold::parse_file(&text).map(Some).map_err(|error| StoreError::Corrupt { path, error }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Unreadable { owner: owner_of(&path), path, error: e.to_string() }),
        }
    }

    /// Write (`Some`) or remove (`None`) the hold file, atomically and fsynced — under the state
    /// lock, never reading `reap.json` (RR13: a corrupt one doesn't stop `reap hold`).
    pub fn set_hold(&self, hold: Option<&super::Hold>) -> Result<(), StoreError> {
        let _lock = self.lock_state()?;
        self.write_hold(hold)
    }

    /// [`Store::set_hold`] only while the hold is still `seen` (what an operator was shown
    /// before confirming a release): `Ok(false)` = it changed meanwhile, left as it is.
    pub fn clear_hold_if(&self, seen: &super::Hold) -> Result<bool, StoreError> {
        let _lock = self.lock_state()?;
        if self.read_hold().ok().flatten().as_ref() != Some(seen) {
            return Ok(false);
        }
        self.write_hold(None).map(|()| true)
    }

    fn write_hold(&self, hold: Option<&super::Hold>) -> Result<(), StoreError> {
        let path = self.hold_path();
        let werr = |e: std::io::Error| StoreError::Write { owner: owner_of(&path), path: path.clone(), error: e.to_string() };
        crate::snapshot::create_private_dir(&self.dir).map_err(werr)?;
        match hold {
            Some(h) => crate::snapshot::write_atomic(&path, h.to_file().as_bytes(), 0o600).map_err(werr)?,
            None => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(werr(e)),
            },
        }
        File::open(&self.dir).and_then(|d| d.sync_all()).map_err(werr)
    }

    /// Load for display only (`pods list`'s ENDS column): `Ok(None)` when there is no file.
    pub fn load_if_any(&self) -> Result<Option<ReapState>, StoreError> {
        if !self.state_path().exists() {
            return Ok(None);
        }
        self.load().map(Some)
    }

    /// Save atomically (temp + fsync + rename) as 0600, then fsync the directory so the rename
    /// itself is durable — what a write-ahead record needs.
    pub fn save(&self, state: &ReapState) -> Result<(), StoreError> {
        let path = self.state_path();
        let werr = |e: std::io::Error| StoreError::Write { owner: owner_of(&path), path: path.clone(), error: e.to_string() };
        crate::snapshot::create_private_dir(&self.dir).map_err(werr)?;
        crate::snapshot::write_atomic(&path, state.to_json().as_bytes(), 0o600).map_err(werr)?;
        File::open(&self.dir).and_then(|d| d.sync_all()).map_err(werr)?;
        // The hold, as the state now has it (it lives in its own file).
        if self.read_hold().ok().flatten() != state.hold {
            self.write_hold(state.hold.as_ref())?;
        }
        Ok(())
    }

    /// The state lock, held until the returned file is dropped (waits up to
    /// [`STATE_LOCK_WAIT`]).
    pub fn lock_state(&self) -> Result<File, StoreError> {
        let path = self.dir.join(STATE_LOCK);
        let lerr = |e: String| StoreError::Lock { path: path.clone(), error: e };
        crate::snapshot::create_private_dir(&self.dir).map_err(|e| lerr(e.to_string()))?;
        let f = open_lock(&path).map_err(|e| lerr(e.to_string()))?;
        let deadline = std::time::Instant::now() + STATE_LOCK_WAIT;
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(f),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(lerr(format!("held by another arena for {}s", STATE_LOCK_WAIT.as_secs())))
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(lerr(e.to_string())),
            }
        }
    }

    /// Load–modify–save under the state lock: `change` sees the strictly loaded state and may
    /// refuse (`Err` — nothing is saved). Every write goes through here, so concurrent writers
    /// (`reap set` during a run) never lose each other's records.
    pub fn update<T>(&self, change: impl FnOnce(&mut ReapState) -> Result<T, String>) -> Result<T, StoreError> {
        let _lock = self.lock_state()?;
        let mut state = self.load()?;
        let out = change(&mut state).map_err(StoreError::Refused)?;
        self.save(&state)?;
        Ok(out)
    }

    /// The run lock, without waiting (beyond [`LOCK_GRACE`]): `Ok(None)` = another run holds it.
    pub fn try_run_lock(&self) -> Result<Option<File>, StoreError> {
        let path = self.dir.join(super::store::RUN_LOCK);
        let lerr = |e: std::io::Error| StoreError::Lock { path: path.clone(), error: e.to_string() };
        crate::snapshot::create_private_dir(&self.dir).map_err(lerr)?;
        let f = open_lock(&path).map_err(lerr)?;
        let deadline = std::time::Instant::now() + LOCK_GRACE;
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(Some(f)),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(e)) => return Err(lerr(e)),
            }
        }
    }

    /// Append lines to the log (0600), fsynced; rotated to `.1` first once over
    /// [`super::LOG_ROTATE_BYTES`].
    pub fn append_log(&self, lines: &[String]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        crate::snapshot::create_private_dir(&self.dir)?;
        let path = self.log_path();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > super::LOG_ROTATE_BYTES) {
            std::fs::rename(&path, self.dir.join(format!("{LOG_FILE}.1")))?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        let mut buf = String::new();
        for l in lines {
            buf.push_str(l.trim_end_matches('\n'));
            buf.push('\n');
        }
        f.write_all(buf.as_bytes())?;
        f.sync_all()
    }

    /// The log's lines, oldest first (the rotated file's, then the current one's).
    pub fn read_log(&self) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        for p in [self.dir.join(format!("{LOG_FILE}.1")), self.log_path()] {
            match std::fs::read_to_string(&p) {
                Ok(t) => out.extend(t.lines().filter(|l| !l.trim().is_empty()).map(String::from)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// Open (creating, 0600) a lock file.
fn open_lock(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Write the write-ahead record of a reap attempt — `terminating` on the pod's policy and the
/// attempt counted for the daily cap — and save it durably. Only a successful save yields the
/// [`WriteAheadProof`] a ticket needs (spec I9: no action without a durable write-ahead).
/// Refused when the policy is gone or already has a pending terminate.
pub fn write_ahead(store: &Store, key: &str, run_id: &str, trigger: Trigger, now: u64) -> Result<WriteAheadProof, StoreError> {
    store.update(|s| {
        let p = s.pods.get_mut(key).ok_or_else(|| format!("{key}: no policy any more"))?;
        if p.terminating.is_some() {
            return Err(format!("{key}: a terminate is already pending"));
        }
        p.terminating = Some(Terminating::new(run_id, trigger, now));
        s.attempts.push(now);
        Ok(())
    })?;
    Ok(WriteAheadProof::saved(key, now))
}

/// The run's own pending write-ahead of `key`, to record a step on (refused when it isn't there
/// any more, or is another run's).
fn own_write_ahead<'a>(s: &'a mut ReapState, key: &str, run_id: &str) -> Result<&'a mut Terminating, String> {
    s.pods
        .get_mut(key)
        .and_then(|p| p.terminating.as_mut())
        .filter(|t| t.run_id == run_id)
        .ok_or_else(|| format!("{key}: this run's write-ahead isn't pending any more"))
}

/// Record on the attempt's write-ahead that its freeze completed — the frozen copy's dirs,
/// counted equal to what was verified (W7LR-1) — saved before anything else is done. Only a copy
/// recorded here may ever take a reap's name; a half-made one (a run killed mid-freeze) never is.
pub fn record_frozen(store: &Store, key: &str, run_id: &str, dirs: &FrozenDirs) -> Result<(), StoreError> {
    store.update(|s| {
        own_write_ahead(s, key, run_id)?.frozen = Some(dirs.clone());
        Ok(())
    })
}

/// Mark the attempt's write-ahead "terminate sent", durably, right before the call (W7LR-1): a
/// later run that finds the pod gone calls it reaped — its copy renamed to a reap's — only with
/// this mark. Refused (no mark, so no terminate) unless this run's write-ahead is pending with
/// its frozen copy recorded. Only a successful save yields the [`SentMark`] the terminate needs.
pub fn mark_sent(store: &Store, key: &str, run_id: &str, now: u64) -> Result<SentMark, StoreError> {
    store.update(|s| {
        let t = own_write_ahead(s, key, run_id)?;
        if t.frozen.is_none() {
            return Err(format!("{key}: no completed freeze recorded"));
        }
        t.sent_at = Some(now);
        Ok(())
    })?;
    Ok(SentMark::saved(key))
}
