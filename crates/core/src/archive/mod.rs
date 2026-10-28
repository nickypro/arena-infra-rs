//! The backup archive (`arena archive …`): one small always-on Hetzner CPU VM per cohort that
//! keeps copy #2 of the cohort's backups, so they outlive pods (a wipe, a replace, the end of
//! the program) and participants can recover their own files without the operator.
//!
//! Two volumes, because they fail differently: volume A holds the dated small-file snapshots
//! (`snap/<wNdM>/<pod>/`, hard-linked across days exactly like the control box's own
//! `LOCAL_BACKUP_DIR`), volume B the all-files mirror (`big/<pod>/`). A checkpoint dump that
//! fills B can then never stop A's history from being written, and each is sized, resized,
//! monitored and deleted on its own.
//!
//! Who can do what (the whole design hangs on it):
//! - **The control box pushes** from `LOCAL_BACKUP_DIR` on its own cron lines, with a push key
//!   the archive forces into `rrsync -wo -no-del -no-lock`: it can write, never read, delete,
//!   get a shell or forward. Pods never push (every participant is root on every pod), and the
//!   archive never pulls (it would need a fleet key or a key into the control box): the archive
//!   holds **no outbound credential** and can't reach pods, providers or the control box.
//! - **Participants read** over sftp as one chrooted, read-only user (`backups`) with the cohort
//!   key (laptops) or a per-cohort read key (copied to pods) — read-only in three independent
//!   layers (`internal-sftp -R`, read-only bind mounts, ownership). Access starts OFF after
//!   `archive up` until the operator runs `archive access on`, which says plainly that everyone
//!   holding the cohort key can then read every pod's backups (they are root on every pod
//!   already). Secret-bearing files are stripped on the way out ([`SECRET_EXCLUDES`]) and only
//!   this cohort's pod dirs ever leave the control box ([`allowed_pod_dirs`]).
//! - **The operator** manages it with a dedicated admin key no pod authorizes, over SSH pinned to
//!   the host key `archive up` recorded ([`crate::ssh::HostKeyPolicy`]).
//! - **The fleet never sees it**: every resource carries the label [`LABEL_KEY`]` = <prefix>`,
//!   the Hetzner fleet listing drops labelled servers, and delete protection backs that up for
//!   an older binary.
//!
//! Nothing here deletes data on its own: no expiry, no pruning, no auto-resize. Deleting the
//! volumes is `archive down --delete-volumes --confirm <prefix>-archive`, typed.
//!
//! Layout of this module: this file holds the names, tiers and settings; [`plan`] the pure
//! up/down/resize planners; [`sync`] the push planner (labels, sealing, allowlist, argv,
//! failure classes); [`render`] every file and command sent to the VM; [`report`] the setup
//! report / probe parsers and the status judge; [`state`] the local state files; [`cloud`] the
//! Hetzner API seam (with a stateful fake for tests).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;

pub mod cloud;
pub mod plan;
pub mod render;
pub mod report;
pub mod state;
pub mod sync;

pub use cloud::{ArchiveCloud, CloudServer, CloudSshKey, CloudVolume, HetznerArchiveCloud, Inventory, Labels, Prices};
pub use plan::{plan_down, plan_resize, plan_up, DownStep, ResizePlan, SizeFlags, UpPlan, UpStep};
pub use sync::{
    allowed_pod_dirs, classify_push, label_day, label_end, link_labels_for, local_labels, plan_snap, push_args, sealed, PushFailure,
    SECRET_EXCLUDES,
};

/// The label key every archive resource carries; its value is the cohort prefix. The fleet
/// listing drops servers carrying this key (any value): `provider::hetzner::is_archive_server`.
pub const LABEL_KEY: &str = "arena-archive";
/// The label key that says which tier a volume is (`snap` | `big`).
pub const TIER_LABEL_KEY: &str = "arena-archive-tier";

/// The push user on the VM: its key is forced into rrsync (write-only, no deletes).
pub const PUSH_USER: &str = "arena-push";
/// The read user on the VM: sftp-only, chrooted, read-only.
pub const READ_USER: &str = "backups";
/// rrsync's root on the VM; the volumes are mounted under it.
pub const PUSH_ROOT: &str = "/srv/arena/push";
/// The read user's chroot; read-only binds of the two volumes.
pub const VIEW_ROOT: &str = "/srv/arena/view";
/// The image the VM runs (setup's commands assume Ubuntu's paths and packages).
pub const IMAGE: &str = "ubuntu-24.04";
/// What `setup.sh` this binary carries — recorded on the VM and in `archive.json`, so `status`
/// can say when `archive up` should be re-run to bring the VM's config up to date.
pub const SETUP_VERSION: u32 = 1;

/// Volume size bounds (Hetzner's: 10 GB to 10 TB).
pub const MIN_GB: u32 = 10;
pub const MAX_GB: u32 = 10240;
/// Default creation sizes (`ARCHIVE_SNAP_GB` / `ARCHIVE_BIG_GB`).
pub const DEFAULT_SNAP_GB: u32 = 50;
pub const DEFAULT_BIG_GB: u32 = 200;
/// Server types tried in order on placement/availability errors only (`ARCHIVE_SERVER_TYPES`).
pub const DEFAULT_SERVER_TYPES: &[&str] = &["cx23", "cax11"];
/// Default `--bwlimit` on every push (`ARCHIVE_BWLIMIT`; `0` = none): the control box is also
/// the participants' SSH proxy, so a first big push must not saturate its uplink.
pub const DEFAULT_BWLIMIT: &str = "10M";
/// Wall-clock budget per snap label / for the big push (`ARCHIVE_SNAP_TIMEOUT_SECS` /
/// `ARCHIVE_BIG_TIMEOUT_SECS`), on top of rsync's own I/O timeout.
pub const DEFAULT_SNAP_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_BIG_TIMEOUT_SECS: u64 = 7200;

/// Which volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Volume A: the dated small-file snapshots, `snap/<wNdM>/<pod>/`.
    Snap,
    /// Volume B: the all-files mirror, `big/<pod>/`.
    Big,
}

impl Tier {
    /// Both, in the order everything handles them (snap first: it's the history).
    pub const ALL: [Tier; 2] = [Tier::Snap, Tier::Big];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Snap => "snap",
            Tier::Big => "big",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "snap" => Some(Tier::Snap),
            "big" => Some(Tier::Big),
            _ => None,
        }
    }

    /// Where the volume is mounted on the VM (rrsync's view: `<PUSH_ROOT>/<tier>`).
    pub fn mountpoint(self) -> String {
        format!("{PUSH_ROOT}/{}", self.as_str())
    }

    /// Where its read-only bind is (the read user's view: `<VIEW_ROOT>/<tier>`).
    pub fn view_mountpoint(self) -> String {
        format!("{VIEW_ROOT}/{}", self.as_str())
    }

    /// The flag that sizes it (`--snap-gb` / `--big-gb`), for printed commands.
    pub fn gb_flag(self) -> &'static str {
        match self {
            Tier::Snap => "--snap-gb",
            Tier::Big => "--big-gb",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every name the archive's resources go by, from the cohort prefix — one place, so the
/// planner, the API calls, the printed commands and the typed `--confirm` agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveNames {
    pub prefix: String,
}

impl ArchiveNames {
    /// Validated: the prefix becomes part of a hostname (`<prefix>-archive`), volume names and
    /// an ssh alias, so only letters, digits and inner `-`; never `archive` itself — the alias
    /// would be `archive-archive`, which the fleet's own `Host archive-*` wildcard would match.
    pub fn new(prefix: &str) -> Result<Self, String> {
        let ok = !prefix.is_empty()
            && prefix.len() <= 40
            && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !prefix.starts_with('-')
            && !prefix.ends_with('-');
        if !ok {
            return Err(format!(
                "MACHINE_NAME_PREFIX `{prefix}` can't name an archive: letters, digits and inner `-` only (≤ 40)"
            ));
        }
        if prefix == "archive" {
            return Err("MACHINE_NAME_PREFIX can't be `archive`: the archive's ssh alias would be `archive-archive`, \
                        which the fleet's `Host archive-*` block would match"
                .into());
        }
        Ok(Self { prefix: prefix.to_string() })
    }

    /// From the config's `MACHINE_NAME_PREFIX` (default `arena`, as everywhere).
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        Self::new(cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena"))
    }

    /// The server: `<prefix>-archive` — also the exact text `down --delete-volumes` wants typed.
    pub fn server(&self) -> String {
        format!("{}-archive", self.prefix)
    }

    /// A volume: `<prefix>-archive-snap` / `<prefix>-archive-big`.
    pub fn volume(&self, tier: Tier) -> String {
        format!("{}-archive-{}", self.prefix, tier.as_str())
    }

    /// The Hetzner SSH-key object holding the admin pubkey: `<prefix>-archive-admin`.
    pub fn ssh_key(&self) -> String {
        format!("{}-archive-admin", self.prefix)
    }

    /// The ssh alias participants and pods use: `archive-<prefix>`. Deliberately NOT
    /// `<prefix>-archive`: the fleet's `Host <prefix>-*` wildcard (`sshconfig`) comes first in
    /// every generated config and would impose `User root`, the proxy's `HostName` and
    /// `StrictHostKeyChecking=no` on it.
    pub fn alias(&self) -> String {
        format!("archive-{}", self.prefix)
    }

    /// The labels on the server and the SSH-key object.
    pub fn labels(&self) -> Labels {
        BTreeMap::from([(LABEL_KEY.to_string(), self.prefix.clone())])
    }

    /// A volume's labels: the prefix and its tier.
    pub fn volume_labels(&self, tier: Tier) -> Labels {
        let mut l = self.labels();
        l.insert(TIER_LABEL_KEY.to_string(), tier.as_str().to_string());
        l
    }

    /// Whether `labels` mark a resource as this cohort's archive.
    pub fn owns(&self, labels: &Labels) -> bool {
        labels.get(LABEL_KEY).map(String::as_str) == Some(self.prefix.as_str())
    }
}

/// The archive's settings from config — pure, validated up front, so a typo fails `up`/`sync`
/// before anything is created or pushed (never silently a default).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveSettings {
    /// `ARCHIVE_LOCATION`, else `HETZNER_LOCATION`, else `nbg1`. Server and volumes must share it.
    pub location: String,
    /// `ARCHIVE_SERVER_TYPES` (comma-separated), tried in order on availability errors.
    pub server_types: Vec<String>,
    /// Creation sizes (`ARCHIVE_SNAP_GB` / `ARCHIVE_BIG_GB`), 10..=10240.
    pub snap_gb: u32,
    pub big_gb: u32,
    /// `ARCHIVE_KEEP_UNTIL` as days since the epoch: drives messages, the README and
    /// `teardown --check`'s `◷ kept` — never a deletion.
    pub keep_until_days: Option<i64>,
    /// `ARCHIVE_HOST`: a DNS name for participants' and pods' configs (pushes always use the
    /// pinned IP).
    pub host_override: Option<String>,
    /// `ARCHIVE_BWLIMIT` as rsync takes it (`10M`), `None` for unlimited (`0`).
    pub bwlimit: Option<String>,
    /// Wall-clock budgets per snap label / for the big push.
    pub snap_timeout: Duration,
    pub big_timeout: Duration,
    /// `ARCHIVE_EXTRA_EXCLUDES`: patterns ADDED to [`SECRET_EXCLUDES`] (which can't be shortened).
    pub extra_excludes: Vec<String>,
    /// The snap push's `--max-size` safety net: `BACKUP_MAX_SIZE`, else `50M`.
    pub snap_max_size: String,
}

impl ArchiveSettings {
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let get = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty());
        // One location: the archive's server and its volumes must share it, so it can't walk as a
        // fleet create does. HETZNER_LOCATION's first named one when it lists any (`any` alone
        // names none), else nbg1.
        let fleet = crate::provider::hetzner::Locations::parse(get("HETZNER_LOCATION"));
        let location = get("ARCHIVE_LOCATION").or_else(|| fleet.first()).unwrap_or("nbg1").to_string();
        if !simple_word(&location) {
            return Err(format!("ARCHIVE_LOCATION `{location}`: a Hetzner location name like nbg1"));
        }
        let server_types: Vec<String> = match get("ARCHIVE_SERVER_TYPES") {
            Some(raw) => raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            None => DEFAULT_SERVER_TYPES.iter().map(|s| s.to_string()).collect(),
        };
        if server_types.is_empty() || !server_types.iter().all(|t| simple_word(t)) {
            return Err(format!(
                "ARCHIVE_SERVER_TYPES `{}`: comma-separated Hetzner server types like cx23,cax11",
                get("ARCHIVE_SERVER_TYPES").unwrap_or("")
            ));
        }
        let size = |key: &str, default: u32| -> Result<u32, String> {
            match get(key) {
                None => Ok(default),
                Some(raw) => raw.parse::<u32>().ok().filter(|n| (MIN_GB..=MAX_GB).contains(n)).ok_or_else(|| {
                    format!("{key} must be a whole number of GB, {MIN_GB}..={MAX_GB} (got `{raw}`)")
                }),
            }
        };
        let keep_until_days = match get("ARCHIVE_KEEP_UNTIL") {
            None => None,
            Some(raw) => Some(parse_day(raw).ok_or_else(|| format!("ARCHIVE_KEEP_UNTIL must be YYYY-MM-DD (got `{raw}`)"))?),
        };
        let host_override = match get("ARCHIVE_HOST") {
            None => None,
            Some(h) if is_hostname(h) => Some(h.to_string()),
            Some(h) => return Err(format!("ARCHIVE_HOST `{h}`: a DNS name or IP (letters, digits, `.`, `-`, `:`)")),
        };
        let bwlimit = match get("ARCHIVE_BWLIMIT") {
            None => Some(DEFAULT_BWLIMIT.to_string()),
            Some("0") => None,
            Some(raw) if is_rsync_size(raw) => Some(raw.to_string()),
            Some(raw) => return Err(format!("ARCHIVE_BWLIMIT must be an rsync rate like 10M or 500K, or 0 for none (got `{raw}`)")),
        };
        let snap_max_size = get("BACKUP_MAX_SIZE").unwrap_or("50M").to_string();
        if !is_rsync_size(&snap_max_size) {
            return Err(format!("BACKUP_MAX_SIZE must be an rsync size like 50M (got `{snap_max_size}`)"));
        }
        let extra_excludes: Vec<String> = get("ARCHIVE_EXTRA_EXCLUDES")
            .map(|raw| raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        // `--exclude=` reads a leading `!` as "clear every rule so far" — the secret excludes
        // included — and `+ `/`- ` as a rule prefix (an INCLUDE): refuse those, and control chars.
        if let Some(bad) = extra_excludes.iter().find(|p| p.chars().any(|c| c.is_control()) || p.starts_with(['!', '+', '-'])) {
            return Err(format!(
                "ARCHIVE_EXTRA_EXCLUDES: pattern {bad:?} can't start with `!`, `+` or `-` (rsync would read it as a rule, not a \
                 pattern) or hold a control character"
            ));
        }
        Ok(Self {
            location,
            server_types,
            snap_gb: size("ARCHIVE_SNAP_GB", DEFAULT_SNAP_GB)?,
            big_gb: size("ARCHIVE_BIG_GB", DEFAULT_BIG_GB)?,
            keep_until_days,
            host_override,
            bwlimit,
            snap_timeout: budget("ARCHIVE_SNAP_TIMEOUT_SECS", get("ARCHIVE_SNAP_TIMEOUT_SECS"), DEFAULT_SNAP_TIMEOUT_SECS)?,
            big_timeout: budget("ARCHIVE_BIG_TIMEOUT_SECS", get("ARCHIVE_BIG_TIMEOUT_SECS"), DEFAULT_BIG_TIMEOUT_SECS)?,
            extra_excludes,
            snap_max_size,
        })
    }

    /// The creation size of `tier`.
    pub fn size_of(&self, tier: Tier) -> u32 {
        match tier {
            Tier::Snap => self.snap_gb,
            Tier::Big => self.big_gb,
        }
    }

    /// The wall-clock budget of one push of `tier`.
    pub fn timeout_of(&self, tier: Tier) -> Duration {
        match tier {
            Tier::Snap => self.snap_timeout,
            Tier::Big => self.big_timeout,
        }
    }

    /// `ARCHIVE_KEEP_UNTIL` as text, or the words every message uses when it's unset.
    pub fn keep_until_text(&self) -> String {
        keep_until_text(self.keep_until_days)
    }
}

/// `kept until 2027-01-31`-style date text, or "no retention date set".
pub fn keep_until_text(days: Option<i64>) -> String {
    match days {
        Some(d) => crate::schedule::ymd_string(d),
        None => "no retention date set".to_string(),
    }
}

/// `ARCHIVE_KEEP_UNTIL` alone (for `teardown --check`, which needn't validate the rest).
pub fn keep_until_from_config(cfg: &Config) -> Result<Option<i64>, String> {
    match cfg.get("ARCHIVE_KEEP_UNTIL").map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(raw) => parse_day(raw).map(Some).ok_or_else(|| format!("ARCHIVE_KEEP_UNTIL must be YYYY-MM-DD (got `{raw}`)")),
    }
}

/// A budget setting, validated like `pull::backup_timeout`: whole seconds, 1..=a day; anything
/// else names the value — a typo must never become "no limit".
fn budget(key: &str, raw: Option<&str>, default: u64) -> Result<Duration, String> {
    let Some(raw) = raw else { return Ok(Duration::from_secs(default)) };
    raw.parse::<u64>()
        .ok()
        .filter(|n| (1..=crate::setup::MAX_STEP_TIMEOUT_SECS).contains(n))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{key} must be a whole number of seconds, 1..={} (got `{raw}`)", crate::setup::MAX_STEP_TIMEOUT_SECS))
}

fn parse_day(raw: &str) -> Option<i64> {
    let (y, m, d) = crate::schedule::parse_ymd(raw)?;
    Some(crate::schedule::days_from_civil(y, m, d))
}

/// `[a-z0-9-]+`: a Hetzner location or server-type name.
fn simple_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A DNS name or IP literal safe to put in an ssh config and a known_hosts line.
fn is_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// An rsync size/rate: digits, an optional fraction, an optional unit (`10M`, `1.5G`, `500k`).
fn is_rsync_size(s: &str) -> bool {
    let body = s.strip_suffix(|c: char| "KMGTkmgtBb".contains(c)).unwrap_or(s);
    let mut parts = body.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    !int.is_empty() && int.chars().all(|c| c.is_ascii_digit()) && frac.is_none_or(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
}

/// Where the archive's Hetzner token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `ARCHIVE_HETZNER_API_KEY` — its own project, the recommended setup.
    Archive,
    /// Fell back to the fleet's `HETZNER_API_KEY`: the archive then shares the fleet's project.
    Fleet,
}

/// The one line `archive up`/`status` and `config check` print when the archive falls back to
/// the fleet's token.
pub const FLEET_TOKEN_WARNING: &str = "warning: ARCHIVE_HETZNER_API_KEY is unset — the archive uses the fleet's \
     HETZNER_API_KEY (same Hetzner project: fleet commands skip it by its label and its delete protection stays on; a \
     separate project is recommended for prod)";

/// The token the archive's API calls use: `ARCHIVE_HETZNER_API_KEY`, else the fleet's
/// `HETZNER_API_KEY` (with [`FLEET_TOKEN_WARNING`]). `None` = neither is set.
pub fn archive_token(cfg: &Config) -> Option<(String, TokenSource)> {
    let get = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    get("ARCHIVE_HETZNER_API_KEY")
        .map(|t| (t, TokenSource::Archive))
        .or_else(|| get("HETZNER_API_KEY").map(|t| (t, TokenSource::Fleet)))
}

/// The key blob of an OpenSSH public key line (`[options] type base64 [comment]`), for
/// comparing keys whatever their comments or options: the token after the key type.
pub fn key_blob(line: &str) -> Option<&str> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| is_key_type(w))?;
    words.get(i + 1).copied().filter(|b| b.len() > 16)
}

/// `(type, blob)` of a public key line — what a known_hosts line or an authorized key is built from.
pub fn key_type_and_blob(line: &str) -> Option<(&str, &str)> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| is_key_type(w))?;
    Some((words[i], words.get(i + 1).copied().filter(|b| b.len() > 16)?))
}

fn is_key_type(w: &str) -> bool {
    w.starts_with("ssh-") || w.starts_with("ecdsa-sha2-") || w.starts_with("sk-ssh-") || w.starts_with("sk-ecdsa-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_labels_derive_from_the_prefix() {
        let n = ArchiveNames::new("arena9").unwrap();
        assert_eq!(n.server(), "arena9-archive");
        assert_eq!(n.volume(Tier::Snap), "arena9-archive-snap");
        assert_eq!(n.volume(Tier::Big), "arena9-archive-big");
        assert_eq!(n.ssh_key(), "arena9-archive-admin");
        assert_eq!(n.alias(), "archive-arena9");
        assert_eq!(n.labels(), BTreeMap::from([("arena-archive".to_string(), "arena9".to_string())]));
        assert_eq!(n.volume_labels(Tier::Big)["arena-archive-tier"], "big");
        assert!(n.owns(&n.volume_labels(Tier::Snap)));
        assert!(!n.owns(&ArchiveNames::new("arena8").unwrap().labels()));
        for bad in ["", "archive", "-x", "x-", "a b", "a/b", "a.b"] {
            assert!(ArchiveNames::new(bad).is_err(), "{bad:?}");
        }
    }

    /// A tiny ssh_config first-match matcher (`*` and `?` globs, as ssh's `Host`): the alias is
    /// NOT caught by the fleet's `Host <prefix>-*` wildcard, while `<prefix>-archive` would be.
    #[test]
    fn the_alias_escapes_the_fleet_wildcard() {
        fn glob(p: &[u8], s: &[u8]) -> bool {
            match (p.first(), s.first()) {
                (None, None) => true,
                (Some(b'*'), _) => glob(&p[1..], s) || (!s.is_empty() && glob(p, &s[1..])),
                (Some(b'?'), Some(_)) => glob(&p[1..], &s[1..]),
                (Some(a), Some(b)) if a == b => glob(&p[1..], &s[1..]),
                _ => false,
            }
        }
        let n = ArchiveNames::new("arena9").unwrap();
        let wildcard = "arena9-*";
        assert!(!glob(wildcard.as_bytes(), n.alias().as_bytes()));
        assert!(glob(wildcard.as_bytes(), n.server().as_bytes()), "why the alias isn't the server's name");
        assert!(glob(wildcard.as_bytes(), b"arena9-apple"));
    }

    #[test]
    fn settings_defaults_and_overrides() {
        let s = ArchiveSettings::from_config(&Config::parse("")).unwrap();
        assert_eq!(s.location, "nbg1");
        assert_eq!(s.server_types, ["cx23", "cax11"]);
        assert_eq!((s.snap_gb, s.big_gb), (50, 200));
        assert_eq!(s.keep_until_days, None);
        assert_eq!(s.keep_until_text(), "no retention date set");
        assert_eq!(s.bwlimit.as_deref(), Some("10M"));
        assert_eq!((s.snap_timeout.as_secs(), s.big_timeout.as_secs()), (900, 7200));
        assert_eq!(s.snap_max_size, "50M");
        assert!(s.extra_excludes.is_empty());
        let s = ArchiveSettings::from_config(&Config::parse(
            "HETZNER_LOCATION=fsn1\nARCHIVE_SERVER_TYPES= cx33 , cax21\nARCHIVE_SNAP_GB=10\nARCHIVE_BIG_GB=10240\n\
             ARCHIVE_KEEP_UNTIL=2027-01-31\nARCHIVE_HOST=archive.example.org\nARCHIVE_BWLIMIT=0\n\
             ARCHIVE_SNAP_TIMEOUT_SECS=60\nARCHIVE_BIG_TIMEOUT_SECS=86400\nARCHIVE_EXTRA_EXCLUDES=*.key, secrets/\nBACKUP_MAX_SIZE=20M\n",
        ))
        .unwrap();
        assert_eq!(s.location, "fsn1");
        assert_eq!(s.server_types, ["cx33", "cax21"]);
        assert_eq!((s.snap_gb, s.big_gb), (10, 10240));
        assert_eq!(s.keep_until_text(), "2027-01-31");
        assert_eq!(s.host_override.as_deref(), Some("archive.example.org"));
        assert_eq!(s.bwlimit, None);
        assert_eq!((s.snap_timeout.as_secs(), s.big_timeout.as_secs()), (60, 86400));
        assert_eq!(s.extra_excludes, ["*.key", "secrets/"]);
        assert_eq!(s.snap_max_size, "20M");
        // ARCHIVE_LOCATION wins over the fleet's.
        let s = ArchiveSettings::from_config(&Config::parse("HETZNER_LOCATION=fsn1\nARCHIVE_LOCATION=hel1\n")).unwrap();
        assert_eq!(s.location, "hel1");
        // The fleet's walks; the archive takes one place: its first named location, else nbg1.
        for (fleet, want) in [("any", "nbg1"), ("\"hel1,any\"", "hel1"), ("\"fsn1, nbg1\"", "fsn1"), ("\"any fsn1\"", "fsn1")] {
            let s = ArchiveSettings::from_config(&Config::parse(&format!("HETZNER_LOCATION={fleet}\n"))).unwrap();
            assert_eq!(s.location, want, "{fleet}");
        }
    }

    #[test]
    fn settings_refuse_bad_values_by_name() {
        for (text, needle) in [
            ("ARCHIVE_SNAP_GB=5", "ARCHIVE_SNAP_GB"),
            ("ARCHIVE_BIG_GB=10241", "ARCHIVE_BIG_GB"),
            ("ARCHIVE_BIG_GB=1T", "ARCHIVE_BIG_GB"),
            ("ARCHIVE_KEEP_UNTIL=31/01/2027", "ARCHIVE_KEEP_UNTIL"),
            ("ARCHIVE_KEEP_UNTIL=2027-13-01", "ARCHIVE_KEEP_UNTIL"),
            ("ARCHIVE_BWLIMIT=fast", "ARCHIVE_BWLIMIT"),
            ("ARCHIVE_BWLIMIT=-1", "ARCHIVE_BWLIMIT"),
            ("ARCHIVE_SNAP_TIMEOUT_SECS=0", "ARCHIVE_SNAP_TIMEOUT_SECS"),
            ("ARCHIVE_BIG_TIMEOUT_SECS=2h", "ARCHIVE_BIG_TIMEOUT_SECS"),
            ("ARCHIVE_BIG_TIMEOUT_SECS=86401", "ARCHIVE_BIG_TIMEOUT_SECS"),
            ("ARCHIVE_SERVER_TYPES=,", "ARCHIVE_SERVER_TYPES"),
            ("ARCHIVE_SERVER_TYPES=CX23", "ARCHIVE_SERVER_TYPES"),
            ("ARCHIVE_LOCATION=nbg 1", "ARCHIVE_LOCATION"),
            ("ARCHIVE_HOST=evil host", "ARCHIVE_HOST"),
            ("ARCHIVE_HOST=-oProxyCommand=x", "ARCHIVE_HOST"),
            ("BACKUP_MAX_SIZE=big", "BACKUP_MAX_SIZE"),
        ] {
            let e = ArchiveSettings::from_config(&Config::parse(text)).unwrap_err();
            assert!(e.contains(needle), "{text}: {e}");
        }
        for ok in ["10M", "500K", "1.5G", "1024", "2g"] {
            assert!(is_rsync_size(ok), "{ok}");
        }
        for bad in ["", "M", "1.", ".5M", "1..5M", "1MB5", "1e3"] {
            assert!(!is_rsync_size(bad), "{bad}");
        }
    }

    #[test]
    fn the_token_prefers_the_archives_own_and_says_when_it_falls_back() {
        assert_eq!(archive_token(&Config::parse("")), None);
        assert_eq!(archive_token(&Config::parse("HETZNER_API_KEY=f\n")), Some(("f".into(), TokenSource::Fleet)));
        assert_eq!(
            archive_token(&Config::parse("HETZNER_API_KEY=f\nARCHIVE_HETZNER_API_KEY=a\n")),
            Some(("a".into(), TokenSource::Archive))
        );
        assert_eq!(archive_token(&Config::parse("ARCHIVE_HETZNER_API_KEY=\nHETZNER_API_KEY=f\n")).unwrap().1, TokenSource::Fleet);
    }

    #[test]
    fn key_blobs_ignore_options_and_comments() {
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIOv1m2n4bQ7oZ8c1Yt0y5g4xjvGmZ0eHq8b5mCw1ZkWh";
        assert_eq!(key_blob(&format!("ssh-ed25519 {blob} me@box")), Some(blob));
        assert_eq!(key_blob(&format!("restrict,command=\"x y\" ssh-ed25519 {blob}")), Some(blob));
        assert_eq!(key_type_and_blob(&format!("1.2.3.4 ssh-ed25519 {blob}")), Some(("ssh-ed25519", blob)));
        assert_eq!(key_blob("garbage"), None);
        assert_eq!(key_blob("ssh-ed25519"), None);
    }
}
