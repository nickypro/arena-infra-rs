//! One read-only picture of the fleet (PLAN Phase 3/4): every pod with its cost labels,
//! proxy port + whether that forward is live, its last `pods test --deep` verdict and
//! whether its SSH port answers right now — built by one pure function ([`build`]) so the
//! CLI (`arena snapshot`), the TUI and the public dashboard all read the same thing.
//!
//! Four pieces, split so most of it is pure and table-tested:
//!
//! 1. **The health cache** ([`HealthCache`]): a deep check takes minutes and SSHes into
//!    every pod, so `snapshot` never runs one — it reads what the last `pods test --deep` /
//!    `up --check` recorded, with its age. Stored per fleet prefix
//!    ([`health_cache_path`]) so the sandbox and production never share one, written
//!    atomically (temp + rename) under a lock, and loaded tolerantly (a corrupt file is an
//!    empty cache plus one warning — it's a convenience, never a reason to fail).
//! 2. **[`FleetSnapshot`]**: the internal view (ids, endpoints, costs, raw check reasons).
//!    For the operator only.
//! 3. **The reachability probe** ([`probe_reachability`]): a provider lists a pod
//!    `RUNNING` with an endpoint well before (and long after) anyone can log in — booting,
//!    sshd not up yet, a wedged host. So the snapshot dials each billing cohort machine's
//!    SSH port (concurrently, a few seconds at most, no auth, no command —
//!    [`SshPortProbe`]; staff boxes and off-list pods are never dialed) and the public `up`
//!    needs that answer. Behind a seam ([`Reach`]) so tests never dial out.
//! 4. **[`PublicSnapshot`]**: what may be published with no auth. An **allowlist by
//!    construction**, not redaction: a separate struct with only name / GPU / up-starting-
//!    down / health verdict + age + a reason from a *fixed* vocabulary ([`Issue`]) /
//!    maintenance start+end, for pods on `MACHINE_NAME_LIST` only. A new internal field
//!    can't leak by accident because nothing copies the internal struct wholesale; raw
//!    check text (which carries IPs, paths, hostnames) never crosses over — only the enum.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::fleet::{self, clip, FleetCost};
use crate::health::{Check, PodHealth, Status};
use crate::naming::{is_absolute, qualify};
use crate::pod::Pod;
use crate::proxy::{parse_nginx, Listing, ListingStatus};
use crate::selector::Naming;
use crate::status::is_billing;
use crate::table::{self, Align};

// ---------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------

/// Seconds since the Unix epoch, from the system clock (the one impure read here; callers
/// pass `now` into everything else so it stays testable).
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `2026-10-08T12:34:56Z` — RFC 3339 in UTC, which every browser's `Date` parses. No date
/// crate for one format: the civil-date math is [`crate::schedule::civil_from_days`].
pub fn rfc3339(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    let (y, m, d) = crate::schedule::civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// A compact age: `45s`, `12m`, `3h`, `2d`.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

// ---------------------------------------------------------------------------------------
// Health cache
// ---------------------------------------------------------------------------------------

/// Why a pod isn't a clean PASS, in words safe to publish. A **fixed vocabulary**: each
/// value maps from a check's stable *name* ([`Issue::from_check`]), never from its detail
/// text — details carry IPs, mount paths, hostnames and raw stderr. Serialized as the label
/// itself, and an unknown value read back (an edited or newer cache file) becomes
/// [`Issue::Other`], so no free text can ride in through the cache either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Issue {
    #[serde(rename = "GPU error")]
    GpuError,
    #[serde(rename = "driver too old")]
    DriverTooOld,
    #[serde(rename = "driver unknown")]
    DriverUnknown,
    #[serde(rename = "software error")]
    SoftwareError,
    #[serde(rename = "slow network")]
    SlowNetwork,
    #[serde(rename = "low disk")]
    LowDisk,
    #[serde(rename = "busy host")]
    BusyHost,
    #[serde(rename = "maintenance scheduled")]
    MaintenanceScheduled,
    #[serde(rename = "unreachable")]
    Unreachable,
    #[serde(rename = "check incomplete")]
    CheckIncomplete,
    #[serde(rename = "other issue", other)]
    Other,
}

impl Issue {
    /// The label (also its JSON form).
    pub fn label(self) -> &'static str {
        match self {
            Issue::GpuError => "GPU error",
            Issue::DriverTooOld => "driver too old",
            Issue::DriverUnknown => "driver unknown",
            Issue::SoftwareError => "software error",
            Issue::SlowNetwork => "slow network",
            Issue::LowDisk => "low disk",
            Issue::BusyHost => "busy host",
            Issue::MaintenanceScheduled => "maintenance scheduled",
            Issue::Unreachable => "unreachable",
            Issue::CheckIncomplete => "check incomplete",
            Issue::Other => "other issue",
        }
    }

    /// The issue a failed/warned check stands for, by the check's stable name (see
    /// `health::evaluate`): `ssh` → unreachable, the GPU checks (`nvidia-smi`, `cuda`,
    /// `device_count`, `gpu<i>`, `peer_copy`, `nccl`, `provider`) → GPU error, `driver` →
    /// too old (FAIL) / unknown (WARN), `torch` → software error, `network`, `disk`, `load`,
    /// `maintenance`, and `script` (no or cut-off output) → check incomplete.
    pub fn from_check(c: &Check) -> Issue {
        let gpu_index = |n: &str| n.strip_prefix("gpu").is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()));
        match c.name.as_str() {
            "ssh" => Issue::Unreachable,
            "script" => Issue::CheckIncomplete,
            "driver" if c.status == Status::Fail => Issue::DriverTooOld,
            "driver" => Issue::DriverUnknown,
            "nvidia-smi" | "cuda" | "device_count" | "peer_copy" | "nccl" | "provider" => Issue::GpuError,
            n if gpu_index(n) => Issue::GpuError,
            "torch" => Issue::SoftwareError,
            "network" => Issue::SlowNetwork,
            "disk" => Issue::LowDisk,
            "load" => Issue::BusyHost,
            "maintenance" => Issue::MaintenanceScheduled,
            _ => Issue::Other,
        }
    }
}

/// One pod's last deep-check verdict, as cached. Internal: `reasons` is the check text
/// (it can name IPs and paths) — the public view reads only `status`, `checked_at` and
/// `issues`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthRecord {
    pub id: String,
    pub name: String,
    /// The provider that listed the pod: ids are only unique per provider, and a record is
    /// pruned only once *its* provider listed successfully without it.
    pub provider: String,
    /// `pass` / `warn` / `fail`.
    pub status: Status,
    /// When the check ran (Unix seconds).
    pub checked_at: u64,
    /// `check: detail` for every FAIL then every WARN, one line each (clipped).
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The same checks as [`Issue`]s, worst first, without repeats.
    #[serde(default)]
    pub issues: Vec<Issue>,
    /// What the machine itself reported (`2×RTX A4000`), when the check got that far.
    #[serde(default)]
    pub gpu: Option<String>,
}

/// Longest `reasons` line kept: enough for any check's detail, short enough that a
/// runaway stderr can't bloat the cache.
const REASON_MAX: usize = 200;

impl HealthRecord {
    /// The record for one deep-check result, checked at `now`.
    pub fn from_health(h: &PodHealth, now: u64) -> Self {
        let bad = |s: Status| h.checks.iter().filter(move |c| c.status == s);
        let worst: Vec<&Check> = bad(Status::Fail).chain(bad(Status::Warn)).collect();
        let mut issues: Vec<Issue> = Vec::new();
        for c in &worst {
            let i = Issue::from_check(c);
            if !issues.contains(&i) {
                issues.push(i);
            }
        }
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        HealthRecord {
            id: h.id.clone(),
            name: h.name.clone(),
            provider: h.provider.clone(),
            status: h.status,
            checked_at: now,
            reasons: worst.iter().map(|c| clip(&flat(&format!("{}: {}", c.name, c.detail)), REASON_MAX)).collect(),
            issues,
            gpu: h.facts.as_ref().map(|f| f.gpu_label()).filter(|g| g != "-"),
        }
    }
}

/// The cache key: `provider:id` (ids are only unique per provider).
pub fn cache_key(provider: &str, id: &str) -> String {
    format!("{provider}:{id}")
}

/// The on-disk format's version; a file with another one is ignored (with a warning) rather
/// than misread.
pub const HEALTH_CACHE_VERSION: u32 = 1;

/// Every pod's last deep-check verdict, keyed by [`cache_key`]. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthCache {
    pub version: u32,
    pub pods: BTreeMap<String, HealthRecord>,
}

/// The cache's file name inside its per-prefix state directory.
pub const HEALTH_CACHE_FILE: &str = "health.json";

/// A fleet prefix as one safe path component: anything but `[A-Za-z0-9._-]` becomes `_`,
/// and an empty / `.` / `..` prefix becomes `_` — a config value must never steer a write
/// outside the state directory.
fn path_component(prefix: &str) -> String {
    let s: String =
        prefix.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    if s.is_empty() || s == "." || s == ".." {
        "_".into()
    } else {
        s
    }
}

/// Where the health cache lives: `$ARENA_STATE_DIR/<prefix>/health.json`, else
/// `$XDG_STATE_HOME/arena/<prefix>/health.json`, else
/// `~/.local/state/arena/<prefix>/health.json`. Scoped by `MACHINE_NAME_PREFIX` so the
/// sandbox (`devtest`) and production (`arena9`) never read or prune each other's records,
/// even with one state directory. `lookup` reads a variable (the CLI: config, then the
/// environment), so this is pure. A relative `XDG_STATE_HOME` is ignored (the XDG spec
/// says so); a relative `ARENA_STATE_DIR` is refused — a cron job and a shell would
/// resolve it to different places.
pub fn health_cache_path(prefix: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let set = |k: &str| lookup(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let root = if let Some(dir) = set("ARENA_STATE_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(format!("ARENA_STATE_DIR must be an absolute path (got {})", dir.display()));
        }
        dir
    } else if let Some(xdg) = set("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        xdg.join("arena")
    } else {
        let home = set("HOME").ok_or("no ARENA_STATE_DIR, XDG_STATE_HOME or HOME to keep it in")?;
        PathBuf::from(home).join(".local").join("state").join("arena")
    };
    Ok(root.join(path_component(prefix)).join(HEALTH_CACHE_FILE))
}

/// [`health_cache_path`] for a loaded config, as every surface resolves it: the fleet
/// prefix and `ARENA_STATE_DIR` from the config (which also takes it from the
/// environment), the XDG/`HOME` fallbacks from the environment. One function, so the CLI
/// that writes the cache and the dashboard that reads it can't look in different places.
pub fn health_cache_path_for(cfg: &Config) -> Result<PathBuf, String> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    health_cache_path(prefix, |k| match k {
        "ARENA_STATE_DIR" => cfg.get(k).map(String::from),
        _ => std::env::var(k).ok(),
    })
}

impl HealthCache {
    pub fn new() -> Self {
        Self { version: HEALTH_CACHE_VERSION, pods: BTreeMap::new() }
    }

    /// Parse a cache file. `Err` (with why) for anything that isn't a version-1 cache.
    pub fn parse(text: &str) -> Result<Self, String> {
        let cache: HealthCache = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if cache.version != HEALTH_CACHE_VERSION {
            return Err(format!("unsupported version {} (expected {HEALTH_CACHE_VERSION})", cache.version));
        }
        Ok(cache)
    }

    /// Load tolerantly: a missing file is an empty cache (no check has run yet — normal);
    /// an unreadable or corrupt one is an empty cache plus one warning line. Never an error:
    /// `snapshot` must still render without health, and the next writer replaces the file.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match Self::parse(&text) {
                Ok(c) => (c, None),
                Err(e) => (Self::new(), Some(format!("warning: ignoring the health cache {} ({e})", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::new(), None),
            Err(e) => (Self::new(), Some(format!("warning: can't read the health cache {} ({e})", path.display()))),
        }
    }

    /// The record for a listed pod, if one was cached.
    pub fn get(&self, pod: &Pod) -> Option<&HealthRecord> {
        self.pods.get(&cache_key(&pod.provider, &pod.id))
    }

    /// Record `results` (checked at `now`), replacing each pod's previous record. A result
    /// without an id (never from a real listing) is skipped: it couldn't be joined back.
    pub fn merge(&mut self, results: &[PodHealth], now: u64) {
        for h in results.iter().filter(|h| !h.id.is_empty()) {
            self.pods.insert(cache_key(&h.provider, &h.id), HealthRecord::from_health(h, now));
        }
    }

    /// Drop the records of pods confirmed gone — absent from their provider's
    /// *successful* listing (the proxy merge's R3/R4 rule): a provider that failed to list,
    /// or wasn't queried, confirms nothing, so its records stay. Returns how many went.
    pub fn prune(&mut self, listing: &Listing) -> usize {
        let before = self.pods.len();
        self.pods.retain(|_, r| match listing.status(&r.provider) {
            ListingStatus::Ok => listing
                .providers
                .iter()
                .filter(|pl| pl.provider == r.provider)
                .any(|pl| pl.pods.as_ref().is_ok_and(|pods| pods.iter().any(|p| p.id == r.id))),
            ListingStatus::Failed(_) | ListingStatus::NotQueried => true,
        });
        before - self.pods.len()
    }

    pub fn to_json(&self) -> String {
        // A map of plain structs: serializing it can't fail.
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    /// Write the cache to `path` atomically, owner-only (its reasons can name IPs), creating
    /// the state directory (owner-only) if needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        write_atomic(path, self.to_json().as_bytes(), 0o600)
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Merge `results` into the cache at `path` and save it: load (tolerantly), merge, prune
/// against `listing` when given, save atomically. Held under an exclusive `flock` on the
/// state directory, so a `pods test --deep` and an `up --check` finishing together can't
/// drop each other's records (the atomic rename alone only stops torn files). `Ok` carries
/// the load warning, if the old file had to be ignored.
pub fn record_health(
    path: &Path,
    results: &[PodHealth],
    listing: Option<&Listing>,
    now: u64,
) -> std::io::Result<Option<String>> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    // Best effort: a filesystem without flock still gets the atomic write.
    let lock = std::fs::File::open(dir).ok().filter(|f| f.lock().is_ok());
    let (mut cache, warning) = HealthCache::load(path);
    cache.merge(results, now);
    if let Some(listing) = listing {
        cache.prune(listing);
    }
    let saved = cache.save(path);
    drop(lock);
    saved.map(|()| warning)
}

/// Replace `path` with `bytes` so a reader (a web server serving `fleet.json`, a
/// `snapshot` reading the cache) sees the old file or the new one, never a half-written
/// one: write a hidden temp sibling (same directory, so the rename is atomic), flush it to
/// disk, rename it over. The file gets exactly `mode`, whatever the umask. The temp file is
/// removed if anything fails.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name to write"))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = dir.join(format!(".{}.tmp-{}-{nanos}", name.to_string_lossy(), std::process::id()));
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts.open(&tmp)?;
        // The open mode above is masked by the umask — under `umask 077` (a hardened cron)
        // a public fleet.json would land 0600 and the web server would get 403s. fchmod
        // isn't masked, so the file gets exactly `mode`.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

// ---------------------------------------------------------------------------------------
// The internal snapshot
// ---------------------------------------------------------------------------------------

/// Whether the proxy forwards a pod's stable port to where the pod is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyState {
    /// The proxy config forwards this name to the pod's current SSH endpoint.
    Live,
    /// It forwards the name somewhere else (an old endpoint, a twin pod), or the pod has
    /// no endpoint right now — the sticky merge keeps such entries; `proxy apply` fixes it.
    Stale,
    /// No forward for this name.
    None,
    /// The proxy config wasn't read (a remote proxy — `snapshot` never SSHes — or the
    /// local file couldn't be read).
    Unknown,
}

/// One pod in the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotPod {
    pub pod: Pod,
    /// The stable public port the proxy config gives this name, if it has a forward.
    pub proxy_port: Option<u16>,
    pub proxy: ProxyState,
    /// The last cached deep check of *this* pod (joined by provider + id, so a recreated
    /// pod under the same name doesn't inherit its predecessor's verdict).
    pub health: Option<HealthRecord>,
    /// The `MACHINE_NAME_LIST` entry this pod's name is (`apple`, or `@james-gpu` for an
    /// absolute entry); `None` for a pod that isn't on the list.
    pub list_entry: Option<String>,
    pub in_name_list: bool,
    /// Whether the pod's SSH port answered ([`probe_reachability`]): `Some(true)` an SSH
    /// server greeted us, `Some(false)` it didn't within the probe's few seconds (refused,
    /// silent, closed on us), `None` not probed — not a cohort machine, no endpoint, not
    /// billing, or `--no-probe` ([`reach_targets`]).
    /// [`build`] leaves it `None`; the probe fills it in.
    pub reachable: Option<bool>,
}

/// The whole fleet at one moment. See the module docs. (`Default` = the empty fleet at the
/// epoch: what the dashboard shows before its first listing lands.)
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FleetSnapshot {
    /// When it was built (Unix seconds), and the same as RFC 3339 UTC.
    pub generated_at: u64,
    pub generated_at_rfc3339: String,
    /// Every listed pod, by provider then name (as `pods list`).
    pub pods: Vec<SnapshotPod>,
    pub cost: FleetCost,
    /// Providers that failed to list: their pods are missing from `pods` (and `cost`).
    pub partial: Vec<String>,
}

/// The MACHINE_NAME_LIST entry whose qualified name is `name` (first one, like the proxy's
/// port slots).
fn list_entry(naming: &Naming, name: &str) -> Option<String> {
    naming.list.iter().find(|e| qualify(naming.prefix, e) == name).cloned()
}

/// A pod's SSH endpoint as `(host, port)`, when it has a usable one.
fn endpoint(pod: &Pod) -> Option<(&str, u16)> {
    let ip = pod.ssh_ip.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    pod.ssh_port.filter(|&p| p != 0).map(|p| (ip, p))
}

/// Build the snapshot. Pure: `pods` are what the providers listed (best-effort details
/// already filled), `partial` the providers that failed to list, `proxy_file` the proxy
/// config text (`Some("")` = no file yet → every pod `None`; `None` = not read → every pod
/// `Unknown`), `health` the cache, `naming` the prefix + `MACHINE_NAME_LIST`, `now` the
/// clock.
pub fn build(
    pods: &[Pod],
    partial: &[String],
    proxy_file: Option<&str>,
    health: &HealthCache,
    naming: &Naming,
    now: u64,
) -> FleetSnapshot {
    let forwards = proxy_file.map(parse_nginx);
    let mut sorted: Vec<&Pod> = pods.iter().collect();
    sorted.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
    let pods_out = sorted
        .into_iter()
        .map(|pod| {
            let (proxy_port, proxy) = match &forwards {
                None => (None, ProxyState::Unknown),
                Some(fs) => match fs.iter().find(|f| f.name == pod.name) {
                    None => (None, ProxyState::None),
                    Some(f) => {
                        let live = endpoint(pod).is_some_and(|(host, port)| {
                            host.eq_ignore_ascii_case(f.target_ip.trim()) && port == f.target_port
                        });
                        (Some(f.public_port), if live { ProxyState::Live } else { ProxyState::Stale })
                    }
                },
            };
            let entry = list_entry(naming, &pod.name);
            SnapshotPod {
                pod: pod.clone(),
                proxy_port,
                proxy,
                health: health.get(pod).cloned(),
                in_name_list: entry.is_some(),
                list_entry: entry,
                reachable: None,
            }
        })
        .collect();
    FleetSnapshot {
        generated_at: now,
        generated_at_rfc3339: rfc3339(now),
        pods: pods_out,
        cost: fleet::fleet_cost(pods),
        partial: partial.to_vec(),
    }
}

// ---------------------------------------------------------------------------------------
// Reachability
// ---------------------------------------------------------------------------------------

/// How the snapshot asks "does this pod's SSH port answer?" — a seam, like `Remote`, so
/// tests (here and the CLI's) script the answers and never dial out. [`SshPortProbe`] is
/// the real one.
#[async_trait]
pub trait Reach: Send + Sync {
    async fn answers(&self, host: &str, port: u16) -> bool;
}

/// How long [`SshPortProbe`] waits for a pod: the TCP connect plus the server's greeting.
/// A healthy sshd greets within milliseconds of the connect; 3 seconds also covers a pod
/// across an ocean or a busy host, and keeps a snapshot of a fleet with dead pods (every
/// probe runs at once) at a few seconds.
pub const REACH_TIMEOUT: Duration = Duration::from_secs(3);

/// The most a probe reads while looking for the greeting. RFC 4253 lets a server send other
/// lines before its `SSH-` line; anything that hasn't greeted within this much isn't sshd.
const GREETING_MAX: usize = 1024;

/// The real [`Reach`]: a TCP connect to the pod's SSH endpoint, then wait for the server's
/// identification line (`SSH-2.0-…`), which sshd sends unprompted the moment a client
/// connects. We send **nothing** — no version string, no key exchange, no auth, no command
/// — and close at once. Reading the greeting rather than trusting the connect alone matters
/// wherever something other than the pod's sshd may accept the TCP connection (a provider's
/// SSH relay port, a host-side port mapping whose container is down): an accept followed by
/// a close or by silence is not a pod anyone can log in to. sshd logs each probe as a
/// pre-auth disconnect — one per pod per snapshot (every 2 minutes under the cron).
#[derive(Debug, Clone, Copy)]
pub struct SshPortProbe {
    pub timeout: Duration,
}

impl Default for SshPortProbe {
    fn default() -> Self {
        Self { timeout: REACH_TIMEOUT }
    }
}

#[async_trait]
impl Reach for SshPortProbe {
    async fn answers(&self, host: &str, port: u16) -> bool {
        use tokio::io::AsyncReadExt;
        // An IPv6 literal may come bracketed; the socket API wants it bare.
        let host = host.trim().trim_start_matches('[').trim_end_matches(']').to_string();
        let greet = async move {
            let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await.ok()?;
            let mut buf = Vec::with_capacity(256);
            let mut chunk = [0u8; 256];
            while buf.len() < GREETING_MAX {
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None; // closed without greeting
                }
                buf.extend_from_slice(&chunk[..n]);
                if greets(&buf) {
                    return Some(());
                }
            }
            None
        };
        matches!(tokio::time::timeout(self.timeout, greet).await, Ok(Some(())))
    }
}

/// Whether `received` holds an SSH identification line: a line starting `SSH-` (at the very
/// start, or after a pre-banner line). Its first four bytes are enough — the rest of the
/// line may still be in flight.
fn greets(received: &[u8]) -> bool {
    received.starts_with(b"SSH-") || received.windows(5).any(|w| w == b"\nSSH-")
}

/// Which pods the probe dials: the cohort's machines — pods on a *prefixed*
/// `MACHINE_NAME_LIST` entry, exactly the ones the public page can show — that bill
/// ([`is_billing`]) and have an SSH endpoint; by index into `snap.pods` (two pods can share
/// a name, so never by name) with that endpoint. Everything else stays `None`: a stopped
/// pod's endpoint is stale, one without an endpoint has nothing to dial, and staff boxes
/// (`@` entries) and pods off the list are never dialed at all — the probe exists to gate
/// the public `up`, and it has no business knocking on machines that aren't the cohort's.
/// Pure.
pub fn reach_targets(snap: &FleetSnapshot) -> Vec<(usize, String, u16)> {
    snap.pods
        .iter()
        .enumerate()
        .filter(|(_, p)| p.list_entry.as_deref().is_some_and(|e| !is_absolute(e)))
        .filter(|(_, p)| is_billing(&p.pod.status))
        .filter_map(|(i, p)| endpoint(&p.pod).map(|(host, port)| (i, host.to_string(), port)))
        .collect()
}

/// Probe every [`reach_targets`] pod at once over `reach` and record the answers in
/// [`SnapshotPod::reachable`]; the rest stay `None`. As long as the slowest probe — with
/// [`SshPortProbe`], at most its timeout — however many pods are dead. Read-only: a
/// connect and a read, nothing sent. A probe task that panicked leaves its pod `None`
/// (unknown), never "unreachable".
pub async fn probe_reachability(snap: &mut FleetSnapshot, reach: Arc<dyn Reach>) {
    let mut set = tokio::task::JoinSet::new();
    for (i, host, port) in reach_targets(snap) {
        let reach = reach.clone();
        set.spawn(async move { (i, reach.answers(&host, port).await) });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((i, answered)) = joined {
            if let Some(p) = snap.pods.get_mut(i) {
                p.reachable = Some(answered);
            }
        }
    }
}

/// The SSH column of `arena snapshot`'s table: `ok` (answered), `down` (probed, no answer),
/// `-` (not probed).
pub fn reach_label(reachable: Option<bool>) -> &'static str {
    match reachable {
        Some(true) => "ok",
        Some(false) => "down",
        None => "-",
    }
}

/// The proxy config text [`build`] judges forwards by — never fetched over SSH, so it is
/// safe on every refresh of a dashboard: the local file when the proxy is local
/// (`Some("")` when it doesn't exist yet), `Some("")` with no proxy configured at all
/// (there are no forwards), `None` = unknown for a remote proxy or an unreadable file
/// (with a warning line for the latter). Shared by `arena snapshot` and the TUI.
pub fn local_proxy_text(cfg: &Config) -> (Option<String>, Option<String>) {
    let Ok(px) = crate::proxy::ProxyConfig::from_config(cfg) else {
        return (Some(String::new()), None);
    };
    if !px.local {
        return (None, None);
    }
    let path = expand_home(&px.nginx_path);
    match std::fs::read_to_string(&path) {
        Ok(text) => (Some(text), None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Some(String::new()), None),
        Err(e) => (None, Some(format!("warning: can't read the proxy config {path} ({e}) — PROXY shows `?`"))),
    }
}

/// `~/x` → `$HOME/x` (the config's default proxy path is `~/proxy.conf`); anything else,
/// or no `HOME`, as given — the same rule the CLI applies to the paths it writes.
fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(h)) => format!("{}/{rest}", h.trim_end_matches('/')),
        _ => p.to_string(),
    }
}

/// The PROXY column: `:9500` (live), `:9500 stale`, `-` (no forward), `?` (not read).
pub fn proxy_label(p: &SnapshotPod) -> String {
    match (p.proxy, p.proxy_port) {
        (ProxyState::Live, Some(port)) => format!(":{port}"),
        (ProxyState::Stale, Some(port)) => format!(":{port} stale"),
        (ProxyState::Unknown, _) => "?".into(),
        _ => "-".into(),
    }
}

/// The HEALTH column: `pass 12m`, `fail 2h GPU error` (the worst issue), or `-` (never
/// checked). Ages are relative to the snapshot's `now`.
pub fn health_label(h: Option<&HealthRecord>, now: u64) -> String {
    let Some(h) = h else { return "-".into() };
    let mut out = format!("{} {}", h.status.label(), fmt_age(now.saturating_sub(h.checked_at)));
    if h.status != Status::Pass {
        if let Some(i) = h.issues.first() {
            out.push(' ');
            out.push_str(i.label());
        }
    }
    out
}

/// `arena snapshot`'s table: `pods list`'s columns with SSH ([`reach_label`]), PROXY and
/// HEALTH before MAINT, then the fleet cost footer and, for a partial listing, which
/// providers are missing.
pub fn render_table(snap: &FleetSnapshot) -> String {
    if snap.pods.is_empty() && snap.partial.is_empty() {
        return "(no pods)\n".into();
    }
    let rows: Vec<Vec<String>> = snap
        .pods
        .iter()
        .map(|p| {
            let mut cells = fleet::pod_cells(&p.pod);
            let maint = cells.pop().unwrap_or_default();
            cells.push(reach_label(p.reachable).to_string());
            cells.push(proxy_label(p));
            cells.push(health_label(p.health.as_ref(), snap.generated_at));
            cells.push(maint);
            cells
        })
        .collect();
    let mut headers: Vec<&str> = fleet::POD_HEADERS.to_vec();
    let mut align: Vec<Align> = fleet::POD_ALIGN.to_vec();
    let (maint_h, maint_a) = (headers.pop().unwrap_or("MAINT"), align.pop().unwrap_or(Align::Left));
    headers.extend(["SSH", "PROXY", "HEALTH", maint_h]);
    align.extend([Align::Left, Align::Left, Align::Left, maint_a]);
    let mut out = table::render(&headers, &align, &rows);
    out.push_str(&fleet::fleet_footer(&snap.cost));
    out.push('\n');
    if !snap.partial.is_empty() {
        out.push_str(&format!(
            "partial: {} failed to list — its pods are missing above\n",
            snap.partial.join(", ")
        ));
    }
    out
}

// ---------------------------------------------------------------------------------------
// The public snapshot (allowlist)
// ---------------------------------------------------------------------------------------

/// What `arena snapshot --public` emits, and all it can emit. Safe to serve with no auth.
/// `deny_unknown_fields` + the schema test pin the field set: widening it is a deliberate,
/// reviewed change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicSnapshot {
    /// When the snapshot was taken (RFC 3339 UTC). The page shows its age and flags a stale
    /// file, so a dead cron is visible.
    pub updated_at: String,
    /// `false` when a provider failed to list: machines may be missing (the page says so).
    /// No provider names — just the fact.
    pub complete: bool,
    pub machines: Vec<PublicMachine>,
}

/// One machine on the public page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMachine {
    /// The `MACHINE_NAME_LIST` entry (`apple`), without the fleet prefix — the name
    /// participants are told, and unique within the list; the prefix is an internal tag.
    pub name: String,
    /// `count×type` (`1×RTX A4000`), or `-`.
    pub gpu: String,
    pub status: PublicStatus,
    pub health: PublicHealth,
    /// The host's maintenance window, times only (the provider's note is free text).
    pub maintenance: Option<PublicMaintenance>,
}

/// Up / coming up / not usable — the page's whole status vocabulary, kept to these three
/// (`web/fleet.html` documents and styles exactly them; a new value is a deliberate change
/// to both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicStatus {
    /// Running, with an SSH endpoint whose sshd answered the snapshot's probe (with
    /// `--no-probe`: any endpoint) — someone can log in.
    Up,
    /// Billing but not answering yet: provisioning, booting, no endpoint yet, or listed
    /// running while its SSH port doesn't answer (usually sshd still starting; rarely a
    /// wedged host — the health column says more once a deep check has run).
    Starting,
    /// Stopped, exited, or in an error state.
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicHealthStatus {
    Pass,
    Warn,
    Fail,
    /// Never deep-checked (or the cache had no record of this pod).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicHealth {
    pub status: PublicHealthStatus,
    /// When the deep check ran (RFC 3339 UTC).
    pub checked_at: Option<String>,
    /// How old the check was when the snapshot was taken.
    pub age_secs: Option<u64>,
    /// The worst issue, for warn/fail — only ever a fixed [`Issue`] label.
    pub reason: Option<Issue>,
}

/// A maintenance window's ends, each RFC 3339 UTC re-printed from the parsed instant
/// ([`public_time`]) — `None` when the provider's value wasn't a complete timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMaintenance {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// `up` / `starting` / `down` from the provider status, the endpoint and the probe: down
/// unless billing ([`is_billing`]) — and `ERROR`, which bills but isn't usable, is down
/// too; up only when `RUNNING` with an SSH endpoint that didn't fail the probe
/// (`reachable` is `Some(true)`, or `None` when nothing was probed); anything else billing
/// is starting. A listed-running pod whose port is silent reads `starting`, not a fourth
/// word: the public vocabulary stays the three the page knows.
pub fn public_status(pod: &Pod, reachable: Option<bool>) -> PublicStatus {
    let s = pod.status.trim().to_ascii_uppercase();
    if !is_billing(&s) || s == "ERROR" {
        PublicStatus::Down
    } else if s == "RUNNING" && endpoint(pod).is_some() && reachable != Some(false) {
        PublicStatus::Up
    } else {
        PublicStatus::Starting
    }
}

/// A list entry fit to publish as a name: `[A-Za-z0-9._-]`, at most 63 chars.
fn public_name(entry: &str) -> Option<String> {
    let ok = !entry.is_empty()
        && entry.len() <= 63
        && entry.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then(|| entry.to_string())
}

/// Every IPv4/IPv6 literal in `s`, wherever it sits — `host:port`, a URL, brackets, the
/// middle of a word. Guards untrusted text on its way to the public view (and proves, in the
/// leak test, that the public JSON holds none). IPv4: any four consecutive dotted groups of
/// 0–255 in a run of digits and dots. IPv6: a run of hex digits, colons and dots with at
/// least two colons and a digit that parses as one — so `12:00:00` (a time) never matches.
pub(crate) fn ip_literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for run in s.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let groups: Vec<&str> = run.split('.').collect();
        for w in groups.windows(4) {
            if w.iter().all(|g| !g.is_empty() && g.len() <= 3 && g.parse::<u8>().is_ok()) {
                out.push(w.join("."));
            }
        }
    }
    for run in s.split(|c: char| !(c.is_ascii_hexdigit() || c == ':' || c == '.')) {
        let t = run.trim_matches('.');
        if t.matches(':').count() >= 2
            && t.chars().any(|c| c.is_ascii_hexdigit())
            && t.parse::<std::net::Ipv6Addr>().is_ok()
        {
            out.push(t.to_string());
        }
    }
    out
}

/// A GPU label fit to publish, else `-`. It comes from a provider API (or the pod), so it's
/// untrusted text: kept only if it is short, made of `[A-Za-z0-9 ×+._()-]` and holds no IP
/// literal — dropped whole otherwise, never "cleaned" into something half-meaningful.
fn public_gpu(label: &str) -> String {
    let s = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let ok = !s.is_empty()
        && s.chars().count() <= 40
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '×' | '+' | '.' | '_' | '(' | ')' | '-'))
        && ip_literals(&s).is_empty();
    if ok {
        s
    } else {
        "-".into()
    }
}

/// A maintenance timestamp fit to publish, **rebuilt, never copied**: the value comes from a
/// provider API whose type isn't pinned down (`runpod::loose_time` passes any non-numeric
/// string through), so a shape check on its first characters would let whatever follows
/// them (`2026-10-09T02:00 203.0.113.7:10022`) onto the page. Instead it must be one
/// complete RFC 3339 date-time ([`parse_rfc3339`]) — and hold no IP literal, as a second
/// guard — and what's published is that instant re-printed by [`rfc3339`] (UTC, `Z`), so
/// only digits the parser produced can reach the page. Anything else — no zone (the
/// browser would guess local time), an impossible date, trailing text — is dropped.
fn public_time(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.len() > 40 || !ip_literals(s).is_empty() {
        return None;
    }
    parse_rfc3339(s).map(rfc3339)
}

/// Unix seconds for a complete RFC 3339 date-time: `YYYY-MM-DD`, `T` (or `t` / a space),
/// `HH:MM`, optional `:SS` and `.fraction` (dropped), then `Z` or `±HH:MM` — and nothing
/// after it. Field ranges are checked (no Feb 30, no 24:00); a leap second `:60` is read
/// as the next minute. `None` for anything else, including an instant before 1970 (which
/// [`rfc3339`] can't print).
fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let d = b.get(r)?;
        d.iter().all(u8::is_ascii_digit).then(|| d.iter().fold(0, |n, c| n * 10 + i64::from(c - b'0')))
    };
    let is = |i: usize, set: &[u8]| b.get(i).is_some_and(|c| set.contains(c));
    if !(is(4, b"-") && is(7, b"-") && is(10, b"Tt ") && is(13, b":")) {
        return None;
    }
    let (year, month, day, hour, minute) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?);
    let (mut i, mut second) = (16, 0);
    if is(i, b":") {
        second = num(i + 1..i + 3)?;
        i += 3;
        if is(i, b".") {
            let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            i += 1 + digits;
        }
    }
    let offset = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) if i + 6 == b.len() && is(i + 3, b":") => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3_600 + om * 60;
            if sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return None,
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // In range by the checks above (month 1–12, day 1–31), so the casts are exact.
    let days = crate::schedule::days_from_civil(year, month as u32, day as u32);
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second - offset).ok()
}

fn public_health(h: Option<&HealthRecord>, now: u64) -> PublicHealth {
    let Some(h) = h else {
        return PublicHealth { status: PublicHealthStatus::Unknown, checked_at: None, age_secs: None, reason: None };
    };
    let status = match h.status {
        Status::Pass => PublicHealthStatus::Pass,
        Status::Warn => PublicHealthStatus::Warn,
        Status::Fail => PublicHealthStatus::Fail,
        Status::Skip => PublicHealthStatus::Unknown,
    };
    let reason = match status {
        PublicHealthStatus::Warn | PublicHealthStatus::Fail => Some(h.issues.first().copied().unwrap_or(Issue::Other)),
        _ => None,
    };
    PublicHealth {
        status,
        checked_at: Some(rfc3339(h.checked_at)),
        age_secs: Some(now.saturating_sub(h.checked_at)),
        reason,
    }
}

/// The public view of a snapshot. Only pods whose name is a **prefixed** `MACHINE_NAME_LIST`
/// entry appear: off-list pods (`james-gpu`, `registered_pink_prawn`, …) never do, and
/// neither do absolute (`@name`) entries — those are the personal/staff boxes sharing the
/// list (see `naming`), not cohort machines. A name held by two pods (a double create) is
/// shown once, by its most usable pod: up, then starting, then down; then the one the proxy
/// forwards to (the twin participants actually reach — never one borrowing its twin's
/// PASS); then the one with a health record. Ordered by list position.
pub fn public_snapshot(snap: &FleetSnapshot, naming: &Naming) -> PublicSnapshot {
    let rank = |s: PublicStatus| match s {
        PublicStatus::Up => 0,
        PublicStatus::Starting => 1,
        PublicStatus::Down => 2,
    };
    let mut best: BTreeMap<usize, (PublicMachine, (u8, bool, bool))> = BTreeMap::new();
    for p in &snap.pods {
        let Some(entry) = p.list_entry.as_deref().filter(|e| !is_absolute(e)) else { continue };
        let Some(name) = public_name(entry) else { continue };
        // The slot orders the page; the entry must still qualify to this pod's name now.
        let Some(slot) = naming.list.iter().position(|e| e == entry && qualify(naming.prefix, e) == p.pod.name) else {
            continue;
        };
        let status = public_status(&p.pod, p.reachable);
        let gpu = match fleet::gpu_label(&p.pod) {
            g if g != "-" => g,
            _ => p.health.as_ref().and_then(|h| h.gpu.clone()).unwrap_or_else(|| "-".into()),
        };
        let maintenance = p.pod.maintenance.as_ref().filter(|m| fleet::maintenance_label(Some(m)) != "-").map(|m| {
            PublicMaintenance { start: public_time(m.start.as_deref()), end: public_time(m.end.as_deref()) }
        });
        let machine = PublicMachine {
            name,
            gpu: public_gpu(&gpu),
            status,
            health: public_health(p.health.as_ref(), snap.generated_at),
            maintenance,
        };
        let key = (rank(status), p.proxy != ProxyState::Live, p.health.is_none());
        match best.get(&slot) {
            Some((_, have)) if *have <= key => {}
            _ => {
                best.insert(slot, (machine, key));
            }
        }
    }
    PublicSnapshot {
        updated_at: snap.generated_at_rfc3339.clone(),
        complete: snap.partial.is_empty(),
        machines: best.into_values().map(|(m, _)| m).collect(),
    }
}

/// Whether publishing `next` over the file `previous` would blank the page because of an
/// outage: `next` is partial (a provider failed to list) and shows no machine, while
/// `previous` — the public JSON written last time — showed some. That is the provider
/// holding every machine failing while another configured one answers empty: published, the
/// page would read "No machines." for as long as the outage lasts. Keeping the old file
/// instead lets the page go visibly stale (its banner), as when no provider answers at all.
/// A partial snapshot that still shows machines is published (flagged `complete: false`),
/// and a complete empty one — a fleet really torn down — always is.
pub fn would_blank_the_page(previous: &str, next: &PublicSnapshot) -> bool {
    !next.complete
        && next.machines.is_empty()
        && serde_json::from_str::<PublicSnapshot>(previous).is_ok_and(|p| !p.machines.is_empty())
}

/// [`public_snapshot`] as the JSON written to `fleet.json`.
pub fn public_json(public: &PublicSnapshot) -> String {
    let mut s = serde_json::to_string_pretty(public).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{Check, DeepFacts, GpuFact};
    use crate::pod::Maintenance;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z

    fn pod(name: &str, provider: &str, id: &str) -> Pod {
        Pod {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            ..Default::default()
        }
    }

    fn at(mut p: Pod, ip: &str, port: u16) -> Pod {
        p.ssh_ip = Some(ip.into());
        p.ssh_port = Some(port);
        p
    }

    fn check(name: &str, status: Status, detail: &str) -> Check {
        Check { name: name.into(), status, detail: detail.into() }
    }

    fn health(p: &Pod, status: Status, checks: Vec<Check>) -> PodHealth {
        PodHealth {
            id: p.id.clone(),
            name: p.name.clone(),
            provider: p.provider.clone(),
            status,
            checks,
            facts: None,
            host: None,
        }
    }

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(\n \"apple\"\n \"bloom\"\n \"cloud\"\n \"@james-gpu\"\n)\n")
    }

    /// A proxy config with forwards for apple (to its endpoint) and bloom (to an old one).
    fn proxy_text() -> String {
        use crate::proxy::{render_nginx, Forward};
        let fwd = |name: &str, port: u16, ip: &str, tport: u16| Forward {
            name: name.into(),
            public_port: port,
            target_ip: ip.into(),
            target_port: tport,
            provider: Some("runpod".into()),
            pod_id: None,
        };
        render_nginx(&[
            fwd("devtest-apple", 9500, "10.0.0.1", 22001),
            fwd("devtest-bloom", 9501, "10.0.0.9", 22999),
            fwd("devtest-cloud", 9502, "2001:db8::7", 2222),
        ])
    }

    #[test]
    fn rfc3339_and_ages() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(NOW), "2026-10-08T12:00:00Z");
        assert_eq!(rfc3339(NOW + 86_399), "2026-10-09T11:59:59Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        for (secs, want) in [(0, "0s"), (59, "59s"), (60, "1m"), (3_599, "59m"), (3_600, "1h"), (86_400, "1d")] {
            assert_eq!(fmt_age(secs), want);
        }
    }

    #[test]
    fn issue_from_check_names_table() {
        use Status::*;
        let cases = [
            ("ssh", Fail, Issue::Unreachable),
            ("script", Fail, Issue::CheckIncomplete),
            ("script", Warn, Issue::CheckIncomplete),
            ("driver", Fail, Issue::DriverTooOld),
            ("driver", Warn, Issue::DriverUnknown),
            ("nvidia-smi", Fail, Issue::GpuError),
            ("cuda", Fail, Issue::GpuError),
            ("device_count", Fail, Issue::GpuError),
            ("gpu0", Fail, Issue::GpuError),
            ("gpu12", Fail, Issue::GpuError),
            ("peer_copy", Fail, Issue::GpuError),
            ("nccl", Fail, Issue::GpuError),
            ("provider", Warn, Issue::GpuError),
            ("torch", Fail, Issue::SoftwareError),
            ("network", Warn, Issue::SlowNetwork),
            ("disk", Warn, Issue::LowDisk),
            ("load", Warn, Issue::BusyHost),
            ("maintenance", Warn, Issue::MaintenanceScheduled),
            ("gpu", Skip, Issue::Other),
            ("gpux", Fail, Issue::Other),
            ("brand-new-check", Fail, Issue::Other),
        ];
        for (name, status, want) in cases {
            assert_eq!(Issue::from_check(&check(name, status, "x")), want, "{name}");
        }
        // The JSON form is the label, both ways; anything else reads back as Other.
        assert_eq!(serde_json::to_string(&Issue::BusyHost).unwrap(), "\"busy host\"");
        assert_eq!(serde_json::from_str::<Issue>("\"low disk\"").unwrap(), Issue::LowDisk);
        assert_eq!(serde_json::from_str::<Issue>("\"host 10.0.0.1 is on fire\"").unwrap(), Issue::Other);
    }

    #[test]
    fn health_record_orders_fails_first_and_dedupes_issues() {
        let p = pod("devtest-apple", "runpod", "rp1");
        let mut h = health(
            &p,
            Status::Fail,
            vec![
                check("nvidia-smi", Status::Pass, "1×RTX A4000"),
                check("network", Status::Warn, "0.4 MB/s from huggingface.co\n(< 2 MB/s)"),
                check("cuda", Status::Fail, "RuntimeError: Error 999"),
                check("gpu0", Status::Fail, "no result"),
                check("maintenance", Status::Warn, "maint 10-09 02:00→06:00 UTC"),
            ],
        );
        h.facts = Some(DeepFacts {
            gpus: vec![GpuFact { index: 0, name: Some("NVIDIA RTX A4000".into()), ..Default::default() }],
            ..Default::default()
        });
        let r = HealthRecord::from_health(&h, NOW);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.checked_at, NOW);
        assert_eq!(
            r.reasons,
            [
                "cuda: RuntimeError: Error 999",
                "gpu0: no result",
                "network: 0.4 MB/s from huggingface.co (< 2 MB/s)", // flattened onto one line
                "maintenance: maint 10-09 02:00→06:00 UTC",
            ]
        );
        assert_eq!(r.issues, [Issue::GpuError, Issue::SlowNetwork, Issue::MaintenanceScheduled]);
        assert_eq!(r.gpu.as_deref(), Some("1×RTX A4000"));
        // A clean pass has no reasons; an unreachable pod no GPU.
        let ok = HealthRecord::from_health(&health(&p, Status::Pass, vec![check("cuda", Status::Pass, "ok")]), NOW);
        assert!(ok.reasons.is_empty() && ok.issues.is_empty() && ok.gpu.is_none());
        // A runaway detail is clipped.
        let long = HealthRecord::from_health(&health(&p, Status::Fail, vec![check("ssh", Status::Fail, &"x".repeat(5000))]), NOW);
        assert_eq!(long.reasons[0].chars().count(), REASON_MAX);
    }

    #[test]
    fn cache_path_resolution_table() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| {
            pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
        };
        let cases: &[(&[(&str, &str)], &str, Result<&str, &str>)] = &[
            (&[("HOME", "/home/dev")], "devtest", Ok("/home/dev/.local/state/arena/devtest/health.json")),
            (&[("HOME", "/root"), ("XDG_STATE_HOME", "/var/state")], "arena9", Ok("/var/state/arena/arena9/health.json")),
            // A relative XDG_STATE_HOME is ignored (XDG spec).
            (&[("HOME", "/root"), ("XDG_STATE_HOME", "state")], "arena9", Ok("/root/.local/state/arena/arena9/health.json")),
            // ARENA_STATE_DIR wins, and is still prefix-scoped.
            (&[("HOME", "/root"), ("XDG_STATE_HOME", "/x"), ("ARENA_STATE_DIR", "/srv/arena")], "arena9", Ok("/srv/arena/arena9/health.json")),
            (&[("ARENA_STATE_DIR", "  ")], "p", Err("no ARENA_STATE_DIR")),
            (&[("ARENA_STATE_DIR", "rel/dir")], "p", Err("must be an absolute path")),
            (&[], "p", Err("no ARENA_STATE_DIR")),
            // A prefix can't climb out of the state dir.
            (&[("ARENA_STATE_DIR", "/s")], "../../etc", Ok("/s/.._.._etc/health.json")),
            (&[("ARENA_STATE_DIR", "/s")], "..", Ok("/s/_/health.json")),
            (&[("ARENA_STATE_DIR", "/s")], "", Ok("/s/_/health.json")),
        ];
        for (vars, prefix, want) in cases {
            let got = health_cache_path(prefix, env(vars));
            match (want, &got) {
                (Ok(p), Ok(g)) => assert_eq!(g, Path::new(p), "{vars:?} {prefix}"),
                (Err(needle), Err(e)) => assert!(e.contains(needle), "{e}"),
                _ => panic!("{vars:?} {prefix}: got {got:?}"),
            }
        }
    }

    /// A fresh temp directory, removed at the end of the test.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-snapshot-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    #[test]
    fn health_cache_round_trips_through_an_atomic_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("cache");
        let path = dir.0.join("state").join("devtest").join(HEALTH_CACHE_FILE);
        let apple = pod("devtest-apple", "runpod", "rp1");
        let bloom = pod("devtest-bloom", "vast", "31415926");
        let results = vec![
            health(&apple, Status::Warn, vec![check("disk", Status::Warn, "/ 3.2 GB free (< 10 GB)")]),
            health(&bloom, Status::Fail, vec![check("ssh", Status::Fail, "exit 255: connect to 10.0.0.2 refused")]),
        ];
        let warn = record_health(&path, &results, None, NOW).unwrap();
        assert_eq!(warn, None, "no file yet is not a warning");

        let (back, warn) = HealthCache::load(&path);
        assert_eq!(warn, None);
        assert_eq!(back.version, HEALTH_CACHE_VERSION);
        assert_eq!(back.pods.len(), 2);
        let b = back.get(&bloom).unwrap();
        assert_eq!((b.status, b.issues.as_slice(), b.checked_at), (Status::Fail, &[Issue::Unreachable][..], NOW));
        assert_eq!(back.get(&apple).unwrap().issues, [Issue::LowDisk]);
        // Same id on another provider is another pod.
        assert!(back.get(&pod("devtest-bloom", "runpod", "31415926")).is_none());

        // Owner-only file in an owner-only dir, and no temp file left behind.
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [HEALTH_CACHE_FILE]);

        // A later check replaces that pod's record and keeps the other.
        let again = vec![health(&apple, Status::Pass, vec![])];
        record_health(&path, &again, None, NOW + 60).unwrap();
        let (back, _) = HealthCache::load(&path);
        assert_eq!(back.get(&apple).unwrap().status, Status::Pass);
        assert_eq!(back.get(&apple).unwrap().checked_at, NOW + 60);
        assert_eq!(back.get(&bloom).unwrap().status, Status::Fail);
    }

    #[test]
    fn a_corrupt_or_foreign_cache_loads_empty_with_one_warning_and_is_replaced() {
        let dir = tmp("corrupt");
        let path = dir.0.join(HEALTH_CACHE_FILE);
        for junk in ["{not json", "[]", "{\"version\": 2, \"pods\": {}}", ""] {
            std::fs::write(&path, junk).unwrap();
            let (c, warn) = HealthCache::load(&path);
            assert!(c.pods.is_empty(), "{junk}");
            let warn = warn.unwrap_or_else(|| panic!("{junk:?} must warn"));
            assert!(warn.starts_with("warning: ignoring the health cache"), "{warn}");
        }
        // The next writer reports the warning and replaces the file with a good one.
        let apple = pod("devtest-apple", "runpod", "rp1");
        let warn = record_health(&path, &[health(&apple, Status::Pass, vec![])], None, NOW).unwrap();
        assert!(warn.is_some());
        let (c, warn) = HealthCache::load(&path);
        assert_eq!((c.pods.len(), warn), (1, None));
        // A directory where the file should be: unreadable, still just a warning.
        let (c, warn) = HealthCache::load(&dir.0);
        assert!(c.pods.is_empty() && warn.unwrap().contains("can't read"));
    }

    #[test]
    fn write_atomic_replaces_whole_files_and_cleans_up_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("atomic");
        let path = dir.0.join("fleet.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new", 0o644).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        // Into a missing directory: an error, and nothing stray anywhere.
        assert!(write_atomic(&dir.0.join("nope").join("fleet.json"), b"x", 0o644).is_err());
        // Over a directory: the rename fails, the temp file is removed.
        std::fs::create_dir(dir.0.join("taken")).unwrap();
        assert!(write_atomic(&dir.0.join("taken"), b"x", 0o644).is_err());
        let mut names: Vec<String> =
            std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["fleet.json", "taken"]);
    }

    /// The mode is exact whatever the umask: a cron under `umask 077` must still publish a
    /// world-readable fleet.json. The umask is process-wide — changing it here would race
    /// every other test thread — so this re-runs itself in a child copy of the test binary
    /// started under `umask 077`, which writes the files this parent then checks.
    #[test]
    fn write_atomic_sets_the_mode_even_under_a_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "ARENA_TEST_WRITE_ATOMIC_UMASK_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            std::fs::write(dir.join("control"), "x").unwrap();
            write_atomic(&dir.join("fleet.json"), b"{}", 0o644).unwrap();
            write_atomic(&dir.join("health.json"), b"{}", 0o600).unwrap();
            return;
        }
        let dir = tmp("umask");
        let out = std::process::Command::new("sh")
            .args([
                "-c",
                "umask 077 && exec \"$0\" --exact --test-threads=1 \
                 snapshot::tests::write_atomic_sets_the_mode_even_under_a_restrictive_umask",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(CHILD, &dir.0)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let mode = |n: &str| std::fs::metadata(dir.0.join(n)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("control"), 0o600, "the child really ran under umask 077");
        assert_eq!(mode("fleet.json"), 0o644);
        assert_eq!(mode("health.json"), 0o600);
    }

    #[test]
    fn prune_drops_only_pods_their_own_provider_confirmed_gone() {
        use crate::error::Error;
        let mut cache = HealthCache::new();
        let gone = pod("devtest-apple", "runpod", "rp-gone");
        let kept = pod("devtest-bloom", "runpod", "rp-here");
        let vast = pod("devtest-cloud", "vast", "777");
        let hetz = pod("devtest-flutter", "hetzner", "888");
        cache.merge(
            &[gone.clone(), kept.clone(), vast.clone(), hetz.clone()].map(|p| health(&p, Status::Pass, vec![])),
            NOW,
        );
        let listing = Listing::from_results(vec![
            ("runpod".into(), Ok(vec![kept.clone()])),
            ("vast".into(), Err(Error::provider("HTTP 429"))),
            // hetzner wasn't queried at all
        ]);
        assert_eq!(cache.prune(&listing), 1);
        assert!(cache.get(&gone).is_none(), "runpod listed OK without it");
        assert!(cache.get(&kept).is_some());
        assert!(cache.get(&vast).is_some(), "vast failed to list: can't tell");
        assert!(cache.get(&hetz).is_some(), "hetzner not queried: can't tell");
        // Once vast answers without it, it goes too.
        let listing = Listing::from_results(vec![("vast".into(), Ok(vec![]))]);
        assert_eq!(cache.prune(&listing), 1);
        assert!(cache.get(&vast).is_none());
    }

    #[test]
    fn build_joins_proxy_and_health_by_pod() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        let bloom = at(pod("devtest-bloom", "runpod", "rp2"), "10.0.0.2", 22002); // moved since the proxy wrote
        let cloud = pod("devtest-cloud", "runpod", "rp3"); // no endpoint yet
        let mayor = at(pod("devtest-mayor", "runpod", "rp4"), "10.0.0.4", 22004); // off the list
        let james = at(pod("james-gpu", "runpod", "rp5"), "10.0.0.5", 22005); // absolute entry
        let pods = vec![mayor.clone(), cloud.clone(), bloom.clone(), apple.clone(), james.clone()];
        let mut cache = HealthCache::new();
        cache.merge(&[health(&apple, Status::Warn, vec![check("load", Status::Warn, "host load 90")])], NOW - 600);
        // A record for an older pod that held bloom's name: a different id, so not joined.
        cache.merge(&[health(&pod("devtest-bloom", "runpod", "rp-old"), Status::Fail, vec![])], NOW - 7200);

        let text = proxy_text();
        let snap = build(&pods, &[], Some(&text), &cache, &naming, NOW);
        let row = |name: &str| snap.pods.iter().find(|p| p.pod.name == name).unwrap();
        // (name, proxy port, proxy state, has health, list entry)
        let cases: &[(&str, Option<u16>, ProxyState, bool, Option<&str>)] = &[
            ("devtest-apple", Some(9500), ProxyState::Live, true, Some("apple")),
            ("devtest-bloom", Some(9501), ProxyState::Stale, false, Some("bloom")),
            ("devtest-cloud", Some(9502), ProxyState::Stale, false, Some("cloud")),
            ("devtest-mayor", None, ProxyState::None, false, None),
            ("james-gpu", None, ProxyState::None, false, Some("@james-gpu")),
        ];
        for (name, port, state, has_health, entry) in cases {
            let r = row(name);
            assert_eq!((r.proxy_port, r.proxy), (*port, *state), "{name}");
            assert_eq!(r.health.is_some(), *has_health, "{name}");
            assert_eq!(r.list_entry.as_deref(), *entry, "{name}");
            assert_eq!(r.in_name_list, entry.is_some(), "{name}");
        }
        // Sorted like `pods list`; cost over every pod; the stamp in both forms.
        let names: Vec<&str> = snap.pods.iter().map(|p| p.pod.name.as_str()).collect();
        assert_eq!(names, ["devtest-apple", "devtest-bloom", "devtest-cloud", "devtest-mayor", "james-gpu"]);
        assert_eq!(snap.cost.billing, 5);
        assert_eq!((snap.generated_at, snap.generated_at_rfc3339.as_str()), (NOW, "2026-10-08T12:00:00Z"));
        assert!(snap.partial.is_empty());

        // An IPv6 endpoint matches its (bracketed) proxy target.
        let v6 = at(pod("devtest-cloud", "runpod", "rp3"), "2001:DB8::7", 2222);
        let snap = build(&[v6], &[], Some(&text), &cache, &naming, NOW);
        assert_eq!(snap.pods[0].proxy, ProxyState::Live);

        // No proxy file yet: every pod `None`. Not read: `Unknown`, no ports.
        let snap = build(&pods, &[], Some(""), &cache, &naming, NOW);
        assert!(snap.pods.iter().all(|p| p.proxy == ProxyState::None && p.proxy_port.is_none()));
        let snap = build(&pods, &[], None, &cache, &naming, NOW);
        assert!(snap.pods.iter().all(|p| p.proxy == ProxyState::Unknown && p.proxy_port.is_none()));
    }

    #[test]
    fn build_carries_a_partial_listing() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        let snap = build(&[apple], &["vast".to_string()], None, &HealthCache::new(), &naming, NOW);
        assert_eq!(snap.partial, ["vast"]);
        assert!(render_table(&snap).ends_with("partial: vast failed to list — its pods are missing above\n"));
        let public = public_snapshot(&snap, &naming);
        assert!(!public.complete);
        assert_eq!(public.machines.len(), 1);
        // Every provider failed: nothing to show but the gap.
        let snap = build(&[], &["runpod".to_string()], None, &HealthCache::new(), &naming, NOW);
        assert!(render_table(&snap).contains("partial: runpod failed to list"));
        assert!(!public_snapshot(&snap, &naming).complete);
    }

    /// The proxy file every surface judges PROXY by: read locally or not at all (never over
    /// SSH), with "no file yet" and "no proxy configured" both meaning "no forwards".
    #[test]
    fn local_proxy_text_table() {
        let dir = tmp("proxytext");
        let file = dir.0.join("proxy.conf");
        std::fs::write(&file, proxy_text()).unwrap();
        let px = |extra: &str| Config::parse(&format!("SSH_PROXY_HOST=proxy.example.com\n{extra}"));
        let at_path = |p: &Path| px(&format!("SSH_PROXY_NGINX_CONFIG_PATH={}\n", p.display()));
        // (config, text read?, a warning?)
        let cases: Vec<(&str, Config, Option<String>, bool)> = vec![
            ("no proxy configured", Config::parse(""), Some(String::new()), false),
            ("remote proxy: never fetched", px("PROXY_LOCAL=false\n"), None, false),
            ("local, file present", at_path(&file), Some(proxy_text()), false),
            ("local, no file yet", at_path(&dir.0.join("missing.conf")), Some(String::new()), false),
            ("local, unreadable (a directory)", at_path(&dir.0), None, true),
        ];
        for (what, cfg, want, warns) in cases {
            let (text, warning) = local_proxy_text(&cfg);
            assert_eq!(text, want, "{what}");
            assert_eq!(warning.is_some(), warns, "{what}: {warning:?}");
        }
        assert_eq!(expand_home("/abs/proxy.conf"), "/abs/proxy.conf");
    }

    #[test]
    fn health_cache_path_for_takes_the_state_dir_and_prefix_from_config() {
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR=/srv/arena-state\n");
        assert_eq!(health_cache_path_for(&cfg).unwrap(), Path::new("/srv/arena-state/devtest/health.json"));
        let rel = Config::parse("ARENA_STATE_DIR=state\n");
        assert!(health_cache_path_for(&rel).unwrap_err().contains("absolute"));
    }

    #[test]
    fn snapshot_table_snapshot() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let mut apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        apple.gpu_type = Some("RTX A4000".into());
        apple.gpu_count = Some(1);
        apple.cost_per_hr = Some(0.17);
        let bloom = at(pod("devtest-bloom", "runpod", "rp2"), "10.0.0.2", 22002);
        let mut cloud = pod("devtest-cloud", "runpod", "rp3");
        cloud.status = "EXITED".into();
        let mut cache = HealthCache::new();
        cache.merge(&[health(&apple, Status::Pass, vec![])], NOW - 720);
        cache.merge(&[health(&bloom, Status::Fail, vec![check("cuda", Status::Fail, "Error 999")])], NOW - 7300);
        let text = proxy_text();
        let mut snap = build(&[cloud, bloom, apple], &[], Some(&text), &cache, &naming, NOW);
        snap.pods[0].reachable = Some(true); // apple answered; bloom didn't; cloud wasn't probed
        snap.pods[1].reachable = Some(false);
        let out = render_table(&snap);
        let want = "\
NAME           PROVIDER  ID   STATUS  GPU            $/H  ENDPOINT        SSH   PROXY        HEALTH             MAINT
devtest-apple  runpod    rp1  run     1×RTX A4000  $0.17  10.0.0.1:22001  ok    :9500        pass 12m           -
devtest-bloom  runpod    rp2  run     -                -  10.0.0.2:22002  down  :9501 stale  fail 2h GPU error  -
devtest-cloud  runpod    rp3  exit    -                -  -               -     :9502 stale  -                  -
fleet: $0.17/h across 2 billing pod(s) (1 unpriced)
";
        assert_eq!(out, want, "\n--- got ---\n{out}");
    }

    #[test]
    fn public_status_table() {
        let mk = |status: &str, ep: bool| {
            let p = pod("x", "runpod", "id");
            let mut p = if ep { at(p, "10.0.0.1", 22) } else { p };
            p.status = status.into();
            p
        };
        // (provider status, has an endpoint, probe answer, public status)
        let cases = [
            ("RUNNING", true, Some(true), PublicStatus::Up),
            ("running", true, Some(true), PublicStatus::Up), // vast/hetzner spell it lowercase
            ("RUNNING", true, None, PublicStatus::Up),       // `--no-probe`: the endpoint is all we know
            // Listed running with an endpoint, but sshd didn't answer: not up — and not a
            // fourth public word either.
            ("RUNNING", true, Some(false), PublicStatus::Starting),
            ("RUNNING", false, None, PublicStatus::Starting), // v1 says RUNNING the moment it's asked for
            ("PROVISIONING", false, None, PublicStatus::Starting),
            ("STARTING", true, Some(true), PublicStatus::Starting),
            ("LOADING", false, None, PublicStatus::Starting),
            ("ERROR", true, Some(true), PublicStatus::Down), // bills, but isn't usable
            ("EXITED", true, None, PublicStatus::Down),
            ("STOPPED", false, None, PublicStatus::Down),
            ("OFF", true, Some(true), PublicStatus::Down),
            ("WHATEVER", true, Some(true), PublicStatus::Down),
        ];
        for (status, ep, reachable, want) in cases {
            assert_eq!(public_status(&mk(status, ep), reachable), want, "{status} endpoint={ep} reachable={reachable:?}");
        }
    }

    #[test]
    fn public_keeps_one_row_per_listed_name_in_list_order() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let mut stopped_twin = pod("devtest-apple", "runpod", "rp-old");
        stopped_twin.status = "EXITED".into();
        let apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        let cloud = pod("devtest-cloud", "runpod", "rp3");
        let bloom = at(pod("devtest-bloom", "vast", "99"), "ssh4.vast.ai", 40022);
        let pods = [stopped_twin, cloud, bloom, apple];
        let snap = build(&pods, &[], None, &HealthCache::new(), &naming, NOW);
        let public = public_snapshot(&snap, &naming);
        let rows: Vec<(&str, PublicStatus)> = public.machines.iter().map(|m| (m.name.as_str(), m.status)).collect();
        assert_eq!(rows, [("apple", PublicStatus::Up), ("bloom", PublicStatus::Up), ("cloud", PublicStatus::Starting)]);
        assert!(public.complete);
        assert_eq!(public.updated_at, "2026-10-08T12:00:00Z");
    }

    #[test]
    fn public_health_and_maintenance_fields() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let mut apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        apple.maintenance = Some(Maintenance {
            start: Some("2026-10-09T02:00:00Z".into()),
            end: Some("next tuesday, ask 10.1.1.1".into()), // not a timestamp: dropped
            note: Some("host upgrade".into()),
        });
        let mut bloom = at(pod("devtest-bloom", "runpod", "rp2"), "10.0.0.2", 22002);
        bloom.maintenance = Some(Maintenance { start: None, end: None, note: Some("GPU swap".into()) });
        let cloud = at(pod("devtest-cloud", "runpod", "rp3"), "10.0.0.3", 22003);
        let mut cache = HealthCache::new();
        let mut h = health(&apple, Status::Warn, vec![check("maintenance", Status::Warn, "maint 10-09 02:00→?")]);
        h.facts = Some(DeepFacts {
            gpus: vec![GpuFact { index: 0, name: Some("NVIDIA RTX A4000".into()), ..Default::default() }],
            ..Default::default()
        });
        cache.merge(&[h], NOW - 300);
        // A fail record with no issue list (an older/edited file) still says *something* fixed.
        let mut bare = HealthRecord::from_health(&health(&bloom, Status::Fail, vec![]), NOW - 50);
        bare.issues.clear();
        cache.pods.insert(cache_key("runpod", "rp2"), bare);
        let snap = build(&[apple, bloom, cloud], &[], None, &cache, &naming, NOW);
        let public = public_snapshot(&snap, &naming);
        let m = &public.machines;
        assert_eq!(m[0].gpu, "1×RTX A4000", "the check's GPU view when the provider gave none");
        assert_eq!(
            m[0].health,
            PublicHealth {
                status: PublicHealthStatus::Warn,
                checked_at: Some("2026-10-08T11:55:00Z".into()),
                age_secs: Some(300),
                reason: Some(Issue::MaintenanceScheduled),
            }
        );
        assert_eq!(m[0].maintenance, Some(PublicMaintenance { start: Some("2026-10-09T02:00:00Z".into()), end: None }));
        assert_eq!(m[1].health.reason, Some(Issue::Other));
        assert_eq!(m[1].maintenance, Some(PublicMaintenance { start: None, end: None }), "a window with only a note");
        assert_eq!(m[2].health, PublicHealth { status: PublicHealthStatus::Unknown, checked_at: None, age_secs: None, reason: None });
        assert_eq!(m[2].maintenance, None);
        assert_eq!(m[2].gpu, "-");
    }

    #[test]
    fn public_snapshot_schema_round_trips_and_is_pinned() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let mut apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        apple.gpu_type = Some("RTX A4000".into());
        apple.gpu_count = Some(2);
        apple.maintenance = Some(Maintenance {
            start: Some("2026-10-09T02:00:00Z".into()),
            end: Some("2026-10-09T06:00:00Z".into()),
            note: None,
        });
        let mut cache = HealthCache::new();
        cache.merge(&[health(&apple, Status::Fail, vec![check("driver", Status::Fail, "535 < 580")])], NOW - 90);
        let public = public_snapshot(&build(&[apple], &[], None, &cache, &naming, NOW), &naming);
        let json = public_json(&public);
        let back: PublicSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, public);

        // The exact field set — adding one is a deliberate change to this test.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), ["complete", "machines", "updated_at"]);
        let m = &v["machines"][0];
        assert_eq!(keys(m), ["gpu", "health", "maintenance", "name", "status"]);
        assert_eq!(keys(&m["health"]), ["age_secs", "checked_at", "reason", "status"]);
        assert_eq!(keys(&m["maintenance"]), ["end", "start"]);
        assert_eq!(
            v,
            serde_json::json!({
                "updated_at": "2026-10-08T12:00:00Z",
                "complete": true,
                "machines": [{
                    "name": "apple",
                    "gpu": "2×RTX A4000",
                    "status": "up",
                    "health": {"status": "fail", "checked_at": "2026-10-08T11:58:30Z", "age_secs": 90, "reason": "driver too old"},
                    "maintenance": {"start": "2026-10-09T02:00:00Z", "end": "2026-10-09T06:00:00Z"}
                }]
            })
        );
        // An unexpected field (an internal one, say) is rejected on read.
        let extra = json.replacen("\"complete\"", "\"ssh_ip\": \"10.0.0.1\", \"complete\"", 1);
        assert!(serde_json::from_str::<PublicSnapshot>(&extra).is_err());
    }

    #[test]
    fn public_gpu_keeps_clean_labels_and_drops_the_rest_whole() {
        let long = "A".repeat(41);
        let cases: &[(&str, &str)] = &[
            ("1×RTX A4000", "1×RTX A4000"),
            ("  2×RTX   3090 ", "2×RTX 3090"),
            ("1×A100 80GB PCIe+1×H100", "1×A100 80GB PCIe+1×H100"),
            ("cx23", "cx23"),
            ("4×GPU", "4×GPU"),
            ("", "-"),
            ("RTX 10.9.8.7", "-"),
            ("H100 on fe80::1", "-"),
            ("<b>A4000</b>", "-"),
            ("A4000\"", "-"),
            ("A4000\u{1b}[31m", "-"),
            (&long, "-"),
        ];
        for (label, want) in cases {
            assert_eq!(public_gpu(label), *want, "{label:?}");
        }
    }

    /// Only a complete RFC 3339 date-time survives, and what's published is the instant
    /// re-printed in UTC — never the provider's string, so nothing riding after a valid
    /// prefix (an `ip:port`, a path) can reach the page.
    #[test]
    fn public_time_accepts_timestamps_only() {
        for (raw, want) in [
            (Some("2026-10-09T02:00:00Z"), Some("2026-10-09T02:00:00Z")),
            (Some(" 2026-10-09 02:00:00+02:00 "), Some("2026-10-09T00:00:00Z")),
            (Some("2026-10-09T23:30:00-01:15"), Some("2026-10-10T00:45:00Z")),
            (Some("2026-10-09T02:00:00.000Z"), Some("2026-10-09T02:00:00Z")),
            (Some("2026-10-09t02:00:00.123456789z"), Some("2026-10-09T02:00:00Z")),
            (Some("2026-10-09T02:00Z"), Some("2026-10-09T02:00:00Z")),
            (Some("2028-02-29T00:00:00Z"), Some("2028-02-29T00:00:00Z")),
            (Some("2026-12-31T23:59:60Z"), Some("2027-01-01T00:00:00Z")), // a leap second
            // An address after a timestamp-shaped start: the probe that got through before.
            (Some("2026-10-09T02:00 203.0.113.7:10022"), None),
            (Some("2026-10-09T06:00 2001::42"), None),
            (Some("2026-10-09T02:00:00Z 203.0.113.7"), None),
            (Some("2026-10-09T02:00:00+02:00:10022"), None),
            (Some("2026-10-09T02:00 call 10.0.0.1"), None),
            (Some("2026-10-09T02:00:00Z/../../etc"), None),
            // Not a complete, real instant.
            (Some("2026-10-09T02:00:00"), None), // no zone: the browser would guess
            (Some("2026-10-09"), None),
            (Some("2026-02-30T02:00:00Z"), None),
            (Some("2027-02-29T02:00:00Z"), None),
            (Some("2026-13-01T02:00:00Z"), None),
            (Some("2026-10-09T24:00:00Z"), None),
            (Some("2026-10-09T02:60:00Z"), None),
            (Some("2026-10-09T02:00:00.Z"), None),
            (Some("2026-10-09T02:00:00+24:00"), None),
            (Some("2026-10-09T02:00:00+0200"), None),
            (Some("1969-12-31T23:59:59Z"), None), // before what the page can print
            (Some("２０２６-10-09T02:00:00Z"), None),
            (Some("soon"), None),
            (Some(""), None),
            (None, None),
        ] {
            assert_eq!(public_time(raw).as_deref(), want, "{raw:?}");
        }
    }

    /// A page blanked by an outage is worse than a stale one: an empty *partial* snapshot
    /// never replaces a file that listed machines; anything else is published.
    #[test]
    fn an_empty_partial_snapshot_never_replaces_a_page_that_listed_machines() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        let full = public_json(&public_snapshot(&build(&[apple.clone()], &[], None, &HealthCache::new(), &naming, NOW), &naming));
        let none_listed = public_json(&public_snapshot(&build(&[], &[], None, &HealthCache::new(), &naming, NOW), &naming));
        let snap = |pods: &[Pod], partial: &[&str]| {
            let partial: Vec<String> = partial.iter().map(|p| p.to_string()).collect();
            public_snapshot(&build(pods, &partial, None, &HealthCache::new(), &naming, NOW), &naming)
        };
        let empty_partial = snap(&[], &["runpod"]);
        assert!(would_blank_the_page(&full, &empty_partial), "runpod failed, hetzner answered empty");
        // Published: a partial one that still shows machines, a complete empty one (a real
        // teardown), and an empty partial one over a page that had nothing / isn't ours.
        assert!(!would_blank_the_page(&full, &snap(std::slice::from_ref(&apple), &["vast"])));
        assert!(!would_blank_the_page(&full, &snap(&[], &[])));
        for previous in [none_listed.as_str(), "", "{not json", "{\"old\": true}"] {
            assert!(!would_blank_the_page(previous, &empty_partial), "{previous:?}");
        }
    }

    /// Two pods holding one name (a double create, a replacement left running): the public
    /// row is the twin the proxy forwards to — it must not borrow the other one's PASS.
    #[test]
    fn a_twin_never_lends_its_pass_to_the_pod_the_proxy_forwards_to() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let old = at(pod("devtest-apple", "runpod", "rp-old"), "10.0.0.8", 22008);
        let live = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001); // proxy_text() forwards here
        let mut cache = HealthCache::new();
        cache.merge(&[health(&old, Status::Pass, vec![])], NOW - 60);
        let text = proxy_text();
        for pods in [[old.clone(), live.clone()], [live.clone(), old.clone()]] {
            let snap = build(&pods, &[], Some(&text), &cache, &naming, NOW);
            let m = &public_snapshot(&snap, &naming).machines;
            assert_eq!(m.len(), 1);
            assert_eq!(m[0].health.status, PublicHealthStatus::Unknown, "the live twin was never checked");
        }
        // With the proxy unread, the checked twin is still preferred over an unchecked one.
        let snap = build(&[live.clone(), old.clone()], &[], None, &cache, &naming, NOW);
        assert_eq!(public_snapshot(&snap, &naming).machines[0].health.status, PublicHealthStatus::Pass);
    }

    #[test]
    fn ip_literal_detector_finds_v4_and_v6_anywhere_but_not_times_or_versions() {
        let cases: &[(&str, &[&str])] = &[
            ("a 203.0.113.7:10022 b", &["203.0.113.7"]),
            ("http://198.51.100.9:8080/maint", &["198.51.100.9"]),
            ("host10.0.0.1x", &["10.0.0.1"]),
            ("[2001:db8::1]:22", &["2001:db8::1"]),
            ("fe80::1%eth0", &["fe80::1"]),
            ("\"::1\"", &["::1"]),
            ("2001:db8::42.", &["2001:db8::42"]),
            ("\"2026-10-08T12:00:00Z\", 1.5, 0.17, \"1×RTX A4000\"", &[]),
            ("driver 580.65.06, CUDA 13.0, 300.1.2.3", &[]),
            ("{\"a\": \"b\", \"c\":\"d\"}", &[]),
        ];
        for (text, want) in cases {
            assert_eq!(ip_literals(text), *want, "{text}");
        }
    }

    /// A scripted [`Reach`]: the endpoints that answer, and every endpoint asked about.
    struct Answers {
        up: Vec<(&'static str, u16)>,
        asked: std::sync::Mutex<Vec<(String, u16)>>,
    }

    #[async_trait]
    impl Reach for Answers {
        async fn answers(&self, host: &str, port: u16) -> bool {
            self.asked.lock().unwrap().push((host.to_string(), port));
            self.up.iter().any(|(h, p)| *h == host && *p == port)
        }
    }

    /// The probe dials exactly the cohort's billing machines with an endpoint — each twin
    /// on its own endpoint, joined back by position, not name; never a stopped twin, a staff
    /// box or an off-list pod — and the public page then reads a listed-running machine that
    /// didn't answer as `starting`.
    #[tokio::test]
    async fn probe_reaches_billing_cohort_pods_by_endpoint_and_gates_public_up() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let apple = at(pod("devtest-apple", "runpod", "rp1"), "10.0.0.1", 22001);
        let bloom = at(pod("devtest-bloom", "runpod", "rp2"), "10.0.0.2", 22002); // listed running, sshd silent
        let cloud = pod("devtest-cloud", "runpod", "rp3"); // no endpoint yet
        let mut stopped = at(pod("devtest-apple", "runpod", "rp-old"), "10.0.0.8", 22008); // an old twin
        stopped.status = "EXITED".into();
        let staff = at(pod("james-gpu", "runpod", "rp-staff"), "10.0.0.9", 22009); // an `@` entry
        let mut stray = at(pod("devtest-mayor", "hetzner", "51"), "2001:db8::5", 22); // off the list
        stray.status = "starting".into();
        let mut booting = at(pod("devtest-cloud", "hetzner", "52"), "2001:db8::6", 22); // cloud's twin, booting
        booting.status = "starting".into();
        let pods = [apple, bloom, cloud, stopped, staff, stray, booting];
        let mut snap = build(&pods, &[], None, &HealthCache::new(), &naming, NOW);

        let targets: Vec<(String, u16)> = reach_targets(&snap).into_iter().map(|(_, h, p)| (h, p)).collect();
        assert_eq!(targets, [("2001:db8::6".into(), 22), ("10.0.0.1".into(), 22001), ("10.0.0.2".into(), 22002)]);

        let answers = Arc::new(Answers { up: vec![("10.0.0.1", 22001), ("2001:db8::6", 22)], asked: Default::default() });
        probe_reachability(&mut snap, answers.clone()).await;
        let mut asked = answers.asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked.len(), 3, "one probe per target, no more: {asked:?}");
        let reach = |id: &str| snap.pods.iter().find(|p| p.pod.id == id).unwrap().reachable;
        assert_eq!(
            [reach("rp1"), reach("rp2"), reach("rp3"), reach("rp-old"), reach("rp-staff"), reach("51"), reach("52")],
            [Some(true), Some(false), None, None, None, None, Some(true)]
        );
        let public = public_snapshot(&snap, &naming);
        let rows: Vec<(&str, PublicStatus)> = public.machines.iter().map(|m| (m.name.as_str(), m.status)).collect();
        assert_eq!(rows, [("apple", PublicStatus::Up), ("bloom", PublicStatus::Starting), ("cloud", PublicStatus::Starting)]);
        // The internal JSON carries the answer; the public one only the status word.
        assert!(serde_json::to_string(&snap).unwrap().contains("\"reachable\":false"));
        assert!(!public_json(&public).contains("reachable"));
    }

    #[test]
    fn greeting_detection_table() {
        let cases: &[(&[u8], bool)] = &[
            (b"SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13\r\n", true),
            (b"SSH-", true), // the rest of the line may still be in flight
            (b"SSH-1.99-dropbear\r\n", true),
            (b"Welcome to the jump host\r\nSSH-2.0-OpenSSH_8.9\r\n", true), // RFC 4253 pre-banner lines
            (b"", false),
            (b"SSH", false),
            (b"HTTP/1.1 400 Bad Request\r\n", false),
            (b"xSSH-2.0", false),
            (b"\x00\x00\x00\x0c", false),
        ];
        for (bytes, want) in cases {
            assert_eq!(greets(bytes), *want, "{:?}", String::from_utf8_lossy(bytes));
        }
    }

    /// The real probe against local sockets — nothing leaves the machine: an sshd-like
    /// listener answers; a closed port, a relay that accepts then hangs up, one that
    /// accepts and says nothing, and one that greets with something else don't.
    #[tokio::test]
    async fn ssh_port_probe_against_local_listeners() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        // A listener that answers every connection with `greeting` (None = says nothing,
        // holding the connection open; Some(b"") = closes at once).
        async fn serve(greeting: Option<&'static [u8]>) -> u16 {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = l.accept().await {
                    tokio::spawn(async move {
                        match greeting {
                            Some(g) => {
                                let _ = sock.write_all(g).await;
                            }
                            None => tokio::time::sleep(Duration::from_secs(30)).await,
                        }
                        drop(sock);
                    });
                }
            });
            port
        }
        let sshd = serve(Some(b"SSH-2.0-OpenSSH_9.6\r\n")).await;
        let pre_banner = serve(Some(b"notice: maintenance at 02:00\r\nSSH-2.0-OpenSSH_9.6\r\n")).await;
        let hangs_up = serve(Some(b"")).await;
        let silent = serve(None).await;
        let http = serve(Some(b"HTTP/1.1 400 Bad Request\r\n\r\n")).await;
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port() // dropped: nothing listens there now
        };
        let probe = SshPortProbe { timeout: Duration::from_secs(1) };
        for (what, host, port, want) in [
            ("sshd", "127.0.0.1", sshd, true),
            ("sshd, bracketed host", "[127.0.0.1]", sshd, true),
            ("pre-banner line", "127.0.0.1", pre_banner, true),
            ("closed port", "127.0.0.1", closed, false),
            ("accepts then hangs up", "127.0.0.1", hangs_up, false),
            ("accepts, says nothing", "127.0.0.1", silent, false),
            ("not ssh", "127.0.0.1", http, false),
        ] {
            let started = std::time::Instant::now();
            assert_eq!(probe.answers(host, port).await, want, "{what}");
            assert!(started.elapsed() < Duration::from_secs(5), "{what}: bounded by the probe's timeout");
        }
        // The default budget is the documented few seconds.
        assert_eq!(SshPortProbe::default().timeout, REACH_TIMEOUT);
    }

    /// PLAN Phase 4's leak test: a fleet stuffed with everything that must never be
    /// published — IPv4/IPv6 endpoints, hostnames, ports, provider ids, costs, provider
    /// names, off-list/staff pods, an absolute (`@`) entry, keys in the config, in a
    /// maintenance note and in check reasons — serialized with `--public`: none of it
    /// appears.
    #[test]
    fn public_json_leaks_nothing_from_a_hostile_fixture() {
        let cfg = Config::parse(
            "RUNPOD_API_KEY=rpa_SECRETSECRETSECRET\nVAST_API_KEY=vastkey0123456789\n\
             OPENROUTER_PROVISIONING_KEY=sk-or-v1-provisioningsecret\nHF_TOKEN=hf_tokentokentoken\n\
             SSH_PROXY_HOST=cute.sus.cat\nSSH_PROXY_STARTING_PORT=9500\nMACHINE_NAME_PREFIX=devtest\n\
             MACHINE_NAME_LIST=(\n \"apple\"\n \"bloom\"\n \"cloud\"\n \"dune\"\n \"@james-gpu\"\n \"@arena-james\"\n)\n",
        );
        let naming = Naming::from_config(&cfg);
        let priced = |mut p: Pod, gpu: &str, n: u32, c: f64| {
            p.gpu_type = Some(gpu.into());
            p.gpu_count = Some(n);
            p.cost_per_hr = Some(c);
            p
        };
        let mut apple = priced(at(pod("devtest-apple", "runpod", "x7k2p9q4m1n8"), "203.0.113.7", 10022), "RTX A4000", 1, 0.17);
        apple.maintenance = Some(Maintenance {
            start: Some("2026-10-09T02:00:00Z".into()),
            end: Some("http://198.51.100.9:8080/maint?key=sk-or-v1-leakyleaky".into()),
            note: Some("host 198.51.100.9 reboot; token rpa_NOTEKEYNOTEKEY; see admin@runpod.io".into()),
        });
        let bloom = priced(at(pod("devtest-bloom", "vast", "31415926"), "ssh4.vast.ai", 40022), "RTX 3090", 2, 0.33);
        let mut cloud = priced(at(pod("devtest-cloud", "runpod", "q9w8e7r6t5y4"), "2001:db8::42", 2222), "RTX 4090", 1, 0.44);
        // Timestamp-shaped starts with an address after them (RunPod's raw strings pass through).
        cloud.maintenance = Some(Maintenance {
            start: Some("2026-10-09T02:00 203.0.113.7:10022".into()),
            end: Some("2026-10-09T06:00 2001::42".into()),
            note: None,
        });
        // A GPU name carrying junk (untrusted provider text): only the safe characters survive.
        let dune = priced(at(pod("devtest-dune", "hetzner", "51234567"), "192.0.2.77", 22), "cx23 <img src=x onerror=alert(1)> 10.9.8.7", 0, 0.0056);
        // Off-list / staff / absolute-entry pods.
        let james = priced(at(pod("james-gpu", "runpod", "staff0001abc"), "192.0.2.10", 13000), "H100 SXM", 8, 21.52);
        let arena_james = priced(at(pod("arena-james", "runpod", "staff0002abc"), "192.0.2.11", 13001), "A100", 1, 1.19);
        let prawn = priced(at(pod("registered_pink_prawn", "vast", "27182818"), "192.0.2.12", 13002), "RTX 4090", 1, 0.39);
        let stray = priced(at(pod("devtest-mayor", "runpod", "offlist00099"), "192.0.2.13", 13003), "RTX A4000", 1, 0.17);
        let pods = vec![apple.clone(), bloom.clone(), cloud.clone(), dune.clone(), james, arena_james, prawn, stray];

        let mut cache = HealthCache::new();
        let reasons = |p: &Pod| {
            vec![
                check("ssh", Status::Fail, "exit 255: ssh: connect to host 203.0.113.7 port 10022: Connection refused"),
                check("network", Status::Warn, "huggingface.co unreachable: curl exit 6 (rpa_REASONKEY sk-or-v1-reasonkey)"),
                check("disk", Status::Warn, &format!("/workspace on {} 3.2 GB free", p.id)),
            ]
        };
        for p in &pods {
            cache.merge(&[health(p, Status::Fail, reasons(p))], NOW - 1234);
        }
        let mut proxy = proxy_text();
        proxy.push_str("# arena-forward name=james-gpu port=9504 target=192.0.2.10:13000 provider=runpod pod_id=staff0001abc\nserver {\n    listen 9504;\n    proxy_pass 192.0.2.10:13000;\n}\n");

        let snap = build(&pods, &["vast".to_string()], Some(&proxy), &cache, &naming, NOW);
        // The internal snapshot does carry these (that's what makes this test meaningful).
        let internal = serde_json::to_string(&snap).unwrap();
        for needle in ["203.0.113.7", "x7k2p9q4m1n8", "rpa_REASONKEY", "james-gpu", "9504", "0.17"] {
            assert!(internal.contains(needle), "fixture sanity: {needle}");
        }

        let public = public_snapshot(&snap, &naming);
        let json = public_json(&public);
        let names: Vec<&str> = public.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["apple", "bloom", "cloud", "dune"], "{json}");

        // No IP literal of any kind.
        assert!(ip_literals(&json).is_empty(), "IPs in {json}: {:?}", ip_literals(&json));
        // No key material, hostnames, provider names, money, or off-list/staff names.
        let lower = json.to_lowercase();
        for needle in [
            "sk-", "rpa_", "hf_", "vastkey", "secret", "cute.sus.cat", "vast.ai", "runpod", "vast", "hetzner",
            "admin@", "http", "maint?", "james", "prawn", "registered", "mayor", "devtest", "$", "€", "0.17",
            "0.33", "0.44", "21.52", "0.0056", "cost", "price", "ssh", "endpoint", "port", "proxy", "<", ">",
            "onerror", "reboot", "huggingface", "workspace", "connection refused",
        ] {
            assert!(!lower.contains(needle), "`{needle}` leaked into {json}");
        }
        // No provider id, SSH port or proxy port.
        let numbers = ["10022", "40022", "2222", "13000", "13001", "13002", "13003", "9500", "9501", "9502", "9504"];
        for p in &pods {
            assert!(!json.contains(&p.id), "id {} leaked into {json}", p.id);
        }
        for n in numbers {
            assert!(!json.contains(n), "port {n} leaked into {json}");
        }
        // What *is* there: the allowlisted facts; a junk GPU label is dropped whole.
        let gpus: Vec<&str> = public.machines.iter().map(|m| m.gpu.as_str()).collect();
        assert_eq!(gpus, ["1×RTX A4000", "2×RTX 3090", "1×RTX 4090", "-"]);
        assert!(!public.complete);
        assert!(public.machines.iter().all(|m| m.health.reason == Some(Issue::Unreachable)));
        assert_eq!(public.machines[0].maintenance, Some(PublicMaintenance { start: Some("2026-10-09T02:00:00Z".into()), end: None }));
        assert_eq!(public.machines[2].maintenance, Some(PublicMaintenance { start: None, end: None }));
    }
}
