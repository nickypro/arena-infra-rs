//! Shadeform backend (https://api.shadeform.ai/v1) — GPU **VMs** from many clouds behind one
//! API. Opt-in overflow only (`--provider shadeform`): never in the default plan chain.
//!
//! What a Shadeform pod is, and how it maps (spec `specs/shadeform.md`, read-only spike
//! 2026-10-14; GET responses recorded in `fixtures/shadeform/`):
//! - **A bare GPU VM**, like a Hetzner server with a GPU — not a container from the cohort
//!   image. arena reaches it as `root@ip:ssh_port` once its **bootstrap script**
//!   ([`render_bootstrap`], `shadeform_bootstrap.sh`, run by Shadeform when the VM turns
//!   `active`) has authorized the cohort keys for root and written the identity nonce; setup
//!   then runs the bare-VM script in its GPU variant (`setup::BareVm::Gpu`).
//! - **Placement like Vast's**: the option is a GPU type, and create picks the cheapest
//!   in-stock `(cloud, region, shade_instance_type)` catalog entry that passes the filters
//!   ([`fitting_entries`]): the `SHADEFORM_CLOUDS`/`SHADEFORM_REGIONS` allowlists, a VM (not
//!   bare metal unless allowed), on-demand only, an OS image at or above the CUDA floor
//!   ([`best_os`] — in practice Massed Compute is the only cheap CUDA-13 cloud), a bounded
//!   boot time, and the price cap: `min(--max-price, SHADEFORM_MAX_PRICE)`.
//! - **Create is async and has no idempotency key.** Every create carries a fresh nonce tag
//!   (`arena-nonce-<uuid>`); an answer that might hide an instance (a 5xx, a timeout, a 2xx
//!   without an id) is settled by **listing for that tag** — adopted if found, else the run
//!   stops with "may have been created" (never a blind retry, never the next option) —
//!   [`create_outcome`], [`find_by_nonce`]. A 200 isn't success either: the instance can go
//!   to `error` later, which `up` turns into delete + next option (`EndpointWait::Failed`).
//! - **Ownership.** One API key sees every instance on the account. arena only ever mutates
//!   an instance whose name has `MACHINE_NAME_PREFIX-` **and** whose tags carry
//!   `arena-<prefix>` ([`is_ours`]); anything else is listed (so `pods list` and `teardown`
//!   reveal a shared or wrong account) and refused, with no request, by terminate / rename /
//!   restart.
//! - **No stop/start** (it bills until deleted), **no volumes** in v1, no key-add after
//!   create, no lock. Restart is gated as a wipe until a live test shows the disk survives.
//! - **Secrets.** The key goes in the `X-API-KEY` header only (never a URL); `Debug` isn't
//!   derived on the provider. `GET /sshkeys` returns the managed key's **private half**, so
//!   it's deserialized straight into [`SshKey`] (id, name, default — nothing else is kept)
//!   and its body is never quoted. Error bodies are `text/plain` (recorded) and bounded.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::Provider;
use crate::apiextra::{self, managed, Extra, Managed};
use crate::config::Config;
use crate::error::{Error, ProviderErrorKind, Result};
use crate::fleet::fmt_money;
use crate::http::{send_json, status_error};
use crate::placement::{milli, PriceBook};
use crate::pod::{Pod, PodSpec};

pub(crate) const BASE: &str = "https://api.shadeform.ai/v1";

/// The provider name, as pods, the proxy and `--provider` spell it.
pub const NAME: &str = "shadeform";

/// The bootstrap script's template (`@KEYS@`, `@NONCE@`, `@NAME@` filled by [`render_bootstrap`]).
const BOOTSTRAP: &str = include_str!("shadeform_bootstrap.sh");

/// The rendered script's cap. Shadeform's limit for `base64_script` is unknown; the script with
/// a cohort's handful of keys is ~2 KiB, so 16 KiB is generous and still bounded.
pub const BOOTSTRAP_MAX: usize = 16 * 1024;

/// Where the bootstrap writes the per-create nonce on the VM (the reaper's identity check).
pub const NONCE_FILE: &str = "/var/lib/arena/shadeform-nonce";

/// How long after `active` the endpoint is withheld ([`parse_instance`]): the bootstrap runs
/// once the VM is active (systemd `init-script`) and needs a moment to install root's keys —
/// an endpoint handed out at once would meet a root login it refuses.
pub const BOOT_GRACE: u64 = 60;

/// How many in-stock entries one create tries in turn while each answers "no capacity" — as
/// Vast's `RENT_TRIES`.
const CREATE_TRIES: usize = 3;

/// One create POST's own bound: the client has no timeout, and a stalled create would hold
/// `up` forever. A timeout is an *unsure* answer (the instance may exist), settled by listing.
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);

/// A create answer above the quote by more than this factor is deleted at once ([`price_guard`]).
const QUOTE_SLACK: f64 = 1.10;

/// Every `configuration.gpu_type` in the recorded catalog (2026-10-14, 299 types; the CPU-only
/// types' `"CPU"` left out — never offered for a GPU spec). What an unmapped `--gpu` is told,
/// and what a name already in Shadeform's spelling is accepted as.
pub const RECORDED_GPU_TYPES: &[&str] = &[
    "A10", "A100", "A100_80G", "A16", "A30", "A40", "A4000", "A5000", "A6000", "B200", "B300", "GAUDI2", "GH200", "H100",
    "H100_nvl", "H200", "L4", "L40", "L40S", "RTX4000Ada", "RTX4090", "RTX5090", "RTX6000", "RTX6000Ada", "RTXPro6000",
    "V100", "V100_32G",
];

/// RunPod-style GPU ids (what `--gpu`/`GPU_TYPE` resolve to, `gpu::resolve`) → Shadeform's
/// `gpu_type`, as the recorded catalog spells it. Several ids can share a Shadeform type (the
/// catalog keeps the interconnect apart, in `configuration.interconnect`).
const GPU_TABLE: &[(&str, &str)] = &[
    ("NVIDIA RTX A4000", "A4000"),
    ("NVIDIA RTX A5000", "A5000"),
    ("NVIDIA RTX A6000", "A6000"),
    ("NVIDIA RTX 4000 Ada Generation", "RTX4000Ada"),
    ("NVIDIA RTX 6000 Ada Generation", "RTX6000Ada"),
    ("NVIDIA GeForce RTX 4090", "RTX4090"),
    ("NVIDIA GeForce RTX 5090", "RTX5090"),
    ("NVIDIA A10", "A10"),
    ("NVIDIA A30", "A30"),
    ("NVIDIA A40", "A40"),
    ("NVIDIA L4", "L4"),
    ("NVIDIA L40", "L40"),
    ("NVIDIA L40S", "L40S"),
    ("NVIDIA A100 80GB PCIe", "A100_80G"),
    ("NVIDIA A100-SXM4-80GB", "A100_80G"),
    ("NVIDIA A100 PCIe", "A100"),
    ("NVIDIA A100-SXM4-40GB", "A100"),
    ("NVIDIA H100 80GB HBM3", "H100"),
    ("NVIDIA H100 PCIe", "H100"),
    ("NVIDIA H100 NVL", "H100_nvl"),
    ("NVIDIA H200", "H200"),
    ("NVIDIA B200", "B200"),
    ("NVIDIA B300", "B300"),
];

/// RunPod's RTX PRO 6000 ids all start with this (Server/Workstation/Max-Q editions).
const RTX_PRO_6000: &str = "NVIDIA RTX PRO 6000 Blackwell";

/// Shadeform's `gpu_type` for a RunPod-style id (`NVIDIA RTX A6000` → `A6000`), or for a name
/// already in Shadeform's spelling (`a100_80g` → `A100_80G`). Pure.
///
/// An unmapped id is an error naming the recorded types: **never send an unmapped name** —
/// the catalog answers `200 {"instance_types": []}` to a typo (recorded), which would read as
/// "no stock" forever.
pub fn shadeform_gpu_type(gpu: &str) -> Result<String> {
    let g = gpu.trim();
    if let Some((_, sf)) = GPU_TABLE.iter().find(|(id, _)| id.eq_ignore_ascii_case(g)) {
        return Ok((*sf).to_string());
    }
    if g.len() >= RTX_PRO_6000.len() && g[..RTX_PRO_6000.len()].eq_ignore_ascii_case(RTX_PRO_6000) {
        return Ok("RTXPro6000".into());
    }
    if let Some(sf) = RECORDED_GPU_TYPES.iter().find(|t| t.eq_ignore_ascii_case(g)) {
        return Ok((*sf).to_string());
    }
    Err(Error::Config(format!(
        "--gpu `{g}` has no Shadeform GPU type — pass one of: {} (Shadeform answers an unknown name with \
         an empty catalog, so it's refused here)",
        RECORDED_GPU_TYPES.join(", ")
    )))
}

/// The RunPod-style id for a Shadeform `gpu_type` (and its `interconnect`), so `pods list`
/// labels a Shadeform A6000 as it labels a RunPod one. `None` = no mapping. Pure.
pub fn runpod_gpu_id(sf_type: &str, interconnect: Option<&str>) -> Option<&'static str> {
    let sxm = interconnect.is_some_and(|i| i.to_ascii_lowercase().contains("sxm"));
    match sf_type {
        "A100_80G" => Some(if sxm { "NVIDIA A100-SXM4-80GB" } else { "NVIDIA A100 80GB PCIe" }),
        "A100" => Some(if sxm { "NVIDIA A100-SXM4-40GB" } else { "NVIDIA A100 PCIe" }),
        "H100" => Some(if sxm { "NVIDIA H100 80GB HBM3" } else { "NVIDIA H100 PCIe" }),
        "RTXPro6000" => Some("NVIDIA RTX PRO 6000 Blackwell Server Edition"),
        other => GPU_TABLE.iter().find(|(_, sf)| *sf == other).map(|(id, _)| *id),
    }
}

/// Which underlying clouds create may use (`SHADEFORM_CLOUDS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clouds {
    /// These cloud ids only (lowercase).
    Only(Vec<String>),
    /// `*`: any cloud — `config check` warns (clouds differ in ports, images, drivers, boot).
    All,
}

impl Clouds {
    fn admits(&self, cloud: &str) -> bool {
        match self {
            Clouds::All => true,
            Clouds::Only(list) => list.iter().any(|c| c.eq_ignore_ascii_case(cloud)),
        }
    }

    /// `massedcompute`, `massedcompute, hyperstack`, `any cloud`.
    pub fn label(&self) -> String {
        match self {
            Clouds::All => "any cloud".into(),
            Clouds::Only(list) => list.join(", "),
        }
    }
}

/// Shadeform's create options, from `SHADEFORM_*` config (validated once: [`Self::from_config`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ShadeformOpts {
    pub clouds: Clouds,
    /// `SHADEFORM_REGIONS`: empty = any region.
    pub regions: Vec<String>,
    /// `SHADEFORM_MAX_PRICE`: the hard per-pod $/h ceiling under any `--max-price`.
    pub max_price: f64,
    /// `SHADEFORM_SSH_KEY`: the uploaded key's name; `default` = the account's default key.
    pub ssh_key: String,
    /// `SHADEFORM_MAX_BOOT_SECS`: skip entries whose `boot_time.max_boot_in_sec` is larger.
    pub max_boot_secs: u64,
    /// `SHADEFORM_READY_MINS`: create → `active` with an endpoint, per attempt.
    pub ready_mins: u64,
    /// `SHADEFORM_ALLOW_BAREMETAL`: allow `deployment_type: baremetal`.
    pub allow_baremetal: bool,
    /// `MACHINE_NAME_PREFIX`: whose instances are arena's ([`is_ours`]).
    pub prefix: String,
}

/// `SHADEFORM_SSH_KEY`'s value for "the account's default key" (the Shadeform Managed Key on a
/// fresh account — whose private half Shadeform holds and returns from `GET /sshkeys`).
pub const DEFAULT_KEY: &str = "default";

/// The SHADEFORM_* keys `from_config` reads (besides the API key) — `config check` shows them.
pub const OPTION_KEYS: [&str; 7] = [
    "SHADEFORM_CLOUDS",
    "SHADEFORM_REGIONS",
    "SHADEFORM_MAX_PRICE",
    "SHADEFORM_SSH_KEY",
    "SHADEFORM_MAX_BOOT_SECS",
    "SHADEFORM_READY_MINS",
    "SHADEFORM_ALLOW_BAREMETAL",
];

impl Default for ShadeformOpts {
    fn default() -> Self {
        Self {
            clouds: Clouds::Only(vec!["massedcompute".into()]),
            regions: Vec::new(),
            max_price: 1.0,
            ssh_key: "arena".into(),
            max_boot_secs: 1200,
            ready_mins: 15,
            allow_baremetal: false,
            prefix: "arena".into(),
        }
    }
}

/// A config value for an error message: quoted when short, else only its length — a value
/// pasted into the wrong key may be a secret, and these errors reach every fleet command's
/// output and cron logs.
fn shown(raw: &str) -> String {
    if raw.chars().count() <= 12 {
        format!("`{raw}`")
    } else {
        format!("a value of {} chars, not shown", raw.chars().count())
    }
}

/// A comma list of ids (`[a-z0-9_.-]`, lowercased), or the error for `key`.
fn id_list(key: &str, raw: &str) -> Result<Vec<String>> {
    let items: Vec<String> = raw.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    for it in &items {
        if !it.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(Error::Config(format!("{key} must be a comma list of ids like `massedcompute` (got {})", shown(it))));
        }
    }
    Ok(items)
}

impl ShadeformOpts {
    /// Read and validate every `SHADEFORM_*` option (the API key aside). A bad value is a
    /// config error naming the key — never a silent default — and is checked by `build_fleet`
    /// even when Shadeform isn't the primary, so a typo can't make the Shadeform fleet vanish
    /// from `pods list` (secondary backends that fail to build are dropped quietly).
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let get = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty());
        let d = Self::default();
        let clouds = match get("SHADEFORM_CLOUDS") {
            None => d.clouds,
            Some("*") => Clouds::All,
            Some(raw) => match id_list("SHADEFORM_CLOUDS", raw)? {
                list if list.is_empty() => {
                    return Err(Error::Config("SHADEFORM_CLOUDS lists no cloud (e.g. `massedcompute`, or `*`)".into()))
                }
                list => Clouds::Only(list),
            },
        };
        let regions = get("SHADEFORM_REGIONS").map(|r| id_list("SHADEFORM_REGIONS", r)).transpose()?.unwrap_or_default();
        let max_price = match get("SHADEFORM_MAX_PRICE") {
            None => d.max_price,
            Some(raw) => raw
                .trim_start_matches('$')
                .parse::<f64>()
                .ok()
                .filter(|p| p.is_finite() && *p > 0.0 && *p <= 1000.0)
                .ok_or_else(|| Error::Config(format!("SHADEFORM_MAX_PRICE must be a $/h above 0, e.g. 1.00 (got {})", shown(raw))))?,
        };
        let whole = |key: &str, default: u64, max: u64| -> Result<u64> {
            match get(key) {
                None => Ok(default),
                Some(raw) => raw.parse::<u64>().ok().filter(|n| (1..=max).contains(n)).ok_or_else(|| {
                    Error::Config(format!("{key} must be a whole number from 1 to {max} (got {})", shown(raw)))
                }),
            }
        };
        let max_boot_secs = whole("SHADEFORM_MAX_BOOT_SECS", d.max_boot_secs, 86_400)?;
        let ready_mins = whole("SHADEFORM_READY_MINS", d.ready_mins, 1_440)?;
        let allow_baremetal = match get("SHADEFORM_ALLOW_BAREMETAL").map(str::to_ascii_lowercase).as_deref() {
            None | Some("0" | "false" | "no") => false,
            Some("1" | "true" | "yes") => true,
            Some(other) => {
                return Err(Error::Config(format!(
                    "SHADEFORM_ALLOW_BAREMETAL must be 1/true/yes or 0/false/no (got {})",
                    shown(other)
                )))
            }
        };
        let prefix = cfg.get("MACHINE_NAME_PREFIX").map(str::trim).filter(|p| !p.is_empty()).unwrap_or("arena").to_string();
        // The cohort key, named after the prefix (as HETZNER_SSH_KEY), unless named otherwise.
        let ssh_key = get("SHADEFORM_SSH_KEY").unwrap_or(&prefix).to_string();
        if ssh_key.chars().any(char::is_control) || ssh_key.chars().count() > 128 {
            return Err(Error::Config("SHADEFORM_SSH_KEY must be the name of an SSH key on the Shadeform account".into()));
        }
        Ok(Self { clouds, regions, max_price, ssh_key, max_boot_secs, ready_mins, allow_baremetal, prefix })
    }

    /// The cap a create honours: `--max-price` when given, never above `SHADEFORM_MAX_PRICE`.
    pub fn cap(&self, spec_max: Option<f64>) -> f64 {
        spec_max.map_or(self.max_price, |m| m.min(self.max_price))
    }
}

// ---------------------------------------------------------------------------------------
// Ownership: tags and names
// ---------------------------------------------------------------------------------------

/// A tag-safe spelling: lowercase `[a-z0-9-]` (other characters → `-`). Shadeform's tag
/// charset limits are unverified, so tags stay inside the safest set.
fn tag_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

/// The tag every arena instance of this cohort carries: `arena-<prefix>`.
pub fn prefix_tag(prefix: &str) -> String {
    format!("arena-{}", tag_safe(prefix))
}

/// The per-create nonce tag (`arena-nonce-<uuid>`).
pub fn nonce_tag(nonce: &str) -> String {
    format!("arena-nonce-{nonce}")
}

/// The tags a create sets: `arena`, `arena-<prefix>`, `arena-nonce-<uuid>`. Pure.
pub fn ownership_tags(prefix: &str, nonce: &str) -> Vec<String> {
    vec!["arena".into(), prefix_tag(prefix), nonce_tag(nonce)]
}

fn tags_of(v: &Value) -> Vec<&str> {
    v.get("tags").and_then(Value::as_array).map(|t| t.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// Whether arena may act on this instance: its name is `<prefix>-…` **and** it carries the
/// `arena-<prefix>` tag. Names alone aren't enough — an account (or key) can be shared, and
/// names may not be unique (unverified) — and tags alone aren't either (a rename outside
/// arena moves it off the cohort's names). Pure.
pub fn is_ours(v: &Value, prefix: &str) -> bool {
    let name = v.get("name").and_then(Value::as_str).unwrap_or("");
    let want = prefix_tag(prefix);
    name.starts_with(&format!("{prefix}-")) && tags_of(v).contains(&want.as_str())
}

/// The listed instance carrying `tag` (a create's nonce), if any. Pure.
pub fn find_by_nonce<'a>(instances: &'a [Value], tag: &str) -> Option<&'a Value> {
    instances.iter().find(|v| tags_of(v).contains(&tag))
}

/// A fresh nonce: a UUID v4 from the OS's randomness (`/dev/urandom`), else from std's
/// randomly keyed hasher mixed with the time and pid — it must differ per create, not be secret.
pub fn new_nonce() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if read.is_err() {
        use std::hash::{BuildHasher, Hasher};
        for (i, chunk) in b.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            h.write_u32(std::process::id());
            h.write_usize(i);
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Whether `id` is a Shadeform instance id (a UUID: 8-4-4-4-12 hex) — checked before any
/// request, so a Vast/Hetzner numeric id (or anything else) can never be sent here. Pure.
pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn require_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::Config(format!("`{}` isn't a Shadeform instance id (a UUID) — nothing sent", id.chars().take(64).collect::<String>())))
    }
}

/// The refusal for an instance arena didn't create (for this prefix).
fn foreign_refusal(v: &Value, prefix: &str, what: &str) -> Error {
    let name = v.get("name").and_then(Value::as_str).unwrap_or("(unnamed)");
    Error::provider(format!(
        "`{name}` on Shadeform wasn't created by arena for prefix `{prefix}` — not {what}; delete it in the \
         Shadeform console if it's yours"
    ))
}

// ---------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------

/// The create-time catalog query: one GPU type and count, in stock, cheapest first.
pub fn catalog_path(gpu_type: &str, n: u32) -> String {
    let enc: String = gpu_type.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    format!("/instances/types?gpu_type={enc}&num_gpus={n}&available=true&sort=price")
}

/// The plan-time query: everything in stock, cheapest first (one request per `offers` run).
pub const AVAILABLE_PATH: &str = "/instances/types?available=true&sort=price";

/// The `instance_types` array of a 2xx catalog body; anything else is schema drift and an
/// error — read as "none", it would become capacity and placement would wait on a broken
/// endpoint.
pub fn instance_types(body: &Value) -> Result<&Vec<Value>> {
    body.get("instance_types").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("shadeform catalog: unexpected response shape (no instance_types array): {}", super::body_excerpt(body)))
    })
}

/// One `(cloud, region, type)` arena could create, distilled from a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub cloud: String,
    pub region: String,
    pub shade_instance_type: String,
    /// Shadeform's `gpu_type` and count.
    pub gpu_type: String,
    pub num_gpus: u32,
    /// The OS image to ask for ([`best_os`]).
    pub os: String,
    /// `hourly_price`: an integer in **cents** (57 = $0.57/h).
    pub price_cents: u64,
    /// `boot_time.max_boot_in_sec`; `None` = not given.
    pub boot_max: Option<u64>,
}

impl Entry {
    /// $/h for the whole instance.
    pub fn price(&self) -> f64 {
        self.price_cents as f64 / 100.0
    }

    /// `massedcompute/kansascity-usa-1/A6000`.
    pub fn label(&self) -> String {
        format!("{}/{}/{}", self.cloud, self.region, self.shade_instance_type)
    }
}

/// A `major.minor` version (an Ubuntu release, a CUDA version), compared numerically — as
/// [`crate::pod::cuda_key`] reads one.
type Version = (u32, u32);

/// `ubuntu22.04_cuda13.0_shade_os` → (ubuntu (22, 4), CUDA (13, 0)); `ubuntu22.04` → no CUDA.
/// `None` for anything else (it isn't asked for). Pure.
pub fn parse_os(s: &str) -> Option<(Version, Option<Version>)> {
    let rest = s.trim().strip_prefix("ubuntu")?;
    let (ver, rest) = rest.split_once('_').unwrap_or((rest, ""));
    let ubuntu = crate::pod::cuda_key(ver)?;
    if rest.is_empty() {
        return Some((ubuntu, None));
    }
    let rest = rest.strip_prefix("cuda")?;
    let (cuda, tail) = rest.split_once('_').unwrap_or((rest, ""));
    if !(tail.is_empty() || tail == "shade_os") {
        return None;
    }
    Some((ubuntu, Some(crate::pod::cuda_key(cuda)?)))
}

/// The OS image to create with from a type's `os_options`: with a CUDA floor, only images
/// whose CUDA is at or above it (a plain `ubuntuNN.NN` never: its driver is unknown); with
/// none, any — a CUDA image first, a plain Ubuntu as the fallback. Highest CUDA first, then
/// the newest Ubuntu. `None` = nothing suitable. Pure.
pub fn best_os(options: &[String], floor: Option<(u32, u32)>) -> Option<String> {
    options
        .iter()
        .filter_map(|o| parse_os(o).map(|(u, c)| (o, u, c)))
        .filter(|(_, _, c)| floor.is_none_or(|f| c.is_some_and(|c| c >= f)))
        .max_by_key(|(_, u, c)| (c.is_some(), *c, *u))
        .map(|(o, _, _)| o.clone())
}

/// The spec's CUDA floor as a version key (`MIN_CUDA_VERSION` / `ALLOWED_CUDA_VERSIONS`).
fn floor_of(spec: &PodSpec) -> Option<(u32, u32)> {
    spec.cuda_floor().and_then(|v| crate::pod::cuda_key(&v))
}

/// The catalog entries `spec` may be created on, cheapest first (then the shortest worst-case
/// boot, then the catalog's order), every condition checked client-side — the query's own
/// filters are a hint, renting the wrong VM costs money:
/// - the cloud in `SHADEFORM_CLOUDS`, the region in `SHADEFORM_REGIONS` (when set);
/// - a VM (`deployment_type: vm`; bare metal only with `SHADEFORM_ALLOW_BAREMETAL`);
/// - exactly the spec's GPU type ([`shadeform_gpu_type`]) and count;
/// - an availability entry that is on-demand (missing = on-demand; spot is preemptible and
///   priced in another unit) and available;
/// - an OS image at or above the CUDA floor ([`best_os`]);
/// - `boot_time.max_boot_in_sec ≤ SHADEFORM_MAX_BOOT_SECS`;
/// - `hourly_price` (cents) > 0 and within `min(--max-price, SHADEFORM_MAX_PRICE)`;
/// - not a `(cloud, region)` in `avoid` (one that just failed this run).
///
/// An unmapped GPU is an error (never sent). Pure, so it's tested against the recorded catalog.
pub fn fitting_entries(types: &[Value], spec: &PodSpec, opts: &ShadeformOpts, avoid: &[(String, String)]) -> Result<Vec<Entry>> {
    let want = shadeform_gpu_type(&spec.gpu_type)?;
    let floor = floor_of(spec);
    let cap = opts.cap(spec.max_price);
    let mut out: Vec<(usize, Entry)> = Vec::new();
    for (i, t) in types.iter().enumerate() {
        let s = |k: &str| t.get(k).and_then(Value::as_str);
        let conf = t.get("configuration");
        let cs = |k: &str| conf.and_then(|c| c.get(k)).and_then(Value::as_str);
        let (Some(cloud), Some(kind), Some(shade_type)) = (s("cloud"), s("deployment_type"), s("shade_instance_type")) else {
            continue;
        };
        let kind_ok = kind == "vm" || (kind == "baremetal" && opts.allow_baremetal);
        let gpus = conf.and_then(|c| c.get("num_gpus")).and_then(Value::as_u64);
        if !opts.clouds.admits(cloud) || !kind_ok || cs("gpu_type") != Some(want.as_str()) || gpus != Some(u64::from(spec.gpu_count)) {
            continue;
        }
        let Some(price_cents) = t.get("hourly_price").and_then(Value::as_u64).filter(|c| *c > 0) else { continue };
        if milli(price_cents as f64 / 100.0) > milli(cap) {
            continue;
        }
        let boot_max = t.get("boot_time").and_then(|b| b.get("max_boot_in_sec")).and_then(Value::as_u64);
        if boot_max.is_some_and(|b| b > opts.max_boot_secs) {
            continue;
        }
        let options: Vec<String> = conf
            .and_then(|c| c.get("os_options"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();
        let Some(os) = best_os(&options, floor) else { continue };
        for a in t.get("availability").and_then(Value::as_array).into_iter().flatten() {
            let on_demand = a.get("rental_type").and_then(Value::as_str).is_none_or(|r| r == "on_demand");
            let available = a.get("available").and_then(Value::as_bool) == Some(true);
            let Some(region) = a.get("region").and_then(Value::as_str) else { continue };
            let region_ok = opts.regions.is_empty() || opts.regions.iter().any(|r| r.eq_ignore_ascii_case(region));
            let avoided = avoid.iter().any(|(c, r)| c == cloud && r == region);
            if on_demand && available && region_ok && !avoided {
                out.push((
                    i,
                    Entry {
                        cloud: cloud.to_string(),
                        region: region.to_string(),
                        shade_instance_type: shade_type.to_string(),
                        gpu_type: want.clone(),
                        num_gpus: spec.gpu_count,
                        os: os.clone(),
                        price_cents,
                        boot_max,
                    },
                ));
            }
        }
    }
    out.sort_by_key(|(i, e)| (e.price_cents, e.boot_max.unwrap_or(u64::MAX), *i));
    Ok(out.into_iter().map(|(_, e)| e).collect())
}

/// One GPU option's plan-time quote: its cheapest fitting entry (`None`: none in stock), or
/// why it couldn't be priced.
pub type EntryQuote = std::result::Result<Option<Entry>, String>;

/// Placement's view of [`ShadeformProvider::quote`]: each option priced at its cheapest
/// in-stock fitting entry, and a note for every option left unpriced (no stock right now, an
/// unmapped GPU, a failed catalog read). Pure.
pub fn price_book(quotes: &[(String, EntryQuote)], gpu_count: u32, opts: &ShadeformOpts) -> (PriceBook, Vec<String>) {
    let mut prices = Vec::new();
    let mut notes = vec![format!(
        "shadeform prices are the cheapest in-stock VM right now on {} (on-demand, an OS image at the CUDA floor, \
         ≤ {}/h SHADEFORM_MAX_PRICE); each create reads the catalog again and takes the cheapest entry then — \
         never above --max-price. Shadeform has no stop: a pod bills until terminated",
        opts.clouds.label(),
        fmt_money("$", opts.max_price)
    )];
    for (gpu, quote) in quotes {
        let what = format!("{gpu_count}×{}", shadeform_gpu_type(gpu).unwrap_or_else(|_| gpu.clone()));
        match quote {
            Ok(Some(e)) => {
                prices.push((gpu.clone(), e.price()));
                notes.push(format!("shadeform {what}: {} at {}", e.label(), fmt_money("$", e.price())));
            }
            Ok(None) => notes.push(format!("shadeform: no in-stock {what} on {} right now — unpriced (no stock)", opts.clouds.label())),
            Err(e) => notes.push(format!("shadeform: pricing {what} failed ({e}) — unpriced")),
        }
    }
    (PriceBook::offers(prices, gpu_count), notes)
}

// ---------------------------------------------------------------------------------------
// The bootstrap script
// ---------------------------------------------------------------------------------------

/// Whether `key` is one OpenSSH public key line: a known type, a base64 blob, an optional
/// comment — no `'`, no newline, no control character (it's single-quoted into a shell script).
pub fn valid_pubkey(key: &str) -> bool {
    const TYPES: [&str; 6] = [
        "ssh-ed25519",
        "ssh-rsa",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
        "sk-ssh-ed25519@openssh.com",
    ];
    if key.contains('\'') || key.chars().any(char::is_control) {
        return false;
    }
    let mut parts = key.splitn(3, ' ');
    let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else { return false };
    TYPES.contains(&kind) && !blob.is_empty() && blob.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

/// The bootstrap script for one create, from the template: `keys` (one OpenSSH public key
/// each — the cohort's, from `PUBLIC_KEY`), the create's nonce and the machine name. Every
/// value is checked before it goes into the script ([`valid_pubkey`]; the nonce is a UUID;
/// the name `[A-Za-z0-9._-]`), and the result is capped at [`BOOTSTRAP_MAX`]. Public data only.
/// Pure.
pub fn render_bootstrap(keys: &[String], nonce: &str, name: &str) -> Result<String> {
    for (i, k) in keys.iter().enumerate() {
        if !valid_pubkey(k) {
            return Err(Error::Config(format!(
                "SSH key #{} isn't one OpenSSH public key line (`<type> <base64> [comment]`, no quotes or newlines) — \
                 refusing to write it into the Shadeform bootstrap script",
                i + 1
            )));
        }
    }
    if !valid_id(nonce) {
        return Err(Error::Config("the Shadeform create nonce isn't a UUID".into()));
    }
    if !valid_name(name) {
        return Err(Error::Config(format!("machine name `{name}` can't go into the Shadeform bootstrap script (use [A-Za-z0-9._-])")));
    }
    let script = BOOTSTRAP.replace("@KEYS@", &keys.join("\n")).replace("@NONCE@", nonce).replace("@NAME@", name);
    if script.len() > BOOTSTRAP_MAX {
        return Err(Error::Config(format!(
            "the Shadeform bootstrap script would be {} bytes (cap {BOOTSTRAP_MAX}) — too many SSH keys",
            script.len()
        )));
    }
    Ok(script)
}

/// Whether `name` is a machine name arena writes into a script and onto Shadeform: 1–63 of
/// `[A-Za-z0-9._-]`. Pure.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The cohort's public keys from the spec: `PUBLIC_KEY` holds them newline-joined
/// (`PodSpec::from_config`). Trimmed, deduped, in order.
fn spec_pubkeys(spec: &PodSpec) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (_, v) in spec.env.iter().filter(|(k, _)| k == "PUBLIC_KEY") {
        for k in v.lines().map(str::trim).filter(|k| !k.is_empty()) {
            if !keys.iter().any(|have| have == k) {
                keys.push(k.to_string());
            }
        }
    }
    keys
}

// ---------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------

/// The create-body fields arena sets itself, refused in `--api-json` / `CREATE_EXTRA_JSON`.
/// `auto_delete`, `alert`, `envs` and `volume_mount` pass through (`auto_delete` is the live
/// tests' backstop: Shadeform deletes the box itself after a time or spend).
const MANAGED: &[Managed] = &[
    managed(&["name"], "the machine name"),
    managed(&["cloud"], "placement (SHADEFORM_CLOUDS)"),
    managed(&["region"], "placement (SHADEFORM_REGIONS)"),
    managed(&["shade_instance_type"], "placement (--gpu)"),
    managed(&["shade_cloud"], "always true: arena bills through Shadeform, never a linked cloud account"),
    managed(&["rental_type"], "always on-demand: spot is preemptible"),
    managed(&["os"], "the CUDA floor (MIN_CUDA_VERSION / ALLOWED_CUDA_VERSIONS)"),
    managed(&["ssh_key_id"], "SHADEFORM_SSH_KEY"),
    managed(&["tags"], "arena's ownership and identity tags"),
    managed(&["launch_configuration"], "the bootstrap script (root keys + identity nonce)"),
    managed(&["template_id"], "the fields above — a template would override them"),
    managed(&["volume_ids"], "volumes are unsupported in v1"),
];

/// The `POST /instances/create` body for `spec` on `entry`, with the nonce's tag and the
/// rendered bootstrap (base64); `key_id` = the uploaded cohort key (`None` = the account
/// default). `spec.api_extra` is deep-merged in last, minus [`MANAGED`]. Pure.
pub fn create_payload(opts: &ShadeformOpts, spec: &PodSpec, entry: &Entry, nonce: &str, key_id: Option<&str>) -> Result<Value> {
    let script = render_bootstrap(&spec_pubkeys(spec), nonce, &spec.name)?;
    let mut body = json!({
        "cloud": entry.cloud,
        "region": entry.region,
        "shade_instance_type": entry.shade_instance_type,
        "shade_cloud": true,
        "rental_type": "on_demand",
        "name": spec.name,
        "os": entry.os,
        "tags": ownership_tags(&opts.prefix, nonce),
        "launch_configuration": {"type": "script", "script_configuration": {"base64_script": crate::base64::encode(script.as_bytes())}},
    });
    if let Some(id) = key_id {
        body["ssh_key_id"] = json!(id);
    }
    apiextra::apply(&mut body, spec.api_extra.as_ref(), MANAGED, None)?;
    Ok(body)
}

/// What a create answer means for the instance's existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    /// It exists: its id.
    Id(String),
    /// It may exist: settle by listing for the nonce ([`find_by_nonce`]). Why it's unsure.
    Unsure(String),
}

/// Whether a refusal names a balance/wallet problem (Shadeform's wording is unverified;
/// these are the words a prepaid wallet's refusal would use).
fn wallet_words(text: &str) -> bool {
    let m = text.to_lowercase();
    ["balance", "funds", "wallet", "payment", "insufficient credit", "top up", "top-up"].iter().any(|w| m.contains(w))
}

/// Whether a 4xx says this region/type has no capacity right now (unverified wording: the
/// shared capacity phrases, plus "… unavailable"/"out of capacity" about the region or type).
fn capacity_words(text: &str) -> bool {
    let m = text.to_lowercase();
    crate::error::looks_like_capacity(&m)
        || m.contains("out of capacity")
        || ((m.contains("region") || m.contains("instance type") || m.contains("instance_type"))
            && (m.contains("unavailable") || m.contains("not available")))
}

/// A bounded excerpt of a `text/plain` (or JSON) answer for a message.
fn excerpt(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() > 160 {
        format!("{}…", t.chars().take(160).collect::<String>())
    } else {
        t.to_string()
    }
}

/// What a `POST /instances/create` answer means. Pure, table-tested.
/// - 2xx with a UUID `id` → [`Created::Id`];
/// - 2xx without one, or a body that isn't JSON → [`Created::Unsure`];
/// - 401/403/429 → by status (Auth / RateLimited), whatever the body says;
/// - 5xx → [`Created::Unsure`]: the record may exist (create is async and logs it first);
/// - a 4xx about the wallet/balance → `Denied` ("fund the Shadeform wallet");
/// - a 4xx about capacity → `Capacity` (the next entry/option);
/// - any other 4xx → `Other` (the run stops).
///
/// The capacity and wallet wordings are unverified (to record on the first live failure).
pub fn create_outcome(status: StatusCode, text: &str) -> Result<Created> {
    const CTX: &str = "shadeform create";
    if status.is_success() {
        let id = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::trim).map(String::from))
            .filter(|id| valid_id(id));
        return Ok(match id {
            Some(id) => Created::Id(id),
            None => Created::Unsure(format!("HTTP {status} without an instance id: {}", excerpt(text))),
        });
    }
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return Err(status_error(status, text, CTX));
    }
    if status.is_server_error() {
        return Ok(Created::Unsure(format!("HTTP {status}: {}", excerpt(text))));
    }
    if wallet_words(text) {
        return Err(Error::Provider {
            kind: ProviderErrorKind::Denied,
            message: format!("{CTX} refused (HTTP {status}): {} — fund the Shadeform wallet (console → Wallet)", excerpt(text)),
        });
    }
    if capacity_words(text) {
        return Err(Error::capacity(format!("{CTX}: no capacity (HTTP {status}): {}", excerpt(text))));
    }
    Err(Error::provider(format!("{CTX} refused (HTTP {status}): {}", excerpt(text))))
}

/// The "maybe created" stop: never retried, never the next option — a retry could make a
/// second (billing) instance for the same name. Same contract as Vast's `rent_unsure`.
fn create_unsure(entry: &Entry, name: &str, why: &str) -> Error {
    Error::provider(format!(
        "shadeform create ({}): {why} — the instance may have been created anyway: check `arena pods list \
         --provider shadeform` for {name} before creating it again",
        entry.label()
    ))
}

/// A create whose request failed in transport: only a failure to build it or to *connect*
/// proves nothing reached Shadeform (the plain transport error, retryable); anything else — a
/// timeout, a dropped connection — leaves the create's fate unknown.
fn create_send_error(e: reqwest::Error) -> std::result::Result<Error, String> {
    if e.is_connect() || e.is_builder() {
        Ok(Error::Http(e))
    } else if e.is_timeout() {
        Err(format!("no answer within {}s", CREATE_TIMEOUT.as_secs()))
    } else {
        Err("the connection failed after the request was sent".to_string())
    }
}

/// The post-create price guard: `Some(why)` when the instance's price is above the cap
/// (`min(--max-price, SHADEFORM_MAX_PRICE)`) or more than [`QUOTE_SLACK`] above the quote —
/// then it's deleted at once. Pure.
pub fn price_guard(charged: f64, quoted: f64, cap: f64) -> Option<String> {
    let over_cap = milli(charged) > milli(cap);
    let over_quote = milli(charged) > milli(quoted * QUOTE_SLACK);
    (over_cap || over_quote).then(|| {
        format!(
            "Shadeform charged {}/h, quoted {}/h (cap {}/h)",
            fmt_money("$", charged),
            fmt_money("$", quoted),
            fmt_money("$", cap)
        )
    })
}

// ---------------------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------------------

/// The `instances` array of a 2xx `GET /instances` body. Anything else is schema drift and an
/// error — an empty-by-accident list would read as "every Shadeform pod terminated" to the
/// proxy merge. Fail closed.
pub fn instances_array(body: &Value) -> Result<&Vec<Value>> {
    body.get("instances").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("shadeform list: unexpected response shape (no instances array): {}", super::body_excerpt(body)))
    })
}

/// Shadeform's status → arena's word (so `status::is_billing`, `wait_gone`, teardown and the
/// proxy read it as they read every provider's). `deleting` already stops billing and the
/// instance is on its way out: `TERMINATED`. Pure.
pub fn map_status(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "creating" => "CREATING".into(),
        "pending_provider" => "PROVISIONING".into(),
        "pending" => "PENDING".into(),
        "active" => "RUNNING".into(),
        "error" => "ERROR".into(),
        "deleting" | "deleted" => "TERMINATED".into(),
        other => other.to_uppercase(),
    }
}

/// The instance's $/h: `rental_terms.hourly_price` (a dollar string) when present, else
/// `hourly_price` (integer cents). Two units in one API — never mixed.
fn price_of(v: &Value) -> Option<f64> {
    let dollars = v
        .get("rental_terms")
        .and_then(|r| r.get("hourly_price"))
        .and_then(|p| p.as_str().map(|s| s.trim().parse::<f64>().ok()).unwrap_or_else(|| p.as_f64()));
    dollars.or_else(|| v.get("hourly_price").and_then(Value::as_u64).map(|c| c as f64 / 100.0)).filter(|p| p.is_finite() && *p >= 0.0)
}

fn time_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_str).and_then(crate::reap::parse_rfc3339).and_then(|t| u64::try_from(t).ok())
}

/// One listed (or `info`) instance as a [`Pod`], at `now` (unix seconds). Pure.
///
/// The endpoint (`ip`, `ssh_port` or 22) is given only when the instance is `active` **and**
/// has been for [`BOOT_GRACE`]: an IP can appear while it's still pending, and the bootstrap
/// installs root's keys only once it's active. An `active` instance with no readable
/// `active_at` gets its endpoint (nothing to wait out). `ip` may be a hostname.
pub fn parse_instance(v: &Value, now: u64) -> Pod {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let conf = v.get("configuration");
    let raw_status = s("status").unwrap_or("unknown");
    let active = raw_status.eq_ignore_ascii_case("active");
    let settled = time_of(v, "active_at").is_none_or(|at| now >= at.saturating_add(BOOT_GRACE));
    let ip = s("ip").map(str::trim).filter(|ip| !ip.is_empty()).filter(|_| active && settled);
    let port = v.get("ssh_port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0).unwrap_or(22);
    let gpu = conf.and_then(|c| c.get("gpu_type")).and_then(Value::as_str);
    let interconnect = conf.and_then(|c| c.get("interconnect")).and_then(Value::as_str);
    let gpu_type = gpu.filter(|g| *g != "CPU").map(|g| runpod_gpu_id(g, interconnect).map_or_else(|| format!("{g} (shadeform)"), String::from));
    let identity = tags_of(v).iter().find_map(|t| t.strip_prefix("arena-nonce-")).filter(|n| !n.is_empty()).map(String::from);
    Pod {
        id: s("id").unwrap_or("").to_string(),
        name: s("name").unwrap_or("").to_string(),
        provider: NAME.into(),
        status: map_status(raw_status),
        gpu_type,
        gpu_count: conf.and_then(|c| c.get("num_gpus")).and_then(Value::as_u64).filter(|n| *n > 0).and_then(|n| u32::try_from(n).ok()),
        cost_per_hr: price_of(v),
        ssh_ip: ip.map(String::from),
        ssh_port: ip.map(|_| port),
        maintenance: None,
        // Shadeform exposes no host id (`cloud_assigned_id` is the cloud's id of the VM).
        machine_id: None,
        // No lock and no delete protection on Shadeform: known, not unknown.
        locked: None,
        proxy_only: false,
        // The disk bills with the instance (nothing is ever stopped): not priced apart.
        disk_gb: None,
        volume_gb: None,
        delete_protected: None,
        created_at: time_of(v, "created_at"),
        started_at: None,
        identity,
        status_detail: s("status_details").map(str::trim).filter(|d| !d.is_empty()).map(String::from),
        // `cloud region` (`massedcompute us-central-1`): the region alone doesn't say whose.
        location: match (s("cloud").map(str::trim).filter(|c| !c.is_empty()), s("region").map(str::trim).filter(|r| !r.is_empty())) {
            (Some(c), Some(r)) => Some(format!("{c} {r}")),
            (None, Some(r)) => Some(r.to_string()),
            _ => None,
        },
    }
}

/// One volume on the account (`GET /volumes`), priced from `GET /volumes/types` — for
/// `teardown --check` (review SF7): arena never creates one (`VOLUME_GB>0` and `volume_ids` are
/// refused), but the API key is account-wide, and a volume made in the console bills per
/// GB-month after every instance is gone.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadeformVolume {
    pub id: String,
    pub name: String,
    pub cloud: String,
    pub region: String,
    /// `size_in_gb`; `None` if not given.
    pub size_gb: Option<u32>,
    /// The listed `price_per_gb_per_hour` for its (cloud, region); `None` if unknown.
    pub usd_per_gb_hour: Option<f64>,
}

impl ShadeformVolume {
    /// $/month at the listed rate (730 h), when size and rate are known.
    pub fn usd_per_month(&self) -> Option<f64> {
        Some(f64::from(self.size_gb?) * self.usd_per_gb_hour? * 730.0)
    }
}

/// The volumes of a 2xx `GET /volumes` body, each priced from a `GET /volumes/types` body when
/// there is one. No `volumes` array is schema drift and an error — never "none". Pure.
pub fn parse_volumes(volumes: &Value, types: Option<&Value>) -> Result<Vec<ShadeformVolume>> {
    let list = volumes.get("volumes").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("shadeform volumes: unexpected response shape (no volumes array): {}", super::body_excerpt(volumes)))
    })?;
    let rate = |cloud: &str, region: &str| -> Option<f64> {
        types?.get("volume_types")?.as_array()?.iter().find_map(|t| {
            let same = t.get("cloud").and_then(Value::as_str) == Some(cloud) && t.get("region").and_then(Value::as_str) == Some(region);
            let p = t.get("price_per_gb_per_hour")?;
            same.then(|| p.as_str().and_then(|s| s.trim().parse::<f64>().ok()).or_else(|| p.as_f64())).flatten()
        })
    };
    Ok(list
        .iter()
        .map(|v| {
            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let (cloud, region) = (s("cloud"), s("region"));
            ShadeformVolume {
                id: s("id"),
                name: s("name"),
                size_gb: v.get("size_in_gb").and_then(Value::as_u64).and_then(|g| u32::try_from(g).ok()),
                usd_per_gb_hour: rate(&cloud, &region).filter(|r| r.is_finite() && *r >= 0.0),
                cloud,
                region,
            }
        })
        .collect())
}

/// Unix seconds now.
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------------------
// SSH keys and the account (bodies with secrets/PII: typed, never held as JSON)
// ---------------------------------------------------------------------------------------

/// One account SSH key, as arena keeps it: no `public_key`, and above all no `private_key`
/// (the managed key's private half comes back in the list) — serde drops them, so they're
/// never in a `Value` that could be logged or cached.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SshKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Deserialize)]
struct SshKeys {
    ssh_keys: Vec<SshKey>,
}

/// The account's keys from a `GET /sshkeys` body. The error never quotes the body.
pub fn parse_ssh_keys(text: &str) -> Result<Vec<SshKey>> {
    serde_json::from_str::<SshKeys>(text)
        .map(|k| k.ssh_keys)
        .map_err(|_| Error::provider("shadeform sshkeys: unexpected response shape (not shown: it can hold a private key)"))
}

/// The id of the account key named `name` — exactly one must match; none or several is a
/// config error naming the key *names* only. Pure.
pub fn key_id_by_name(keys: &[SshKey], name: &str) -> Result<String> {
    let names = || keys.iter().map(|k| format!("`{}`", k.name)).collect::<Vec<_>>().join(", ");
    let hits: Vec<&SshKey> = keys.iter().filter(|k| k.name == name).collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(Error::Config(format!(
            "SHADEFORM_SSH_KEY: no SSH key named `{name}` on the Shadeform account (it has: {}) — upload the \
             cohort key's .pub there as `{name}` (console → SSH Keys, or POST /sshkeys/add), or set SHADEFORM_SSH_KEY \
             to its name. arena doesn't upload keys itself",
            if keys.is_empty() { "none".to_string() } else { names() }
        ))),
        _ => Err(Error::Config(format!(
            "SHADEFORM_SSH_KEY: {} SSH keys are named `{name}` on the Shadeform account — rename all but one",
            hits.len()
        ))),
    }
}

/// `GET /account` (undocumented): arena's view of it. Only these fields are read — the answer
/// also carries the account's email, phone, tax id and address, which serde drops here, so
/// the parsed value (and its `Debug`) holds none of them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountFields {
    #[serde(default)]
    balance: Option<Num>,
    #[serde(default)]
    hourly_burn: Option<Num>,
    #[serde(default)]
    pub instance_count: Option<u64>,
    #[serde(default)]
    pub volume_count: Option<u64>,
    #[serde(default)]
    pub tier: Option<String>,
}

/// A decimal-dollar string (`"0"`, `"12.34"`) or a number.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
enum Num {
    S(String),
    F(f64),
}

impl Num {
    fn value(&self) -> Option<f64> {
        match self {
            Num::S(s) => s.trim().parse::<f64>().ok(),
            Num::F(f) => Some(*f),
        }
        .filter(|v| v.is_finite())
    }
}

/// `(balance, hourly_burn)` in USD from a `GET /account` body, or why not (in our own words:
/// the body is never quoted — it holds personal data). Pure.
pub fn parse_account(text: &str) -> std::result::Result<(f64, f64), String> {
    let a: AccountFields = serde_json::from_str(text).map_err(|_| "GET /account isn't a JSON object of the recorded shape".to_string())?;
    match (a.balance.as_ref().and_then(Num::value), a.hourly_burn.as_ref().and_then(Num::value)) {
        (Some(b), Some(h)) => Ok((b, h)),
        _ => Err("no numeric `balance` / `hourly_burn` in GET /account (the answer isn't shown: it holds personal data)".into()),
    }
}

/// The account's own record, `GET {base}/account` with the key in `X-API-KEY` — for its
/// balance ([`crate::balance`]). Undocumented: any failure reads as "balance unknown".
pub(crate) fn account_request(client: &Client, base: &str, api_key: &str) -> RequestBuilder {
    client.get(format!("{}/account", base.trim_end_matches('/'))).header("X-API-KEY", api_key).header("Accept", "application/json")
}

// ---------------------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------------------

/// How an unsure create is settled: relist this many times, this far apart, for the nonce.
#[derive(Debug, Clone, Copy)]
struct Relist {
    tries: u32,
    every: Duration,
}

const RELIST: Relist = Relist { tries: 3, every: Duration::from_secs(10) };

/// The waits between the post-create price reads ([`ShadeformProvider::settle`]): ~30 s of
/// backoff before the listing is tried and, failing that, the instance deleted.
pub const PRICE_READS: [Duration; 5] =
    [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8), Duration::from_secs(15)];

/// What settling a created instance came to ([`ShadeformProvider::settle`]).
#[allow(clippy::large_enum_variant)] // a return value matched at once, never stored: boxing the pod buys nothing
enum Settled {
    /// Priced, within the cap and the quote: the pod.
    Pod(Pod),
    /// Its price couldn't be read: deleted (why) — the create tries the next entry/option.
    Unpriced(String),
}

/// The Shadeform backend. `Debug` is deliberately not derived (it holds the API key).
pub struct ShadeformProvider {
    api_key: String,
    client: Client,
    base: String,
    opts: ShadeformOpts,
    relist: Relist,
    /// Each create's nonce ([`new_nonce`]; a fixed one in tests, so a canned listing can hold it).
    make_nonce: fn() -> String,
    /// The waits between post-create price reads ([`PRICE_READS`]; none in tests).
    price_reads: Vec<Duration>,
}

impl ShadeformProvider {
    pub fn new(api_key: impl Into<String>, opts: ShadeformOpts) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
            opts,
            relist: RELIST,
            make_nonce: new_nonce,
            price_reads: PRICE_READS.to_vec(),
        }
    }

    /// `SHADEFORM_API_KEY` plus the validated options.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let opts = ShadeformOpts::from_config(cfg)?;
        Ok(Self::new(cfg.require("SHADEFORM_API_KEY")?, opts))
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    pub fn opts(&self) -> &ShadeformOpts {
        &self.opts
    }

    /// A request to `path` (under the base) with the key in its header — never the URL.
    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("X-API-KEY", &self.api_key)
            .header("Accept", "application/json")
    }

    async fn get_json(&self, path: &str, ctx: &str) -> Result<Value> {
        send_json(self.req(Method::GET, path), ctx).await
    }

    async fn post_ok(&self, path: &str, body: Option<&Value>, ctx: &str) -> Result<()> {
        let rb = self.req(Method::POST, path);
        let rb = match body {
            Some(b) => rb.json(b),
            None => rb,
        };
        crate::http::send_ok(rb, ctx).await
    }

    async fn info(&self, id: &str) -> Result<Value> {
        require_id(id)?;
        self.get_json(&format!("/instances/{id}/info"), "shadeform info").await
    }

    /// Every instance on the account, raw.
    async fn instances(&self) -> Result<Vec<Value>> {
        let body = self.get_json("/instances", "shadeform list").await?;
        Ok(instances_array(&body)?.clone())
    }

    /// The fresh `info` of `id`, refused unless it's ours ([`is_ours`]) — before any mutation.
    async fn ours(&self, id: &str, what: &str) -> Result<Value> {
        let v = self.info(id).await?;
        if is_ours(&v, &self.opts.prefix) {
            Ok(v)
        } else {
            Err(foreign_refusal(&v, &self.opts.prefix, what))
        }
    }

    /// The `ssh_key_id` to create with: the uploaded key `SHADEFORM_SSH_KEY` names (`None` for
    /// `default`: the account's default key). The `/sshkeys` body is read as text and typed at
    /// once ([`parse_ssh_keys`]) — never logged, never quoted, even in an error.
    async fn key_id(&self) -> Result<Option<String>> {
        if self.opts.ssh_key == DEFAULT_KEY {
            return Ok(None);
        }
        let (status, text) = crate::http::send_text(self.req(Method::GET, "/sshkeys")).await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &"(body not shown)", "shadeform sshkeys"));
        }
        key_id_by_name(&parse_ssh_keys(&text)?, &self.opts.ssh_key).map(Some)
    }

    /// The in-stock entries for `spec`, cheapest first. Read-only.
    async fn entries(&self, spec: &PodSpec, avoid: &[(String, String)]) -> Result<Vec<Entry>> {
        let want = shadeform_gpu_type(&spec.gpu_type)?;
        let body = self.get_json(&catalog_path(&want, spec.gpu_count), "shadeform catalog").await?;
        fitting_entries(instance_types(&body)?, spec, &self.opts, avoid)
    }

    /// Every volume on the account, priced (`GET /volumes`, then `GET /volumes/types` — a
    /// failed price read only leaves them unpriced). Read-only.
    pub async fn list_volumes(&self) -> Result<Vec<ShadeformVolume>> {
        let body = self.get_json("/volumes", "shadeform volumes").await?;
        let types = self.get_json("/volumes/types", "shadeform volume types").await.ok();
        parse_volumes(&body, types.as_ref())
    }

    /// Placement's live prices: per GPU option, the cheapest in-stock entry that fits `base`
    /// with that GPU (`Ok(None)` = none). **One** catalog read for every option (in stock,
    /// cheapest first), bounded by `limit`. Read-only; a failure is each option's `Err`.
    pub async fn quote(&self, gpus: &[String], base: &PodSpec, limit: Duration) -> Vec<(String, EntryQuote)> {
        let catalog = match crate::ratelimit::bounded(limit, self.get_json(AVAILABLE_PATH, "shadeform catalog")).await {
            Ok(Ok(body)) => instance_types(&body).cloned().map_err(|e| e.to_string()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
        };
        gpus.iter()
            .map(|gpu| {
                let spec = PodSpec { gpu_type: gpu.clone(), max_price: None, ..base.clone() };
                let q = match &catalog {
                    Ok(types) => fitting_entries(types, &spec, &self.opts, &[]).map(|e| e.into_iter().next()).map_err(|e| e.to_string()),
                    Err(e) => Err(e.clone()),
                };
                (gpu.clone(), q)
            })
            .collect()
    }

    /// One create POST on `entry` with `nonce`: the instance's id, or why it may exist.
    async fn create_once(&self, payload: &Value) -> Result<Created> {
        let rb = self.req(Method::POST, "/instances/create").timeout(CREATE_TIMEOUT).json(payload);
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                return match create_send_error(e) {
                    Ok(plain) => Err(plain),
                    Err(why) => Ok(Created::Unsure(why)),
                }
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        create_outcome(status, &text)
    }

    /// Settle an unsure create: list for the nonce's tag, [`Relist::tries`] times. The instance
    /// if found (adopted), else `None`.
    async fn find_created(&self, nonce: &str) -> Option<Value> {
        let tag = nonce_tag(nonce);
        for i in 0..self.relist.tries {
            if i > 0 {
                tokio::time::sleep(self.relist.every).await;
            }
            if let Ok(list) = self.instances().await {
                if let Some(v) = find_by_nonce(&list, &tag) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// The created (or adopted) instance as a pod, once its price passed the guard
    /// ([`price_guard`]): one priced above the cap or the quote is deleted at once and the
    /// create fails, saying both prices.
    ///
    /// The price must be **read** (review SF4: one failed `info` used to skip the guard and keep
    /// the instance, shown at the quote): `info` is read again with backoff ([`PRICE_READS`],
    /// ~30 s in all) while it fails or carries no readable price, then the listing is tried. A
    /// price still unknown is no price to accept — the instance is deleted and
    /// [`Settled::Unpriced`] says why, so the create moves on to the next entry/option (nothing
    /// is left behind to duplicate). Only if that delete fails is it an error ("may still be
    /// running"), which ends the run.
    async fn settle(&self, id: &str, known: Option<Value>, entry: &Entry, spec: &PodSpec) -> Result<Settled> {
        let mut v = known.filter(|v| price_of(v).is_some());
        let mut waits = self.price_reads.iter();
        while v.is_none() {
            if let Ok(read) = self.info(id).await {
                if price_of(&read).is_some() {
                    v = Some(read);
                    break;
                }
            }
            match waits.next() {
                Some(w) => tokio::time::sleep(*w).await,
                None => break,
            }
        }
        if v.is_none() {
            if let Ok(list) = self.instances().await {
                v = list.into_iter().find(|i| i.get("id").and_then(Value::as_str) == Some(id)).filter(|i| price_of(i).is_some());
            }
        }
        let delete_path = format!("/instances/{id}/delete");
        let delete = || self.post_ok(&delete_path, None, "shadeform delete");
        let Some(v) = v else {
            let why = format!(
                "the price of {} ({id}) couldn't be read after its create (info and the listing failed or carried none)",
                spec.name
            );
            return match delete().await {
                Ok(()) => Ok(Settled::Unpriced(format!("{why} — deleted it rather than keep an unchecked price; trying the next"))),
                Err(e) => Err(Error::provider(format!(
                    "{why}, and deleting it FAILED ({e}) — it may still be running: check `arena pods list --provider shadeform` \
                     for {} (and `arena pods terminate` it) before creating it again",
                    spec.name
                ))),
            };
        };
        let cap = self.opts.cap(spec.max_price);
        if let Some(why) = price_of(&v).and_then(|charged| price_guard(charged, entry.price(), cap)) {
            return Err(match delete().await {
                Ok(()) => Error::provider(format!("{why} — deleted {} ({id})", spec.name)),
                Err(e) => Error::provider(format!(
                    "{why} — deleting {} ({id}) FAILED ({e}): it may still be running — check `arena pods list --provider \
                     shadeform` and terminate it (`arena pods terminate {}`)",
                    spec.name, spec.name
                )),
            });
        }
        Ok(Settled::Pod(parse_instance(&v, now())))
    }

    /// Up-front refusals for a create: no request has been made when one of these fires.
    fn precheck(&self, spec: &PodSpec) -> Result<Vec<String>> {
        if let Some(extra) = &spec.api_extra {
            self.check_create_extra(extra)?;
        }
        if spec.volume_gb > 0 {
            return Err(Error::Config(format!(
                "VOLUME_GB={} isn't supported on Shadeform (volumes attach only at create, and the CUDA-13 cloud has \
                 none) — set VOLUME_GB=0; work survives through `arena pods backup` and the archive",
                spec.volume_gb
            )));
        }
        if let Some(bad) = spec.bad_cuda() {
            return Err(crate::pod::bad_cuda_error(bad));
        }
        if !spec.name.starts_with(&format!("{}-", self.opts.prefix)) {
            return Err(Error::Config(format!(
                "`{}` doesn't start with `{}-` (MACHINE_NAME_PREFIX) — arena only creates (and later touches) its own \
                 prefix's instances on Shadeform",
                spec.name, self.opts.prefix
            )));
        }
        let keys = spec_pubkeys(spec);
        if keys.is_empty() {
            return Err(Error::Config(
                "no SSH public key to authorize for root (PUBLIC_KEY is empty: check SHARED_SSH_KEY_PATH) — the VM \
                 would be unreachable"
                    .into(),
            ));
        }
        shadeform_gpu_type(&spec.gpu_type)?;
        Ok(keys)
    }
}

/// Why a Shadeform pod can't be stopped (or started): what `pods stop/start` say up front.
pub const NO_STOP: &str = "Shadeform has no stop — an instance bills until it's deleted, idle or not; back it up \
    (`arena pods backup <name>`), then `arena pods terminate <name>`";

#[async_trait]
impl Provider for ShadeformProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn describe(&self, spec: &PodSpec) -> String {
        let gpu = shadeform_gpu_type(&spec.gpu_type).unwrap_or_else(|_| format!("`{}` (no Shadeform type: refused)", spec.gpu_type));
        let cuda = match (spec.bad_cuda(), spec.cuda_floor()) {
            (Some(bad), _) => format!("CUDA `{bad}` — not a version: creates are refused"),
            (None, Some(v)) => format!("CUDA ≥ {v} image"),
            (None, None) => "any OS image (no CUDA floor)".to_string(),
        };
        let regions = if self.opts.regions.is_empty() { String::new() } else { format!(", regions {}", self.opts.regions.join(", ")) };
        let kind = if self.opts.allow_baremetal { "VM or bare metal" } else { "VM" };
        let key = if self.opts.ssh_key == DEFAULT_KEY { "the account's default key".to_string() } else { format!("key `{}`", self.opts.ssh_key) };
        let volume = if spec.volume_gb > 0 { "; VOLUME_GB > 0 is refused (no volumes)" } else { "" };
        format!(
            "{}×{gpu} {kind} via Shadeform (clouds: {}{regions}; on-demand; {cuda}; disk fixed by type; ≤ {}/h), cheapest \
             in stock at create; root over SSH via the bootstrap script ({key}); image, cloud tier, disk and ports aren't \
             used{volume}; no stop — it bills until terminated",
            spec.gpu_count,
            self.opts.clouds.label(),
            fmt_money("$", self.opts.cap(spec.max_price)),
        )
    }

    /// Every instance on the account — arena's and anyone else's (shown, so a shared or wrong
    /// account is visible; [`is_ours`] guards every mutation).
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        let t = now();
        Ok(self.instances().await?.iter().map(|v| parse_instance(v, t)).collect())
    }

    /// Resolve the key, refuse a name already live on the account, read the catalog, then
    /// create on the cheapest fitting entry: a capacity refusal moves to the next entry
    /// ([`CREATE_TRIES`] at most, the failed `(cloud, region)` avoided); an unsure answer is
    /// settled by listing for the nonce — adopted, or the run stops; anything else ends the
    /// create. A created instance goes through the price guard ([`Self::settle`]).
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        self.precheck(spec)?;
        let key_id = self.key_id().await?;
        // Names may not be unique on Shadeform (unverified): arena keeps them unique itself.
        if let Some(twin) = self.instances().await?.iter().find(|v| {
            v.get("name").and_then(Value::as_str) == Some(spec.name.as_str())
                && map_status(v.get("status").and_then(Value::as_str).unwrap_or("")) != "TERMINATED"
        }) {
            let id = twin.get("id").and_then(Value::as_str).unwrap_or("?");
            return Err(Error::provider(format!(
                "an instance named {} ({id}) is already on the Shadeform account — not creating a second one",
                spec.name
            )));
        }
        let mut avoid: Vec<(String, String)> = Vec::new();
        let entries = self.entries(spec, &avoid).await?;
        let mut last_capacity = None;
        let mut tries = 0;
        for entry in &entries {
            if avoid.contains(&(entry.cloud.clone(), entry.region.clone())) {
                continue;
            }
            if tries == CREATE_TRIES {
                break;
            }
            tries += 1;
            let nonce = (self.make_nonce)();
            let payload = create_payload(&self.opts, spec, entry, &nonce, key_id.as_deref())?;
            match self.create_once(&payload).await {
                Ok(Created::Id(id)) => match self.settle(&id, None, entry, spec).await? {
                    Settled::Pod(pod) => return Ok(pod),
                    Settled::Unpriced(why) => {
                        avoid.push((entry.cloud.clone(), entry.region.clone()));
                        last_capacity = Some(Error::capacity(why));
                    }
                },
                Ok(Created::Unsure(why)) => {
                    let found = self.find_created(&nonce).await;
                    let id = found.as_ref().and_then(|v| v.get("id")).and_then(Value::as_str).filter(|id| valid_id(id)).map(String::from);
                    let (Some(v), Some(id)) = (found, id) else {
                        return Err(create_unsure(entry, &spec.name, &why));
                    };
                    match self.settle(&id, Some(v), entry, spec).await? {
                        Settled::Pod(pod) => return Ok(pod),
                        Settled::Unpriced(why) => {
                            avoid.push((entry.cloud.clone(), entry.region.clone()));
                            last_capacity = Some(Error::capacity(why));
                        }
                    }
                }
                Err(e) if e.kind() == Some(ProviderErrorKind::Capacity) => {
                    avoid.push((entry.cloud.clone(), entry.region.clone()));
                    last_capacity = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_capacity.unwrap_or_else(|| {
            let cap = self.opts.cap(spec.max_price);
            let floor = spec.cuda_floor().map(|f| format!(", CUDA ≥ {f}")).unwrap_or_default();
            Error::capacity(format!(
                "shadeform: no in-stock {}×{} VM on {} (on-demand{floor}, ≤ {}/h) right now",
                spec.gpu_count,
                shadeform_gpu_type(&spec.gpu_type).unwrap_or_default(),
                self.opts.clouds.label(),
                fmt_money("$", cap)
            ))
        }))
    }

    async fn stop_pod(&self, _id: &str) -> Result<()> {
        Err(Error::NotImplemented(NO_STOP.into()))
    }

    async fn start_pod(&self, _id: &str) -> Result<()> {
        Err(Error::NotImplemented(NO_STOP.into()))
    }

    fn stop_support(&self, _pod: &Pod) -> Result<()> {
        Err(Error::NotImplemented(NO_STOP.into()))
    }

    /// `SHADEFORM_READY_MINS`: a new instance not active (with an endpoint) by then is a
    /// failed placement — deleted, the next option tried — never left billing.
    fn ready_deadline(&self, _pod: &Pod) -> Option<Duration> {
        Some(Duration::from_secs(self.opts.ready_mins * 60))
    }

    /// `POST /instances/{id}/restart` — of our own instance only. A 200 means "initiated"; the
    /// status stays `active`. Whether the disk survives on every cloud is unverified, so the
    /// trait's safe default (gated as a wipe) stands.
    async fn restart_pod(&self, id: &str) -> Result<()> {
        self.ours(id, "restarted").await?;
        self.post_ok(&format!("/instances/{id}/restart"), None, "shadeform restart").await
    }

    /// `POST /instances/{id}/delete` (no body) — of our own instance only: billing stops at
    /// `deleting`. A 404 (already gone) is still returned as the error it is.
    async fn terminate_pod(&self, id: &str) -> Result<()> {
        self.ours(id, "deleted").await?;
        self.post_ok(&format!("/instances/{id}/delete"), None, "shadeform delete").await
    }

    /// `POST /instances/{id}/update {"name": …}` — of our own instance only; its tags are
    /// left as they are (omitted = unchanged).
    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        // Review SF5: a name without `<prefix>-` would disown the instance — [`is_ours`] fails
        // from then on, so arena could no longer terminate, restart or rename it back while it
        // bills. Refused before any request, as create refuses it.
        require_id(id)?;
        let prefix = format!("{}-", self.opts.prefix);
        if !new_name.starts_with(&prefix) || new_name.len() == prefix.len() || !valid_name(new_name) {
            return Err(Error::Config(format!(
                "can't rename a Shadeform instance to `{new_name}`: arena only manages `{}-…` names there \
                 ([A-Za-z0-9._-], ≤ 63) — renamed outside the prefix, it couldn't be terminated by arena any more",
                self.opts.prefix
            )));
        }
        self.ours(id, "renamed").await?;
        self.post_ok(&format!("/instances/{id}/update"), Some(&json!({ "name": new_name })), "shadeform rename").await
    }

    fn check_create_extra(&self, extra: &Extra) -> Result<()> {
        apiextra::check(extra, MANAGED, None)
    }

    /// The body a create would send, the placement fields (picked at create from the live
    /// catalog) and the key id shown as placeholders — and the bootstrap script decoded beside
    /// it, so the dry run shows what runs on the VM.
    fn preview_create_body(&self, spec: &PodSpec) -> Result<Value> {
        let entry = Entry {
            cloud: "<cheapest in-stock cloud at create>".into(),
            region: "<its region>".into(),
            shade_instance_type: "<its type>".into(),
            gpu_type: shadeform_gpu_type(&spec.gpu_type).unwrap_or_default(),
            num_gpus: spec.gpu_count,
            os: "<the newest image at the CUDA floor>".into(),
            price_cents: 0,
            boot_max: None,
        };
        let nonce = new_nonce();
        let key = (self.opts.ssh_key != DEFAULT_KEY).then(|| format!("<id of `{}`>", self.opts.ssh_key));
        let mut body = create_payload(&self.opts, spec, &entry, &nonce, key.as_deref())?;
        let script = render_bootstrap(&spec_pubkeys(spec), &nonce, &spec.name)?;
        body["launch_configuration"]["script_configuration"]["base64_script"] = json!("<base64 of the script below>");
        body["(bootstrap script, decoded — not a field)"] = json!(script);
        Ok(body)
    }

    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        let v = self.info(id).await?;
        let pod = parse_instance(&v, now());
        let conf = v.get("configuration");
        Ok(PodSpec {
            name: String::new(),
            image: String::new(),
            gpu_type: pod.gpu_type.filter(|g| !g.ends_with("(shadeform)")).unwrap_or_default(),
            gpu_count: pod.gpu_count.unwrap_or(1),
            cloud_type: String::new(),
            disk_gb: conf.and_then(|c| c.get("storage_in_gb")).and_then(Value::as_u64).map_or(0, |g| g as u32),
            volume_gb: 0,
            ports: "22/tcp".into(),
            env: Vec::new(),
            docker_args: None,
            allowed_cuda: Vec::new(),
            min_cuda: None,
            max_price: pod.cost_per_hr,
            api_extra: None,
            avoid_machines: Vec::new(),
            server_type: None,
            vm_image: None,
            region: None,
        })
    }
}

/// Contract tests against recorded live responses (`fixtures/shadeform/`).
#[cfg(test)]
#[path = "shadeform_contract.rs"]
mod contract;

#[cfg(test)]
#[path = "shadeform_tests.rs"]
mod tests;
