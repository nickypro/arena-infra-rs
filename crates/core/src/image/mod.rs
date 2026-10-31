//! Pre-built Hetzner images (`arena image build|list|prune`, `HETZNER_IMAGE=arena`).
//!
//! A RunPod or Vast pod starts from the cohort's Docker image with the env already in it; a
//! Hetzner VM starts as bare Ubuntu, and `hetzner_setup.sh` builds everything (packages, docker,
//! uv, ~4 GB of CPU wheels): ~10 minutes a VM. A snapshot of a VM that setup has built, with
//! everything that belongs to *that* machine scrubbed off, takes setup to about a minute — the
//! script skips the install when the venv already passes its gate.
//!
//! What Hetzner's API says about snapshots (checked live, 2026-10-29):
//! - **project-scoped**: `GET /images?type=snapshot` lists only the token's project's own, so
//!   the sandbox's and prod's are separate; each project bakes its own;
//! - **no location**: an image object has no location field, and one snapshot made servers in
//!   hel1 and nbg1 (2026-10-26) — so the create's location walk needs no filter;
//! - **architecture**: an x86 snapshot only boots x86 types (`architecture` on both);
//! - **disk**: a server's disk must be at least the snapshot's `disk_size` (the disk it was taken
//!   from), so bake on the smallest disk to serve: a cx23 (40 GB) fits cx23, cx33 and up;
//! - **price**: `GET /pricing` → `image.price_per_gb_month`, billed on `image_size` (the data on
//!   the disk, ~6 GB here), not `disk_size`.
//!
//! Which snapshots are arena's: the ones labelled [`LABEL`] *and* this cohort's prefix
//! ([`PREFIX_LABEL`]) — list, prune, teardown and `HETZNER_IMAGE=arena` see only those, so a
//! snapshot made by hand (or another cohort's) is never resolved, pruned or counted as ours.
//!
//! Pure here (table-tested): the labels, which snapshot `arena` resolves to, staleness notes, the
//! bake VM's type and the scrub script's report. The API calls are [`cloud`], the bake itself
//! [`build`].

pub mod build;
pub mod cloud;

use serde_json::{Map, Value};

use crate::config::Config;
use crate::provider::hetzner::ServerType;

/// `HETZNER_IMAGE` / `--image` value meaning "the newest arena-built snapshot that fits".
pub const KEYWORD: &str = "arena";

/// What a VM is created from when no arena image fits: Hetzner's Ubuntu 24.04, which setup
/// builds from scratch.
pub const FALLBACK: &str = "ubuntu-24.04";

/// Label on every snapshot `arena image build` makes (value `1`).
pub const LABEL: &str = "arena-image";

/// Label carrying the cohort prefix (`MACHINE_NAME_PREFIX`), on the snapshot and on the
/// temporary build VM.
pub const PREFIX_LABEL: &str = "arena-prefix";

/// Label on a bake's VM and its snapshot: that bake's own id ([`new_build_id`]).
pub const BUILD_ID_LABEL: &str = "arena-image-build-id";

/// Label on the temporary build VM (value: the prefix). The fleet never lists a server with it
/// (`pods *`, the proxy, backups, the reaper): it's no participant's machine, and a backup or
/// reap mid-bake would only get in the way. `teardown --check` lists it on its own line.
pub const BUILD_LABEL: &str = "arena-image-build";

/// An image older than this gets a note in `config check` and `image list`: the course repo and
/// the setup script move on, and every VM from an old image fast-forwards further at setup.
pub const STALE_DAYS: u64 = 14;

/// The file the bake leaves on the image, read by setup on the VMs made from it (which image,
/// how old, what its env was built from). `key=value` lines; setup appends `adopted=<unix>`
/// after its first run, so only the first setup on a VM compares the requirements.
pub const MARKER: &str = "/etc/arena-image";

/// What the env install reads, in hash order: the repo's own installer, its requirements, and
/// `pyproject.toml` (whose `requires-python` picks the venv's Python when there's no installer).
/// A change to any means the image's env may not be the one a fresh VM would get.
pub const REQ_FILES: [&str; 3] = ["install_uv.sh", "requirements.txt", "pyproject.toml"];

/// The shell twin of [`req_hash`], verbatim in `hetzner_setup.sh` and the scrub script (a test
/// holds all three to the same answer): each of [`REQ_FILES`] that exists in the directory, as
/// its name, a newline and its bytes, through sha256; the first 12 hex digits.
pub const REQ_HASH_FN: &str = r#"arena_req_hash() { ( cd "$1" 2>/dev/null || exit 0; for f in install_uv.sh requirements.txt pyproject.toml; do if [ -f "$f" ]; then printf '%s\n' "$f"; cat "$f"; fi; done ) | sha256sum | cut -c1-12; }"#;

/// [`REQ_HASH_FN`] in Rust, over the files' contents (`None`: not in the repo). Pure.
pub fn req_hash(install_uv: Option<&[u8]>, requirements: Option<&[u8]>, pyproject: Option<&[u8]>) -> String {
    let mut bytes = Vec::new();
    for (name, body) in REQ_FILES.iter().zip([install_uv, requirements, pyproject]) {
        if let Some(b) = body {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'\n');
            bytes.extend_from_slice(b);
        }
    }
    crate::update::sha256_hex(&bytes)[..12].to_string()
}

/// A Hetzner label value: at most 63 of `[A-Za-z0-9._-]`, starting and ending alphanumeric
/// (Hetzner refuses the create otherwise). Anything else becomes `-`. Pure.
pub fn label_value(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let trimmed = mapped.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let mut out: String = trimmed.chars().take(63).collect();
    while out.ends_with(|c: char| !c.is_ascii_alphanumeric()) {
        out.pop();
    }
    out
}

/// Whose images these are: the cohort prefix and its repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub prefix: String,
    /// `owner.name`, as a label value (for people: clipped to 63 characters).
    pub repo: String,
    /// The repo's identity ([`repo_id`]): what `arena` matches on — two long names can share
    /// their first 63 characters.
    pub repo_id: String,
}

/// A repo's identity as a label: the first 16 hex digits of the SHA-256 of `owner/name`
/// (lowercased: GitHub's names aren't case-sensitive). Empty for no repo. Pure.
pub fn repo_id(owner: &str, name: &str) -> String {
    if owner.is_empty() || name.is_empty() {
        return String::new();
    }
    crate::update::sha256_hex(format!("{owner}/{name}").to_lowercase().as_bytes())[..16].to_string()
}

impl Scope {
    /// `prefix`'s images of `owner/name`.
    pub fn new(prefix: &str, owner: &str, name: &str) -> Self {
        let repo = if owner.is_empty() || name.is_empty() { String::new() } else { label_value(&format!("{owner}.{name}")) };
        Self { prefix: label_value(prefix), repo, repo_id: repo_id(owner, name) }
    }

    /// From `MACHINE_NAME_PREFIX` (default `arena`) and `ARENA_REPO_OWNER`/`ARENA_REPO_NAME`.
    pub fn from_config(cfg: &Config) -> Self {
        let get = |k: &str| cfg.get(k).map(str::trim).filter(|s| !s.is_empty());
        Self::new(get("MACHINE_NAME_PREFIX").unwrap_or("arena"), get("ARENA_REPO_OWNER").unwrap_or(""), get("ARENA_REPO_NAME").unwrap_or(""))
    }

    /// `GET /images` / `GET /servers` label selector for this cohort's `key` resources.
    pub fn selector(&self, key: &str) -> String {
        if key == LABEL {
            format!("{LABEL}=1,{PREFIX_LABEL}={}", self.prefix)
        } else {
            format!("{key}={}", self.prefix)
        }
    }
}

/// One snapshot as `GET /images` lists it, with arena's labels read out.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: u64,
    pub description: String,
    /// `creating`, `available` or `unavailable`.
    pub status: String,
    /// When Hetzner made it (unix).
    pub created: Option<u64>,
    /// Billed size: the data on the disk, GB.
    pub size_gb: Option<f64>,
    /// The disk it was taken from: a server needs at least this much.
    pub disk_gb: Option<u32>,
    pub arch: Option<String>,
    /// Labelled [`LABEL`].
    pub arena: bool,
    pub prefix: Option<String>,
    pub repo: Option<String>,
    /// [`repo_id`] of the repo it holds.
    pub repo_id: Option<String>,
    pub commit: Option<String>,
    pub req: Option<String>,
    /// `built=` (unix), else `created`.
    pub built: Option<u64>,
}

impl Snapshot {
    /// Whether it's this cohort's arena image.
    pub fn ours(&self, scope: &Scope) -> bool {
        self.arena && self.prefix.as_deref() == Some(scope.prefix.as_str())
    }

    /// Seconds since it was built (0 for a clock behind it).
    pub fn age(&self, now: u64) -> Option<u64> {
        self.built.map(|b| now.saturating_sub(b))
    }

    /// What it bills a month at `per_gb_month`.
    pub fn monthly(&self, per_gb_month: f64) -> Option<f64> {
        self.size_gb.map(|gb| gb * per_gb_month)
    }
}

/// `v` (one `GET /images` item) as a [`Snapshot`]; `None` without an id. Pure.
pub fn parse_snapshot(v: &Value) -> Option<Snapshot> {
    let label = |k: &str| v.pointer(&format!("/labels/{k}")).and_then(Value::as_str).map(String::from).filter(|s| !s.is_empty());
    let created = v.get("created").and_then(Value::as_str).and_then(crate::reap::parse_rfc3339).and_then(|t| u64::try_from(t).ok());
    Some(Snapshot {
        id: v.get("id").and_then(Value::as_u64)?,
        description: v.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        status: v.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string(),
        created,
        size_gb: v.get("image_size").and_then(Value::as_f64),
        disk_gb: v.get("disk_size").and_then(Value::as_f64).map(|d| d as u32),
        arch: v.get("architecture").and_then(Value::as_str).map(String::from),
        arena: label(LABEL).is_some(),
        prefix: label(PREFIX_LABEL),
        repo: label("repo"),
        repo_id: label("repo-id"),
        commit: label("commit"),
        req: label("req"),
        built: label("built").and_then(|b| b.parse().ok()).or(created),
    })
}

/// What a GB of snapshot costs a month, in the account's currency (`GET /pricing`: the sandbox
/// account bills in USD, prod's in EUR — never assumed).
#[derive(Debug, Clone, PartialEq)]
pub struct ImagePrice {
    pub per_gb_month: f64,
    pub currency: String,
}

impl ImagePrice {
    /// `€`, `$`, or the currency code.
    pub fn symbol(&self) -> String {
        match self.currency.as_str() {
            "EUR" => "€".into(),
            "USD" => "$".into(),
            other => format!("{other} "),
        }
    }

    /// `€0.07`.
    pub fn money(&self, amount: f64) -> String {
        format!("{}{amount:.2}", self.symbol())
    }
}

/// `GET /pricing` → the snapshot price: `pricing.image.price_per_gb_month.gross` (what's billed)
/// and `pricing.currency`. `None` for any other shape. Pure.
pub fn parse_image_price(body: &Value) -> Option<ImagePrice> {
    let p = body.get("pricing")?;
    let gross = p.pointer("/image/price_per_gb_month/gross").or_else(|| p.pointer("/image/price_per_gb_month/net"))?;
    let per_gb_month = gross.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| gross.as_f64())?;
    let currency = p.get("currency").and_then(Value::as_str).unwrap_or("EUR").to_string();
    Some(ImagePrice { per_gb_month, currency })
}

/// What `HETZNER_IMAGE` (or `--image`) says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    /// [`KEYWORD`]: resolve against the project's snapshots.
    Arena,
    /// A snapshot (or any image) by id.
    Id(u64),
    /// A system image by name (`ubuntu-24.04`).
    Name(String),
}

impl Setting {
    /// Pure. Empty is the default system image.
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim();
        if s.eq_ignore_ascii_case(KEYWORD) {
            Setting::Arena
        } else if let Ok(id) = s.parse::<u64>() {
            Setting::Id(id)
        } else if s.is_empty() {
            Setting::Name(FALLBACK.into())
        } else {
            Setting::Name(s.to_string())
        }
    }
}

/// What a server type offers an image: its architecture and disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fit {
    pub server_type: String,
    pub arch: Option<String>,
    pub disk_gb: Option<u32>,
}

impl Fit {
    /// From a `GET /server_types` listing; `None` when Hetzner has no such type.
    pub fn of(types: &[ServerType], server_type: &str) -> Option<Self> {
        types.iter().find(|t| t.name == server_type).map(|t| Self {
            server_type: t.name.clone(),
            arch: t.architecture.clone(),
            disk_gb: t.disk_gb,
        })
    }
}

/// The newest of this cohort's available arena snapshots for this repo that `fit` can boot (same
/// architecture, a disk at least the snapshot's), or why there's none. Pure.
pub fn newest_fitting<'a>(snaps: &'a [Snapshot], scope: &Scope, fit: &Fit) -> Result<&'a Snapshot, String> {
    let ours: Vec<&Snapshot> = snaps.iter().filter(|s| s.ours(scope)).collect();
    if ours.is_empty() {
        return Err("no arena-built snapshot in this Hetzner project".into());
    }
    let mine: Vec<&Snapshot> = ours.iter().copied().filter(|s| !scope.repo_id.is_empty() && s.repo_id.as_deref() == Some(scope.repo_id.as_str())).collect();
    if mine.is_empty() {
        return Err(format!("none of the {} arena-built snapshot(s) here is of {}", ours.len(), scope.repo.replacen('.', "/", 1)));
    }
    let ready: Vec<&Snapshot> = mine.iter().copied().filter(|s| s.status == "available").collect();
    if ready.is_empty() {
        return Err("no arena-built snapshot here is available yet (still being made?)".into());
    }
    let arch: Vec<&Snapshot> = ready.iter().copied().filter(|s| fit.arch.is_none() || s.arch == fit.arch).collect();
    if arch.is_empty() {
        return Err(format!("none is for {}'s {} architecture", fit.server_type, fit.arch.as_deref().unwrap_or("?")));
    }
    let fits: Vec<&Snapshot> = arch.iter().copied().filter(|s| matches!((s.disk_gb, fit.disk_gb), (Some(need), Some(has)) if need <= has)).collect();
    fits.into_iter().max_by_key(|s| (s.built.unwrap_or(0), s.id)).ok_or_else(|| {
        format!("none fits {}'s {} GB disk (the smallest is {} GB)", fit.server_type, fit.disk_gb.map_or("?".into(), |d| d.to_string()), arch.iter().filter_map(|s| s.disk_gb).min().map_or("?".into(), |d| d.to_string()))
    })
}

/// What a create sends as `image`, and what to say about it.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// `arena` → this snapshot.
    Snapshot(Box<Snapshot>),
    /// `arena`, but none fits: [`FALLBACK`], and why.
    Fallback { why: String },
    /// An id or a name, sent as given.
    Given(Setting),
}

impl Resolved {
    /// The create body's `image`: an id as a number, a name as a string.
    pub fn image(&self) -> Value {
        match self {
            Resolved::Snapshot(s) => Value::from(s.id),
            Resolved::Fallback { .. } => Value::from(FALLBACK),
            Resolved::Given(Setting::Id(id)) => Value::from(*id),
            Resolved::Given(Setting::Name(n)) => Value::from(n.as_str()),
            Resolved::Given(Setting::Arena) => Value::from(FALLBACK),
        }
    }

    /// `image` as a `--image` / `HETZNER_IMAGE` value (the snapshot's id).
    pub fn as_setting(&self) -> String {
        match self.image() {
            Value::String(s) => s,
            other => other.to_string(),
        }
    }

    /// One line for `up`/`create`/`config check`: `image arena → snapshot 123 (devtest-image-…,
    /// built 2026-10-29, 2d old)` or `image arena → ubuntu-24.04 (<why>; …)`.
    pub fn describe(&self, now: u64) -> String {
        match self {
            Resolved::Snapshot(s) => format!("image {KEYWORD} → snapshot {} ({})", s.id, about(s, now)),
            Resolved::Fallback { why } => {
                format!("image {KEYWORD} → {FALLBACK} ({why}; setup builds the env, ~10 min — `arena image build` makes an image)")
            }
            Resolved::Given(Setting::Id(id)) => format!("image {id} (a snapshot or image id)"),
            Resolved::Given(Setting::Name(n)) => format!("image {n}"),
            Resolved::Given(Setting::Arena) => format!("image {KEYWORD}"),
        }
    }
}

/// `devtest-image-261029-1200, built 2026-10-29, 2d old, commit abc123`. Pure.
pub fn about(s: &Snapshot, now: u64) -> String {
    let mut parts = Vec::new();
    if !s.description.is_empty() {
        parts.push(s.description.clone());
    }
    if let Some(b) = s.built {
        parts.push(format!("built {}, {} old", &crate::snapshot::rfc3339(b)[..10], crate::snapshot::fmt_age(now.saturating_sub(b))));
    }
    if let Some(c) = &s.commit {
        parts.push(format!("commit {c}"));
    }
    parts.join(", ")
}

/// Resolve `setting` against the project's snapshots for a server of `fit`. Pure.
pub fn resolve(setting: &Setting, snaps: &[Snapshot], scope: &Scope, fit: &Fit) -> Resolved {
    match setting {
        Setting::Arena => match newest_fitting(snaps, scope, fit) {
            Ok(s) => Resolved::Snapshot(Box::new(s.clone())),
            Err(why) => Resolved::Fallback { why },
        },
        other => Resolved::Given(other.clone()),
    }
}

/// Whether an image's requirements are the repo's now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReqMatch {
    Same,
    Differ,
    /// The image has no `req` label, or the repo's couldn't be read.
    Unknown,
}

impl ReqMatch {
    /// Pure.
    pub fn of(image_req: Option<&str>, repo_req: Option<&str>) -> Self {
        match (image_req, repo_req) {
            (Some(a), Some(b)) if a == b => ReqMatch::Same,
            (Some(_), Some(_)) => ReqMatch::Differ,
            _ => ReqMatch::Unknown,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            ReqMatch::Same => "same",
            ReqMatch::Differ => "differ",
            ReqMatch::Unknown => "?",
        }
    }
}

/// What to tell the operator about the image `arena` resolves to: older than [`STALE_DAYS`], or
/// built from other requirements than the repo's now. Empty: nothing to do. Pure.
pub fn notes(s: &Snapshot, now: u64, repo_req: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(age) = s.age(now).filter(|a| *a > STALE_DAYS * 86_400) {
        out.push(format!(
            "built {} days ago (over {STALE_DAYS}): VMs from it fast-forward the course repo at setup — `arena image build` makes a current one",
            age / 86_400
        ));
    }
    if ReqMatch::of(s.req.as_deref(), repo_req) == ReqMatch::Differ {
        out.push(
            "the repo's requirements (install_uv.sh / requirements.txt) changed since it was built: every VM from it reinstalls \
             the env at setup (~10 min) — `arena image build` makes a current one"
                .into(),
        );
    }
    out
}

/// The least memory a bake VM needs: the env install (uv resolving and unpacking torch), docker
/// and VS Code's server. Hetzner sells a 1 GB type with a 20 GB disk (cpx02, 2026-10): its
/// smaller disk would fit more types, but the install would run out of memory on it.
pub const BAKE_MIN_MEMORY_GB: f64 = 4.0;

/// The server type the bake runs on: of the types on sale with `configured`'s architecture and
/// at least [`BAKE_MIN_MEMORY_GB`], the one with the smallest disk (a snapshot only boots on a
/// disk at least its own), then the cheapest, then by name. Err: why there's none. Pure.
pub fn bake_type(types: &[ServerType], configured: &str) -> Result<String, String> {
    let arch = types
        .iter()
        .find(|t| t.name == configured)
        .ok_or_else(|| format!("Hetzner has no server type `{configured}` on sale (HETZNER_SERVER_TYPE)"))?
        .architecture
        .clone();
    types
        .iter()
        .filter(|t| t.architecture == arch && t.disk_gb.is_some() && t.memory_gb.is_some_and(|m| m >= BAKE_MIN_MEMORY_GB))
        .min_by(|a, b| {
            a.disk_gb
                .cmp(&b.disk_gb)
                .then(a.eur_per_hour.unwrap_or(f64::MAX).total_cmp(&b.eur_per_hour.unwrap_or(f64::MAX)))
                .then(a.name.cmp(&b.name))
        })
        .map(|t| t.name.clone())
        .ok_or_else(|| format!("no server type with `{configured}`'s architecture and {BAKE_MIN_MEMORY_GB} GB of memory is on sale — pass --server-type"))
}

/// The temporary VM's name, which is also the snapshot's description: `<prefix>-image-<yymmdd-hhmm>`
/// (UTC). Pure.
pub fn build_name(prefix: &str, now: u64) -> String {
    let t = crate::snapshot::rfc3339(now);
    format!("{prefix}-image-{}{}{}-{}{}", &t[2..4], &t[5..7], &t[8..10], &t[11..13], &t[14..16])
}

/// What the bake learnt, for the snapshot's labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baked {
    /// This bake's own id ([`BUILD_ID_LABEL`]).
    pub build_id: String,
    pub built: u64,
    pub commit: String,
    pub req: String,
    pub arch: String,
    pub disk_gb: u32,
}

/// The snapshot's labels. Pure.
pub fn labels(scope: &Scope, b: &Baked) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in [
        (LABEL, "1".to_string()),
        (PREFIX_LABEL, scope.prefix.clone()),
        ("built", b.built.to_string()),
        ("repo", scope.repo.clone()),
        ("repo-id", scope.repo_id.clone()),
        (BUILD_ID_LABEL, label_value(&b.build_id)),
        ("commit", label_value(&b.commit)),
        ("req", label_value(&b.req)),
        ("arch", label_value(&b.arch)),
        ("disk", b.disk_gb.to_string()),
    ] {
        m.insert(k.to_string(), Value::from(v));
    }
    m
}

/// The temporary VM's labels: [`BUILD_LABEL`], the prefix and this bake's id.
pub fn build_vm_labels(scope: &Scope, build_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(BUILD_LABEL.into(), Value::from(scope.prefix.clone()));
    m.insert(PREFIX_LABEL.into(), Value::from(scope.prefix.clone()));
    m.insert(BUILD_ID_LABEL.into(), Value::from(label_value(build_id)));
    m
}

/// A bake's own id (unix nanoseconds and the pid): on its VM and its snapshot, so a bake whose
/// create (or snapshot) answer never came finds what it made — and only that, never another
/// bake's of the same name. Impure (the clock).
pub fn new_build_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos}-{}", std::process::id())
}

/// The scrub script, rendered: the secret patterns the backup redacts (plus private keys) for its
/// scan, the token variables to strip from the rc files, the hash function. Run as root on the
/// bake VM with `REPO_DIR`, `KEY_REMOTE`, `NAME` and `BUILT` in its environment ([`scrub_command`]).
pub fn scrub_script() -> String {
    let vars: Vec<&str> = crate::apikeys::PROVIDERS
        .iter()
        .flat_map(|(_, _, envs)| envs.iter().copied())
        .chain(crate::apikeys::BROADCAST_TOKENS.iter().flat_map(|(_, _, envs)| envs.iter().copied()))
        // Each machine's own Claude Code token (`crate::claude`), no longer a broadcast one.
        .chain(std::iter::once(crate::claude::TOKEN_KEY))
        .collect();
    let mut uniq: Vec<&str> = Vec::new();
    for v in vars {
        if !uniq.contains(&v) {
            uniq.push(v);
        }
    }
    include_str!("scrub.sh")
        .replace("@SECRET_RE@", &crate::backup::secret_regex())
        .replace("@TOKEN_VARS@", &uniq.join("|"))
        .replace("@REQ_HASH_FN@", REQ_HASH_FN)
        .replace("@MARKER@", MARKER)
}

/// The command that runs [`scrub_script`] on the bake VM (as a base64 blob: one argv, no quoting
/// of a long script through ssh).
pub fn scrub_command(repo_dir: &str, key_remote: &str, name: &str, built: u64) -> String {
    let q = crate::setup::shell_quote;
    format!(
        "printf '%s' {} | base64 -d > /root/.arena-scrub.sh && REPO_DIR={} KEY_REMOTE={} NAME={} BUILT={built} bash /root/.arena-scrub.sh",
        crate::base64::encode(scrub_script().as_bytes()),
        q(repo_dir),
        q(key_remote),
        q(name)
    )
}

/// What the scrub script said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    pub commit: Option<String>,
    pub req: Option<String>,
    /// Files the scan found a secret in — names only, never what matched.
    pub hits: Vec<String>,
    /// It reached its end.
    pub done: bool,
}

/// The scrub script's `arena-…:` lines. Pure.
pub fn parse_scrub(stdout: &str) -> ScrubReport {
    let mut r = ScrubReport::default();
    for line in stdout.lines().map(|l| l.trim_end_matches('\r')) {
        if let Some(v) = line.strip_prefix("arena-image-commit: ") {
            r.commit = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        } else if let Some(v) = line.strip_prefix("arena-image-req: ") {
            r.req = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        } else if let Some(v) = line.strip_prefix("arena-scan: ") {
            r.hits.push(v.trim().to_string());
        } else if line.starts_with("arena-scrub: done") {
            r.done = true;
        }
    }
    r
}

/// `image list`'s table: this cohort's arena snapshots, newest first — ID, BUILT, AGE, SIZE,
/// /MONTH, COMMIT, REQ (the repo's requirements now: `same`, `differ`, `?`), and `← arena` on
/// the one `HETZNER_IMAGE=arena` resolves to (`resolved`). Pure.
pub fn list_table(snaps: &[Snapshot], scope: &Scope, now: u64, price: Option<&ImagePrice>, repo_req: Option<&str>, resolved: Option<u64>) -> String {
    let mut ours: Vec<&Snapshot> = snaps.iter().filter(|s| s.ours(scope)).collect();
    ours.sort_by_key(|s| std::cmp::Reverse((s.built.unwrap_or(0), s.id)));
    let unit = price.map_or_else(|| "/MONTH".to_string(), |p| format!("{}/MONTH", p.symbol().trim()));
    let rows: Vec<Vec<String>> = ours
        .iter()
        .map(|s| {
            let mut note = Vec::new();
            if Some(s.id) == resolved {
                note.push(format!("← {KEYWORD}"));
            }
            if s.status != "available" {
                note.push(s.status.clone());
            }
            if s.repo_id.as_deref() != Some(scope.repo_id.as_str()) {
                note.push(format!("other repo ({})", s.repo.as_deref().unwrap_or("?")));
            }
            vec![
                s.id.to_string(),
                s.built.map_or("?".into(), |b| crate::snapshot::rfc3339(b)[..16].replace('T', " ")),
                s.age(now).map_or("?".into(), crate::snapshot::fmt_age),
                s.size_gb.map_or("?".into(), |g| format!("{g:.1} GB")),
                match (price, s.size_gb) {
                    (Some(p), Some(g)) => p.money(g * p.per_gb_month),
                    _ => "?".into(),
                },
                s.disk_gb.map_or("?".into(), |d| format!("{d} GB")),
                s.commit.clone().unwrap_or_else(|| "?".into()),
                ReqMatch::of(s.req.as_deref(), repo_req).word().to_string(),
                note.join(", "),
            ]
        })
        .collect();
    use crate::table::Align::{Left, Right};
    crate::table::render(
        &["ID", "BUILT (UTC)", "AGE", "SIZE", &unit, "MIN DISK", "COMMIT", "REQ", ""],
        &[Left, Left, Right, Right, Right, Right, Left, Left, Left],
        &rows,
    )
}

/// What `image prune --keep N` deletes: this cohort's arena snapshots past the newest `keep`,
/// except (for `keep` ≥ 1) those in `protect` — the one `HETZNER_IMAGE=arena` resolves to now, or
/// the one HETZNER_IMAGE names by id: `(kept, deleted)`, newest first. `--keep 0` deletes them
/// all. (Leftover build VMs: [`leftover_build_vms`].) Pure.
pub fn prune_plan<'a>(snaps: &'a [Snapshot], scope: &Scope, keep: usize, protect: &[u64]) -> (Vec<&'a Snapshot>, Vec<&'a Snapshot>) {
    let mut ours: Vec<&Snapshot> = snaps.iter().filter(|s| s.ours(scope)).collect();
    ours.sort_by_key(|s| std::cmp::Reverse((s.built.unwrap_or(0), s.id)));
    let (mut kept, mut gone) = (Vec::new(), Vec::new());
    for (i, s) in ours.into_iter().enumerate() {
        // `--keep 0` is the programme's end: everything goes, protected or not.
        if i < keep || (keep > 0 && protect.contains(&s.id)) {
            kept.push(s);
        } else {
            gone.push(s);
        }
    }
    (kept, gone)
}

/// A build VM older than this is left over (a bake takes ~20 min and deletes its VM, even on
/// Ctrl+C): `image prune` deletes it, `teardown --check` flags it.
pub const LEFTOVER_SECS: u64 = 3 * 3600;

/// The build VMs among `servers` that are left over at `now` (created over [`LEFTOVER_SECS`]
/// ago, or with no creation time). Pure.
pub fn leftover_build_vms(servers: &[crate::pod::Pod], now: u64) -> Vec<&crate::pod::Pod> {
    servers.iter().filter(|p| p.created_at.is_none_or(|c| now.saturating_sub(c) > LEFTOVER_SECS)).collect()
}

/// What `teardown --check` reads from the Hetzner project: every snapshot (arena's and any
/// other: a snapshot bills whoever made it), the build VMs, and the snapshot price.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inventory {
    pub snapshots: Vec<Snapshot>,
    /// Servers labelled [`BUILD_LABEL`] (any prefix).
    pub build_vms: Vec<crate::pod::Pod>,
    pub price: Option<ImagePrice>,
}

#[cfg(test)]
mod tests;
