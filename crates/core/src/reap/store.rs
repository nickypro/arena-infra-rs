//! The reaper's files in `$ARENA_STATE_DIR/<prefix>/` (spec §5.1): `reap.json` (0600,
//! written atomically and fsynced, loaded STRICTLY), its short lock `reap.json.lock` (held
//! only around a load–modify–save), the run lock `reap.lock` (one run at a time) and the log
//! `reap-log.jsonl` (+ `.1`).
//!
//! Strict means: a missing file is empty state (nothing is authorised); a corrupt, unreadable
//! or other-version file is an error, never read as empty and never overwritten — its bytes
//! stay exactly as they were (spec I9).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::judge::WriteAheadProof;
use super::{ReapState, Terminating, Trigger};

pub const STATE_FILE: &str = "reap.json";
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
            StoreError::Corrupt { path, error } => {
                write!(f, "{} isn't a reap state file this arena can read ({error}) — left untouched", path.display())
            }
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

    /// Load strictly (see the module doc).
    pub fn load(&self) -> Result<ReapState, StoreError> {
        let path = self.state_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => ReapState::parse(&text).map_err(|error| StoreError::Corrupt { path, error }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReapState::default()),
            Err(e) => Err(StoreError::Unreadable { owner: owner_of(&path), path, error: e.to_string() }),
        }
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
        File::open(&self.dir).and_then(|d| d.sync_all()).map_err(werr)
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
        p.terminating = Some(Terminating { run_id: run_id.to_string(), trigger, at: now });
        s.attempts.push(now);
        Ok(())
    })?;
    Ok(WriteAheadProof::saved(key, now))
}
