//! One read-only picture of the fleet (PLAN Phase 3/4): every pod with its cost labels,
//! proxy port + whether that forward is live, and its last `pods test --deep` verdict —
//! built by one pure function ([`build`]) so the CLI (`arena snapshot`), the TUI and the
//! public dashboard all read the same thing.
//!
//! Three pieces, split so most of it is pure and table-tested:
//!
//! 1. **The health cache** ([`HealthCache`]): a deep check takes minutes and SSHes into
//!    every pod, so `snapshot` never runs one — it reads what the last `pods test --deep` /
//!    `up --check` recorded, with its age. Stored per fleet prefix
//!    ([`health_cache_path`]) so the sandbox and production never share one, written
//!    atomically (temp + rename) under a lock, and loaded tolerantly (a corrupt file is an
//!    empty cache plus one warning — it's a convenience, never a reason to fail).
//! 2. **[`FleetSnapshot`]**: the internal view (ids, endpoints, costs, raw check reasons).
//!    For the operator only.
//! 3. **[`PublicSnapshot`]**: what may be published with no auth. An **allowlist by
//!    construction**, not redaction: a separate struct with only name / GPU / up-starting-
//!    down / health verdict + age + a reason from a *fixed* vocabulary ([`Issue`]) /
//!    maintenance start+end, for pods on `MACHINE_NAME_LIST` only. A new internal field
//!    can't leak by accident because nothing copies the internal struct wholesale; raw
//!    check text (which carries IPs, paths, hostnames) never crosses over — only the enum.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fleet::{self, clip, iso_parts, FleetCost};
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
/// disk, rename it over. The temp file is removed if anything fails.
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
}

/// The whole fleet at one moment. See the module docs.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// `arena snapshot`'s table: `pods list`'s columns with PROXY and HEALTH before MAINT, then
/// the fleet cost footer and, for a partial listing, which providers are missing.
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
            cells.push(proxy_label(p));
            cells.push(health_label(p.health.as_ref(), snap.generated_at));
            cells.push(maint);
            cells
        })
        .collect();
    let mut headers: Vec<&str> = fleet::POD_HEADERS.to_vec();
    let mut align: Vec<Align> = fleet::POD_ALIGN.to_vec();
    let (maint_h, maint_a) = (headers.pop().unwrap_or("MAINT"), align.pop().unwrap_or(Align::Left));
    headers.extend(["PROXY", "HEALTH", maint_h]);
    align.extend([Align::Left, Align::Left, maint_a]);
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

/// Up / coming up / not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicStatus {
    /// Running with an SSH endpoint.
    Up,
    /// Billing but not yet reachable (provisioning, booting, no endpoint yet).
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMaintenance {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// `up` / `starting` / `down` from the provider status and the endpoint: down unless
/// billing ([`is_billing`]) — and `ERROR`, which bills but isn't usable, is down too; up
/// only when `RUNNING` with an SSH endpoint; anything else billing is starting.
pub fn public_status(pod: &Pod) -> PublicStatus {
    let s = pod.status.trim().to_ascii_uppercase();
    if !is_billing(&s) || s == "ERROR" {
        PublicStatus::Down
    } else if s == "RUNNING" && endpoint(pod).is_some() {
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

/// A maintenance timestamp, only if it is one: ISO-8601-shaped (`YYYY-MM-DD[T ]HH:MM…`),
/// short, and made of timestamp characters. Anything else is dropped, not shown raw.
fn public_time(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    let ok = s.len() <= 40
        && iso_parts(s).is_some()
        && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+' | ' '));
    ok.then(|| s.to_string())
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
/// shown once, by its most usable pod (up, then starting, then down; then the one with a
/// health record). Ordered by list position.
pub fn public_snapshot(snap: &FleetSnapshot, naming: &Naming) -> PublicSnapshot {
    let rank = |s: PublicStatus| match s {
        PublicStatus::Up => 0,
        PublicStatus::Starting => 1,
        PublicStatus::Down => 2,
    };
    let mut best: BTreeMap<usize, (PublicMachine, (u8, bool))> = BTreeMap::new();
    for p in &snap.pods {
        let Some(entry) = p.list_entry.as_deref().filter(|e| !is_absolute(e)) else { continue };
        let Some(name) = public_name(entry) else { continue };
        // The slot orders the page; the entry must still qualify to this pod's name now.
        let Some(slot) = naming.list.iter().position(|e| e == entry && qualify(naming.prefix, e) == p.pod.name) else {
            continue;
        };
        let status = public_status(&p.pod);
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
        let key = (rank(status), p.health.is_none());
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

/// [`public_snapshot`] as the JSON written to `fleet.json`.
pub fn public_json(public: &PublicSnapshot) -> String {
    let mut s = serde_json::to_string_pretty(public).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
        let out = render_table(&build(&[cloud, bloom, apple], &[], Some(&text), &cache, &naming, NOW));
        let want = "\
NAME           PROVIDER  ID   STATUS  GPU            $/H  ENDPOINT        PROXY        HEALTH             MAINT
devtest-apple  runpod    rp1  run     1×RTX A4000  $0.17  10.0.0.1:22001  :9500        pass 12m           -
devtest-bloom  runpod    rp2  run     -                -  10.0.0.2:22002  :9501 stale  fail 2h GPU error  -
devtest-cloud  runpod    rp3  exit    -                -  -               :9502 stale  -                  -
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
        let cases = [
            ("RUNNING", true, PublicStatus::Up),
            ("running", true, PublicStatus::Up), // vast/hetzner spell it lowercase
            ("RUNNING", false, PublicStatus::Starting), // v1 says RUNNING the moment it's asked for
            ("PROVISIONING", false, PublicStatus::Starting),
            ("STARTING", true, PublicStatus::Starting),
            ("LOADING", false, PublicStatus::Starting),
            ("ERROR", true, PublicStatus::Down), // bills, but isn't usable
            ("EXITED", true, PublicStatus::Down),
            ("STOPPED", false, PublicStatus::Down),
            ("OFF", true, PublicStatus::Down),
            ("WHATEVER", true, PublicStatus::Down),
        ];
        for (status, ep, want) in cases {
            assert_eq!(public_status(&mk(status, ep)), want, "{status} endpoint={ep}");
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

    #[test]
    fn public_time_accepts_timestamps_only() {
        for (raw, want) in [
            (Some("2026-10-09T02:00:00Z"), Some("2026-10-09T02:00:00Z")),
            (Some(" 2026-10-09 02:00:00+02:00 "), Some("2026-10-09 02:00:00+02:00")),
            (Some("2026-10-09T02:00:00.000Z"), Some("2026-10-09T02:00:00.000Z")),
            (Some("soon"), None),
            (Some("2026-10-09T02:00 call 10.0.0.1"), None),
            (Some("2026-10-09T02:00:00Z/../../etc"), None),
            (None, None),
        ] {
            assert_eq!(public_time(raw).as_deref(), want, "{raw:?}");
        }
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
        let cloud = priced(at(pod("devtest-cloud", "runpod", "q9w8e7r6t5y4"), "2001:db8::42", 2222), "RTX 4090", 1, 0.44);
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
    }
}
