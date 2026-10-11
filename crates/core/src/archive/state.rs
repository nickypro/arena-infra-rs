//! The archive's local state: `<state dir>/<prefix>/archive/` (mode 0700), resolved like the
//! health cache ([`crate::snapshot::state_dir`]) so root's cron and an interactive run that
//! share `ARENA_STATE_DIR` share it.
//!
//! | file | mode | written by |
//! |---|---|---|
//! | `admin_ed25519{,.pub}` | 0600/0644 | `up` (once): root on the archive, authorized by no pod |
//! | `push_ed25519{,.pub}` | 0600/0644 | `up`: the cron push, forced into rrsync (write-only) |
//! | `read_ed25519{,.pub}` | 0600/0644 | `up`: the per-cohort read key `copy-keys` puts on pods |
//! | `known_hosts` | 0600 | `up`: the archive's pinned host key |
//! | `archive.json` | 0600 | `up`, `access`, `down`, `resize` |
//! | `sync-snap.json` / `sync-big.json` | 0600 | each tier's push (only its own) |
//! | `sync-snap.lock` / `sync-big.lock` | 0600 | each tier's push (`flock`, non-blocking) |
//!
//! State files are written atomically (temp + rename, [`crate::snapshot::write_atomic`]). One
//! with an unknown `version` is an error, never silently reset: the sync state is what seals a
//! label, so losing it would quietly re-push (harmless) — but reading a future format as an old
//! one could seal a label that never got its final push.
//!
//! Losing this directory stops pushes and admin access (the keys are gone), but not the
//! participants' read access (their keys live on their laptops and pods). Back it up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::sync::PushFailure;
use super::Tier;
use crate::config::Config;

/// The state files' format version.
pub const STATE_VERSION: u32 = 1;

/// What `archive up` learned about the VM — everything `sync`, `ssh-config`, `copy-keys`,
/// `access` and `status` need without asking the API (the cron path never holds a token).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveHost {
    pub version: u32,
    pub prefix: String,
    /// `0` after `archive down` (server deleted, volumes kept): nothing to connect to.
    pub server_id: u64,
    /// The pinned address every push and admin call uses (empty when down).
    pub ipv4: String,
    pub location: String,
    pub snap_device: String,
    pub big_device: String,
    /// Whether participants may read (`archive access on|off`); `up` re-applies it. Off until
    /// the operator turns it on.
    pub access_on: bool,
    /// The `setup.sh` version last applied.
    pub setup_version: u32,
    /// Volume sizes as last seen (for the `resize` hint a full volume prints).
    #[serde(default)]
    pub snap_gb: u32,
    #[serde(default)]
    pub big_gb: u32,
    /// `SHA256:…` of the pinned host key, for the participants' "compare on first connect".
    #[serde(default)]
    pub host_key_fingerprint: Option<String>,
}

impl ArchiveHost {
    /// Whether there is a server to talk to.
    pub fn is_up(&self) -> bool {
        self.server_id != 0 && !self.ipv4.is_empty()
    }

    pub fn device(&self, tier: Tier) -> &str {
        match tier {
            Tier::Snap => &self.snap_device,
            Tier::Big => &self.big_device,
        }
    }

    pub fn size_gb(&self, tier: Tier) -> u32 {
        match tier {
            Tier::Snap => self.snap_gb,
            Tier::Big => self.big_gb,
        }
    }
}

/// One label's last successful push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRecord {
    /// When that push STARTED (UNIX s): a label is sealed by a push that started two hours
    /// after its day ended ([`super::sync::sealed`]).
    pub ok_started: u64,
    pub ok_finished: u64,
    pub files: u64,
    pub bytes: u64,
}

/// One run of a tier's sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub started: u64,
    pub finished: u64,
    pub ok: bool,
}

/// The last failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrRecord {
    pub at: u64,
    pub label: Option<String>,
    pub class: PushFailure,
    pub message: String,
}

/// `sync-snap.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapState {
    pub version: u32,
    pub labels: BTreeMap<String, LabelRecord>,
    pub last_run: Option<RunRecord>,
    pub last_error: Option<ErrRecord>,
}

impl Default for SnapState {
    fn default() -> Self {
        Self { version: STATE_VERSION, labels: BTreeMap::new(), last_run: None, last_error: None }
    }
}

impl SnapState {
    /// The last time any label pushed OK (finished).
    pub fn last_ok(&self) -> Option<u64> {
        self.labels.values().map(|r| r.ok_finished).max()
    }
}

/// `sync-big.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BigState {
    pub version: u32,
    pub last_ok: Option<RunRecord>,
    pub last_run: Option<RunRecord>,
    pub last_error: Option<ErrRecord>,
}

impl Default for BigState {
    fn default() -> Self {
        Self { version: STATE_VERSION, last_ok: None, last_run: None, last_error: None }
    }
}

/// Where everything lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePaths {
    pub dir: PathBuf,
}

impl ArchivePaths {
    /// `<state dir>/<prefix>/archive`. A relative `ARENA_STATE_DIR` is refused (a cron job and a
    /// shell would resolve it differently).
    pub fn for_config(cfg: &Config) -> Result<Self, String> {
        Ok(Self { dir: crate::snapshot::state_dir_for(cfg)?.join("archive") })
    }

    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn admin_key(&self) -> PathBuf {
        self.dir.join("admin_ed25519")
    }
    pub fn push_key(&self) -> PathBuf {
        self.dir.join("push_ed25519")
    }
    pub fn read_key(&self) -> PathBuf {
        self.dir.join("read_ed25519")
    }
    /// The public half of a key file.
    pub fn public(key: &Path) -> PathBuf {
        let mut s = key.as_os_str().to_owned();
        s.push(".pub");
        PathBuf::from(s)
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }
    pub fn host_file(&self) -> PathBuf {
        self.dir.join("archive.json")
    }
    pub fn sync_state(&self, tier: Tier) -> PathBuf {
        self.dir.join(format!("sync-{}.json", tier.as_str()))
    }
    pub fn sync_lock(&self, tier: Tier) -> PathBuf {
        self.dir.join(format!("sync-{}.lock", tier.as_str()))
    }

    /// The paths go into rsync's `-e` string, which rsync splits on spaces (quotes, but no
    /// backslashes): refuse a state dir that can't be written there plainly.
    pub fn check_plain(&self) -> Result<(), String> {
        let s = self.dir.to_str().ok_or_else(|| format!("the archive state dir {} isn't UTF-8", self.dir.display()))?;
        if s.chars().any(|c| c.is_whitespace() || "'\"\\$`".contains(c)) {
            return Err(format!(
                "the archive state dir {s} holds a space or quote, which rsync's -e can't carry — set ARENA_STATE_DIR to a plain path"
            ));
        }
        Ok(())
    }

    /// Create the directory owner-only.
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        crate::snapshot::create_private_dir(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Read `path` as `T`: `Ok(None)` when it doesn't exist, an error naming the file when it can't
/// be read, doesn't parse or has another `version`.
fn load<T: DeserializeOwned>(path: &Path, version_of: impl Fn(&T) -> u32) -> Result<Option<T>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let v: T = serde_json::from_str(&text).map_err(|e| format!("{} doesn't parse: {e}", path.display()))?;
    match version_of(&v) {
        STATE_VERSION => Ok(Some(v)),
        other => Err(format!(
            "{} is version {other}, this arena reads version {STATE_VERSION} — refusing to guess (a newer arena wrote it?)",
            path.display()
        )),
    }
}

/// `archive.json`, if `up` has written one.
pub fn load_host(path: &Path) -> Result<Option<ArchiveHost>, String> {
    load(path, |h: &ArchiveHost| h.version)
}

/// `sync-snap.json` (missing = nothing pushed yet).
pub fn load_snap(path: &Path) -> Result<SnapState, String> {
    Ok(load(path, |s: &SnapState| s.version)?.unwrap_or_default())
}

/// `sync-big.json` (missing = nothing pushed yet).
pub fn load_big(path: &Path) -> Result<BigState, String> {
    Ok(load(path, |s: &BigState| s.version)?.unwrap_or_default())
}

/// Write a state file atomically, 0600.
pub fn save<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    text.push('\n');
    crate::snapshot::write_atomic(path, text.as_bytes(), 0o600)
}

/// Write a plain-text state file (the pinned known_hosts) atomically, 0600.
pub fn save_text(path: &Path, text: &str) -> std::io::Result<()> {
    crate::snapshot::write_atomic(path, text.as_bytes(), 0o600)
}

/// Generate `key` (ed25519, no passphrase) unless it exists; `Ok(true)` when it was made.
/// `ssh-keygen` writes the private half 0600 and the public 0644.
pub fn ensure_keypair(key: &Path, comment: &str) -> std::io::Result<bool> {
    if key.exists() {
        return Ok(false);
    }
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(key)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "ssh-keygen for {} failed: {}",
            key.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(true)
}

/// A key's public half, trimmed (`None` if unreadable/empty).
pub fn read_pub(key: &Path) -> Option<String> {
    std::fs::read_to_string(ArchivePaths::public(key)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether `path` exists and this user can open it for reading.
pub fn readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-archive-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn host() -> ArchiveHost {
        ArchiveHost {
            version: STATE_VERSION,
            prefix: "arena9".into(),
            server_id: 5,
            ipv4: "192.0.2.1".into(),
            location: "nbg1".into(),
            snap_device: "/dev/disk/by-id/scsi-0HC_Volume_1".into(),
            big_device: "/dev/disk/by-id/scsi-0HC_Volume_2".into(),
            access_on: false,
            setup_version: 1,
            snap_gb: 50,
            big_gb: 200,
            host_key_fingerprint: Some("SHA256:abc".into()),
        }
    }

    /// Round trip, mode 0600, atomic replace, a missing file, a version mismatch → error (§14.13).
    #[test]
    fn state_files_round_trip_privately_and_refuse_another_version() {
        let t = tmp("rt");
        let p = ArchivePaths::new(t.0.join("archive"));
        p.ensure_dir().unwrap();
        assert_eq!(load_host(&p.host_file()).unwrap(), None);
        assert_eq!(load_snap(&p.sync_state(Tier::Snap)).unwrap(), SnapState::default());
        save(&p.host_file(), &host()).unwrap();
        assert_eq!(load_host(&p.host_file()).unwrap(), Some(host()));
        let mut snap = SnapState::default();
        snap.labels.insert("w1d1".into(), LabelRecord { ok_started: 1, ok_finished: 2, files: 3, bytes: 4 });
        snap.last_error = Some(ErrRecord { at: 9, label: Some("w1d2".into()), class: PushFailure::NoSpace, message: "full".into() });
        save(&p.sync_state(Tier::Snap), &snap).unwrap();
        assert_eq!(load_snap(&p.sync_state(Tier::Snap)).unwrap(), snap);
        // Replaced in place, no temp files left behind.
        snap.labels.clear();
        save(&p.sync_state(Tier::Snap), &snap).unwrap();
        assert_eq!(load_snap(&p.sync_state(Tier::Snap)).unwrap(), snap);
        let names: Vec<String> = std::fs::read_dir(&p.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |f: &Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&p.host_file()), 0o600);
            assert_eq!(mode(&p.sync_state(Tier::Snap)), 0o600);
            assert_eq!(mode(&p.dir), 0o700);
        }
        // Another version: an error, and the file is left as it is.
        let text = std::fs::read_to_string(p.host_file()).unwrap().replace("\"version\": 1", "\"version\": 2");
        std::fs::write(p.host_file(), &text).unwrap();
        assert!(load_host(&p.host_file()).unwrap_err().contains("version 2"));
        assert_eq!(std::fs::read_to_string(p.host_file()).unwrap(), text);
        std::fs::write(p.sync_state(Tier::Big), "{not json").unwrap();
        assert!(load_big(&p.sync_state(Tier::Big)).unwrap_err().contains("doesn't parse"));
    }

    #[test]
    fn paths_and_the_plain_path_check() {
        let p = ArchivePaths::new("/s/arena9/archive");
        assert_eq!(p.sync_lock(Tier::Big), Path::new("/s/arena9/archive/sync-big.lock"));
        assert_eq!(ArchivePaths::public(&p.read_key()), Path::new("/s/arena9/archive/read_ed25519.pub"));
        p.check_plain().unwrap();
        for bad in ["/s p/a", "/s'/a", "/s\"/a", "/s\\/a"] {
            assert!(ArchivePaths::new(bad).check_plain().is_err(), "{bad}");
        }
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR=/srv/st\n");
        assert_eq!(ArchivePaths::for_config(&cfg).unwrap().dir, Path::new("/srv/st/devtest/archive"));
        assert!(ArchivePaths::for_config(&Config::parse("ARENA_STATE_DIR=rel\n")).is_err());
    }

    #[test]
    fn keypairs_are_generated_once() {
        if std::process::Command::new("ssh-keygen").arg("-?").output().is_err() {
            eprintln!("no ssh-keygen — skipping");
            return;
        }
        let t = tmp("keys");
        let k = t.0.join("admin_ed25519");
        assert!(ensure_keypair(&k, "arena-archive-admin").unwrap());
        let first = read_pub(&k).unwrap();
        assert!(first.starts_with("ssh-ed25519 ") && first.ends_with("arena-archive-admin"), "{first}");
        assert!(!ensure_keypair(&k, "x").unwrap(), "kept");
        assert_eq!(read_pub(&k).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&k).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
