//! `pods test --deep` (PLAN 1.A): is this pod actually usable for the course?
//!
//! A plain `import torch` passes on every failure we have actually seen on a bad host: a
//! `cuInit` 999 / `CUDA error: unknown error` GPU, a GPU-to-GPU copy that silently returns
//! garbage, a 535 driver under a CUDA 13 image, a 0.4 MB/s network, an oversubscribed
//! host. This module checks for those, split three ways so most of it is pure and
//! table-tested:
//!
//! 1. [`DEEP_CHECK_SCRIPT`] (`deep_check.sh`, bash + an inline python file) runs on the
//!    pod — delivered in one `Remote::exec` by [`deep_check_command`] — and prints facts
//!    as `key=value` lines. It only *measures*; every judgement lives here, so a policy
//!    change never needs a pod to test.
//! 2. [`parse_deep`] turns that text into [`DeepFacts`]. Tolerant: a missing key is
//!    "unknown", junk lines are ignored, and a cut-off run is visible (`complete`).
//! 3. [`evaluate`] applies a [`HealthPolicy`] (driver floor from config, network/disk/load
//!    thresholds) plus the provider-side maintenance window, giving [`Check`]s with a
//!    [`Status`]; the worst one is the pod's verdict.
//!
//! The rendering helpers at the bottom (table, per-check lines, same-host summary) are
//! here rather than in the CLI so the TUI and the `up --check` summary (PLAN 2.B / Phase
//! 3) show a pod's health the same way.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fleet::{clip, maintenance_label};
use crate::metrics::normalize_gpu_name;
use crate::pod::{Maintenance, Pod};
use crate::table::{self, Align};

/// The on-pod fact gatherer. Embedded (like `hetzner_setup.sh`) so the binary is all an
/// operator needs, and versioned with the parser that reads its output.
pub const DEEP_CHECK_SCRIPT: &str = include_str!("deep_check.sh");

/// The whole remote call's budget. The script bounds itself to ≈110s worst case (each
/// sub-step under `timeout`, the network probe in parallel with the GPU checks); on top
/// come the ssh connect (10s) and sourcing the participants' `~/.zshrc`. Past this the pod
/// is wedged and counts as a FAIL — the fleet run never waits on it longer.
pub const DEEP_CHECK_TIMEOUT: Duration = Duration::from_secs(150);

/// Below this Hugging Face download speed a pod WARNs: course notebooks pull models and
/// datasets, and at under 2 MB/s a 500 MB checkpoint takes minutes.
pub const DEFAULT_MIN_HF_MBPS: f64 = 2.0;

/// Below this much free space on `/` or `/workspace` a pod WARNs: a model download or a
/// checkpoint will fail part-way.
pub const DEFAULT_MIN_FREE_GB: f64 = 10.0;

/// Host load never WARNs below this, whatever the CPU count (see [`HealthPolicy::load_floor`]).
pub const DEFAULT_LOAD_FLOOR: f64 = 32.0;

/// The command that runs [`DEEP_CHECK_SCRIPT`] on a pod, *before* the login-shell wrap:
/// the script travels base64-encoded inside the command itself (one exec, no scp, no
/// temp file to clean up — and no quoting to get wrong, since base64 has no quotes or
/// `$`). `</dev/null`: nothing in it may wait on input. Exposed so a test can run it
/// locally against stub tools.
pub fn deep_check_inner_command() -> String {
    format!(
        "bash -c \"$(printf %s {} | base64 -d)\" arena-deep-check </dev/null",
        base64_encode(DEEP_CHECK_SCRIPT.as_bytes())
    )
}

/// The full remote command: [`deep_check_inner_command`] wrapped by
/// [`crate::ssh::login_shell_wrap`], so `python` is the participants' conda env
/// (`CONDA_ENV`, default `arena-env`) — the interpreter that has to work.
pub fn deep_check_command(conda_env: Option<&str>) -> String {
    crate::ssh::login_shell_wrap(&deep_check_inner_command(), conda_env)
}

/// Standard base64 (RFC 4648, padded) — just enough for [`deep_check_inner_command`];
/// not worth a dependency.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------------------

/// One GPU as `nvidia-smi` reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GpuFact {
    pub index: u32,
    pub name: Option<String>,
    pub driver: Option<String>,
    pub mem_total_mib: Option<u64>,
    pub mem_used_mib: Option<u64>,
}

/// What the check script measured on one pod. Every field is optional: `None` means the
/// script didn't say (or said `unknown`), and [`evaluate`] decides what an unknown means
/// for each check. Result strings (`torch`, `tensor`, `peer`, `nccl`, …) are the script's
/// own: `ok`, `skipped (…)`, `mismatch`, or `error: <Type>: <message>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeepFacts {
    /// Saw the `deep_check=` header — i.e. this output is the script's at all.
    pub started: bool,
    /// Saw the `deep_check_end=1` trailer — the script ran to the end.
    pub complete: bool,
    /// Host load averages (the host's, not the container's) and CPU counts.
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    /// Processors in `/proc/cpuinfo` (the host's CPUs, which is what `load1` is about).
    pub cpus: Option<u32>,
    /// `nproc`: CPUs this container may use.
    pub nproc: Option<u32>,
    pub uptime_secs: Option<u64>,
    /// `ok`, `missing`, or `error: …`.
    pub smi: Option<String>,
    /// The "CUDA Version" in nvidia-smi's header: the newest CUDA the driver supports.
    pub smi_cuda: Option<String>,
    /// Error from the `--query-gpu` call, when the header call worked but that didn't.
    pub smi_query: Option<String>,
    pub smi_gpus: Option<u32>,
    pub gpus: Vec<GpuFact>,
    /// The python found on PATH (in the conda env), or `missing`.
    pub python: Option<String>,
    /// The python step's exit status (124/137 = killed by its `timeout`).
    pub py_exit: Option<i32>,
    pub py_stderr: Option<String>,
    /// The python step printed its last line.
    pub py_done: bool,
    pub torch: Option<String>,
    pub torch_version: Option<String>,
    /// The CUDA torch was built against (`torch.version.cuda`).
    pub torch_cuda: Option<String>,
    pub cuda_available: Option<bool>,
    /// Why CUDA is unavailable (what `torch.cuda.init()` raised).
    pub cuda_error: Option<String>,
    pub device_count: Option<u32>,
    pub device_count_error: Option<String>,
    /// Per torch device index: the small tensor op's result.
    pub tensor: BTreeMap<u32, String>,
    /// Per `"i-j"` pair: GPU i → GPU j copy result.
    pub peer: BTreeMap<String, String>,
    /// `skipped (1 GPU)` when there was nothing to copy between.
    pub peer_skipped: Option<String>,
    pub nccl: Option<String>,
    pub nccl_ranks: Option<u32>,
    /// curl's exit status for the Hugging Face probe, or `missing` (no curl).
    pub net_curl_exit: Option<String>,
    pub net_http: Option<u16>,
    pub net_bytes: Option<u64>,
    pub net_secs: Option<f64>,
    pub net_error: Option<String>,
    pub disk_root_avail_kb: Option<u64>,
    pub disk_workspace_avail_kb: Option<u64>,
    /// Any other `key=value` the script printed (a newer script, an `error=`): kept for
    /// `--json` rather than dropped.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, String>,
}

impl DeepFacts {
    /// Hugging Face download speed in MB/s (10⁶ bytes), when something was downloaded.
    /// Computed from what arrived in the time it took, so a probe cut off by curl's
    /// `--max-time` still reads as the (slow) speed it was.
    pub fn hf_mbps(&self) -> Option<f64> {
        match (self.net_bytes, self.net_secs) {
            (Some(b), Some(s)) if b > 0 && s > 0.0 => Some(b as f64 / s / 1e6),
            _ => None,
        }
    }

    /// The lowest driver version any GPU reports (they share one driver; the lowest is
    /// the safe one to judge).
    pub fn driver(&self) -> Option<&str> {
        self.gpus
            .iter()
            .filter_map(|g| g.driver.as_deref().and_then(|d| parse_version(d).map(|v| (v, d))))
            .min_by(|a, b| cmp_versions(&a.0, &b.0))
            .map(|(_, d)| d)
    }

    /// GPUs as the machine sees them — `2×RTX A4000`, `1×A100 80GB PCIe+1×H100` for a
    /// mixed box — else torch's count (`2×GPU`), else `-`.
    pub fn gpu_label(&self) -> String {
        let mut groups: Vec<(String, usize)> = Vec::new();
        for g in &self.gpus {
            let name = g.name.as_deref().map(normalize_gpu_name).unwrap_or_else(|| "GPU".into());
            match groups.iter_mut().find(|(n, _)| *n == name) {
                Some((_, c)) => *c += 1,
                None => groups.push((name, 1)),
            }
        }
        if !groups.is_empty() {
            return groups.iter().map(|(n, c)| format!("{c}×{n}")).collect::<Vec<_>>().join("+");
        }
        match self.device_count {
            Some(n) if n > 0 => format!("{n}×GPU"),
            _ => "-".into(),
        }
    }

    /// How many GPUs nvidia-smi listed — only when it actually listed them. The header
    /// call working isn't enough: if `--query-gpu` then failed (`smi_query`, the classic
    /// GPU-fell-off-the-bus error) the count is unknown, not 0, and must not read as
    /// "nvidia-smi sees 0 GPUs" next to torch's count.
    fn smi_count(&self) -> Option<u32> {
        if self.smi.as_deref() != Some("ok") || self.smi_query.is_some() {
            return None;
        }
        self.smi_gpus.or_else(|| (!self.gpus.is_empty()).then_some(self.gpus.len() as u32))
    }

    /// Why python facts may be missing: the step timed out / crashed / was cut off.
    fn python_cut(&self) -> String {
        match self.py_exit {
            Some(124) | Some(137) => "python checks timed out".into(),
            Some(c) if c != 0 => match self.py_stderr.as_deref().filter(|s| !s.is_empty()) {
                Some(e) => format!("python exited {c}: {e}"),
                None => format!("python exited {c}"),
            },
            _ if !self.py_done => "python output cut off".into(),
            _ => "not reported".into(),
        }
    }
}

/// Parse the check script's stdout. Only lines after the `deep_check=` header count (shell
/// rc files can print anything before it), and parsing stops at `deep_check_end`. A line
/// that isn't `key=value` with a plain key is ignored; a value of `unknown` (or one that
/// doesn't parse as the field's type) leaves the field unknown. Later duplicates win.
pub fn parse_deep(stdout: &str) -> DeepFacts {
    let mut f = DeepFacts::default();
    let mut gpus: BTreeMap<u32, GpuFact> = BTreeMap::new();
    for line in stdout.lines() {
        let Some((key, value)) = line.trim_end_matches('\r').split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
            continue;
        }
        if !f.started {
            f.started = key == "deep_check";
            continue;
        }
        if key == "deep_check_end" {
            f.complete = true;
            break;
        }
        let text = || (!value.is_empty() && value != "unknown").then(|| value.to_string());
        let num = || value.parse::<f64>().ok().filter(|v| v.is_finite());
        let int = || value.parse::<u64>().ok();
        let small = || value.parse::<u32>().ok();
        match key {
            "load1" => f.load1 = num(),
            "load5" => f.load5 = num(),
            "load15" => f.load15 = num(),
            "cpus" => f.cpus = small().filter(|&n| n > 0),
            "nproc" => f.nproc = small().filter(|&n| n > 0),
            "uptime_secs" => f.uptime_secs = int(),
            "smi" => f.smi = text(),
            "smi_cuda" => f.smi_cuda = text(),
            "smi_query" => f.smi_query = text(),
            "smi_gpus" => f.smi_gpus = small(),
            "python" => f.python = text(),
            "py_exit" => f.py_exit = value.parse().ok(),
            "py_stderr" => f.py_stderr = text(),
            "py_done" => f.py_done = value == "1",
            "torch" => f.torch = text(),
            "torch_version" => f.torch_version = text(),
            "torch_cuda" => f.torch_cuda = text(),
            "cuda_available" => {
                f.cuda_available = match value {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            "cuda_error" => f.cuda_error = text(),
            "device_count" => f.device_count = small(),
            "device_count_error" => f.device_count_error = text(),
            "peer" => f.peer_skipped = text(),
            "nccl" => f.nccl = text(),
            "nccl_ranks" => f.nccl_ranks = small(),
            "net.curl_exit" => f.net_curl_exit = text(),
            "net.http" => f.net_http = value.parse().ok(),
            "net.bytes" => f.net_bytes = int(),
            "net.secs" => f.net_secs = num(),
            "net.error" => f.net_error = text(),
            "disk.root_avail_kb" => f.disk_root_avail_kb = int(),
            "disk.workspace_avail_kb" => f.disk_workspace_avail_kb = int(),
            _ => {
                if let Some(v) = text() {
                    if !indexed_fact(&mut f, &mut gpus, key, v.clone()) {
                        f.other.insert(key.to_string(), v);
                    }
                }
            }
        }
    }
    f.gpus = gpus.into_values().collect();
    f
}

/// `gpu.<i>.<field>`, `tensor.<i>`, `peer.<i>-<j>`: the per-device facts. `false` if `key`
/// isn't one of them.
fn indexed_fact(f: &mut DeepFacts, gpus: &mut BTreeMap<u32, GpuFact>, key: &str, value: String) -> bool {
    if let Some(rest) = key.strip_prefix("gpu.") {
        let Some((idx, field)) = rest.split_once('.') else { return false };
        let Ok(index) = idx.parse::<u32>() else { return false };
        if !matches!(field, "name" | "driver" | "mem_total_mib" | "mem_used_mib") {
            return false;
        }
        let g = gpus.entry(index).or_insert_with(|| GpuFact { index, ..Default::default() });
        match field {
            "name" => g.name = Some(value),
            "driver" => g.driver = Some(value),
            "mem_total_mib" => g.mem_total_mib = value.parse().ok(),
            _ => g.mem_used_mib = value.parse().ok(),
        }
        return true;
    }
    if let Some(idx) = key.strip_prefix("tensor.") {
        if let Ok(i) = idx.parse::<u32>() {
            f.tensor.insert(i, value);
            return true;
        }
        return false;
    }
    if let Some(pair) = key.strip_prefix("peer.") {
        let valid = pair.split_once('-').is_some_and(|(a, b)| a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok());
        if valid {
            f.peer.insert(pair.to_string(), value);
        }
        return valid;
    }
    false
}

// ---------------------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------------------

/// The minimum NVIDIA driver a pod must run, and why (shown in the check's detail).
#[derive(Debug, Clone, PartialEq)]
pub struct DriverFloor {
    /// Dotted version, e.g. `[580]` or `[580, 65, 6]`; compared component-wise.
    pub version: Vec<u32>,
    pub why: String,
}

/// What [`evaluate`] judges against. Fleet-wide (built once from config); the per-pod
/// inputs are the facts and the maintenance window.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthPolicy {
    /// `None` = no driver check (nothing configured to derive one from).
    pub min_driver: Option<DriverFloor>,
    pub min_hf_mbps: f64,
    pub min_free_gb: f64,
    /// Host load WARNs when `load1` exceeds `max(load_floor, host CPUs)`. Load average is
    /// the *host's* run queue (containers share the kernel), so it is judged against the
    /// host's CPU count: above one runnable task per CPU the machine is oversubscribed and
    /// everything on it slows down. The floor keeps small boxes quiet and matches the ops
    /// playbook's experience — single digits normal, ~40 oversubscribed.
    pub load_floor: f64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            min_driver: None,
            min_hf_mbps: DEFAULT_MIN_HF_MBPS,
            min_free_gb: DEFAULT_MIN_FREE_GB,
            load_floor: DEFAULT_LOAD_FLOOR,
        }
    }
}

impl HealthPolicy {
    /// The policy from config: `MIN_DRIVER_VERSION` (e.g. `580` or `580.65.06`; `none`
    /// turns the check off) wins; otherwise the floor is derived from the host CUDA
    /// versions pods are created on (`ALLOWED_CUDA_VERSIONS`, else
    /// `RUNPOD_ALLOWED_CUDA_VERSIONS` — the same keys `PodSpec` reads), see
    /// [`driver_floor_for_cuda`]. A malformed `MIN_DRIVER_VERSION` is an error, not a
    /// silently skipped check.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let mut policy = Self::default();
        if let Some(raw) = cfg.get("MIN_DRIVER_VERSION").map(str::trim).filter(|s| !s.is_empty()) {
            if raw.eq_ignore_ascii_case("none") {
                return Ok(policy);
            }
            let version = parse_version(raw).ok_or_else(|| {
                Error::Config(format!("MIN_DRIVER_VERSION={raw:?} is not a driver version like 580 or 580.65.06"))
            })?;
            policy.min_driver = Some(DriverFloor { version, why: "MIN_DRIVER_VERSION".into() });
            return Ok(policy);
        }
        let allowed = ["ALLOWED_CUDA_VERSIONS", "RUNPOD_ALLOWED_CUDA_VERSIONS"]
            .iter()
            .find_map(|k| cfg.get(k).filter(|v| !v.trim().is_empty()))
            .unwrap_or("");
        let versions: Vec<&str> = allowed.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        policy.min_driver = driver_floor_for_cuda(&versions);
        Ok(policy)
    }
}

/// The driver each host CUDA version guarantees (NVIDIA's CUDA toolkit release notes,
/// Linux, by driver branch). CUDA 13.x is handled separately: every 13.x runs on the 580
/// branch (minor-version compatibility), which is what a cu130 image needs.
const CUDA_DRIVER_FLOORS: &[(&str, u32)] = &[
    ("12.9", 575),
    ("12.8", 570),
    ("12.6", 560),
    ("12.5", 555),
    ("12.4", 550),
    ("12.3", 545),
    ("12.2", 535),
    ("12.1", 530),
    ("12.0", 525),
    ("11.8", 520),
];

/// The driver floor implied by the allowed host CUDA versions: each maps to its driver
/// branch (any 13.x → 580, 12.8 → 570, 12.4 → 550, …) and the **lowest** wins — the
/// provider may legitimately place a pod on a host with any listed version, so demanding
/// more would fail pods the config deliberately allows (the Vast search filters on the
/// lowest listed version for the same reason). Versions not in the table are ignored;
/// none known → `None` (no driver check).
pub fn driver_floor_for_cuda(versions: &[&str]) -> Option<DriverFloor> {
    let floor = |v: &str| -> Option<u32> {
        let mut parts = v.split('.');
        let major: u32 = parts.next()?.trim().parse().ok()?;
        if major == 13 {
            return Some(580);
        }
        let minor: u32 = parts.next()?.trim().parse().ok()?;
        let mm = format!("{major}.{minor}");
        CUDA_DRIVER_FLOORS.iter().find(|(c, _)| *c == mm).map(|(_, d)| *d)
    };
    versions
        .iter()
        .filter_map(|v| floor(v).map(|d| (d, *v)))
        .min_by_key(|(d, _)| *d)
        .map(|(d, v)| DriverFloor { version: vec![d], why: format!("CUDA {v}") })
}

/// `"580.65.06"` → `[580, 65, 6]`; `None` unless every dot-separated part is a number.
pub fn parse_version(s: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = s.trim().split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|v| !v.is_empty())
}

/// Compare dotted versions component-wise, a missing component counting as 0 (so `580`
/// == `580.0` and `580` < `580.65.06`).
fn cmp_versions(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| a.get(i).copied().unwrap_or(0).cmp(&b.get(i).copied().unwrap_or(0)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

// ---------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------

/// One check's verdict. Overall: any `Fail` fails the pod, else any `Warn` warns, else it
/// passes (`Skip` = not applicable / not configured, never a verdict of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Status::Pass => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
            Status::Skip => "-",
        }
    }
}

/// One named check of one pod: `name` is stable (`cuda`, `gpu1`, `peer_copy`, `network`,
/// …) so scripts can key on it; `detail` is the human reason or measurement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status, detail: detail.into() }
    }
}

/// The pod's verdict from its checks (see [`Status`]).
pub fn overall(checks: &[Check]) -> Status {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Pass
    }
}

/// Whether a pod should have GPUs: every pod but Hetzner's (CPU VMs, whose GPU checks are
/// skipped instead of failing every time). Decided by provider only, never by the API's
/// GPU count: a RunPod pod reported with 0 GPUs (resumed without one, or one the API lost
/// track of) is a broken course pod, and skipping its GPU checks would PASS it.
pub fn expects_gpu(pod: &Pod) -> bool {
    !pod.provider.eq_ignore_ascii_case("hetzner")
}

/// Judge a GPU pod's facts. See [`evaluate_cpu`] for CPU VMs.
pub fn evaluate(facts: &DeepFacts, policy: &HealthPolicy, maintenance: Option<&Maintenance>) -> Vec<Check> {
    evaluate_as(facts, policy, maintenance, true)
}

/// Judge a CPU VM's facts: the same checks minus everything GPU (one `gpu` Skip line).
pub fn evaluate_cpu(facts: &DeepFacts, policy: &HealthPolicy, maintenance: Option<&Maintenance>) -> Vec<Check> {
    evaluate_as(facts, policy, maintenance, false)
}

/// Advice torch appends to its CUDA errors. It pushes the actual error (`Error 999:
/// unknown error`, `found version 12020`) out of a table cell, so check details drop it;
/// the facts (and `--json`) keep the raw text. `true` = cut from here to the end.
const TORCH_BOILERPLATE: &[(&str, bool)] = &[
    ("Did you run some cuda functions before calling NumCudaDevices() that might have already set an error?", false),
    ("CUDA kernel errors might be asynchronously reported", true),
    ("For debugging consider passing CUDA_LAUNCH_BLOCKING=1", true),
    ("Compile with `TORCH_USE_CUDA_DSA`", true),
    ("Please update your GPU driver", true),
    ("Please check that you have an NVIDIA GPU", true),
];

/// A script result as a check detail: `error: RuntimeError: …` → `RuntimeError: …` (the
/// check line already says it failed), minus torch's boilerplate advice.
fn reason(v: &str) -> String {
    let mut s = v.strip_prefix("error:").map(str::trim).unwrap_or(v).to_string();
    for (phrase, to_end) in TORCH_BOILERPLATE {
        if let Some(at) = s.find(phrase) {
            if *to_end {
                s.truncate(at);
            } else {
                s.replace_range(at..at + phrase.len(), "");
            }
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn evaluate_as(f: &DeepFacts, p: &HealthPolicy, maintenance: Option<&Maintenance>, gpu: bool) -> Vec<Check> {
    use Status::*;
    let mut out = Vec::new();
    if !f.started {
        out.push(Check::new("script", Fail, "no output from the check script"));
        return out;
    }
    if !f.complete {
        out.push(Check::new("script", Warn, "output cut off before the end; later facts unknown"));
    }

    if gpu {
        out.push(smi_check(f));
        out.push(driver_check(f, p));
    } else {
        out.push(Check::new("gpu", Skip, "CPU pod: GPU checks skipped"));
    }

    let torch_ok = f.torch.as_deref() == Some("ok");
    out.push(match (f.python.as_deref(), f.torch.as_deref()) {
        (Some("missing"), _) => Check::new("torch", Fail, "no python on PATH (is CONDA_ENV's env there?)"),
        (_, Some("ok")) => Check::new(
            "torch",
            Pass,
            format!(
                "{} (CUDA {})",
                f.torch_version.as_deref().unwrap_or("?"),
                f.torch_cuda.as_deref().unwrap_or("?")
            ),
        ),
        (_, Some(e)) => Check::new("torch", Fail, reason(e)),
        (_, None) => Check::new("torch", Fail, f.python_cut()),
    });

    if gpu {
        let cuda_ok = torch_ok && f.cuda_available == Some(true);
        out.push(match f.cuda_available {
            _ if !torch_ok => Check::new("cuda", Skip, "needs torch"),
            Some(true) => Check::new("cuda", Pass, "available"),
            Some(false) => Check::new(
                "cuda",
                Fail,
                f.cuda_error.as_deref().map(reason).unwrap_or_else(|| "torch.cuda.is_available() is False".into()),
            ),
            None => Check::new("cuda", Fail, f.python_cut()),
        });
        if cuda_ok {
            gpu_checks(f, &mut out);
        } else {
            for name in ["device_count", "peer_copy", "nccl"] {
                out.push(Check::new(name, Skip, "needs CUDA"));
            }
        }
    }

    out.push(network_check(f, p));
    out.push(disk_check(f, p));
    out.push(load_check(f, p));
    out.push(match maintenance_label(maintenance).as_str() {
        "-" => Check::new("maintenance", Pass, "none reported"),
        window => Check::new("maintenance", Warn, window),
    });
    out
}

fn smi_check(f: &DeepFacts) -> Check {
    use Status::*;
    match f.smi.as_deref() {
        Some("ok") if f.gpus.is_empty() => match f.smi_query.as_deref() {
            Some(e) => Check::new("nvidia-smi", Fail, reason(e)),
            None => Check::new("nvidia-smi", Fail, "lists no GPUs"),
        },
        Some("ok") => Check::new("nvidia-smi", Pass, f.gpu_label()),
        Some("missing") => Check::new("nvidia-smi", Fail, "not found (no NVIDIA driver in the container)"),
        Some(e) => Check::new("nvidia-smi", Fail, reason(e)),
        None => Check::new("nvidia-smi", Fail, "not reported"),
    }
}

fn driver_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let Some(driver) = f.driver() else {
        return match f.smi.as_deref() {
            Some("ok") => Check::new("driver", Warn, "version not reported"),
            _ => Check::new("driver", Skip, "needs nvidia-smi"),
        };
    };
    let Some(floor) = &p.min_driver else {
        return Check::new("driver", Skip, format!("{driver} (no minimum configured)"));
    };
    let want = floor.version.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
    let have = parse_version(driver).unwrap_or_default();
    if cmp_versions(&have, &floor.version).is_lt() {
        Check::new("driver", Fail, format!("{driver} < {want} ({} needs ≥ {want})", floor.why))
    } else {
        Check::new("driver", Pass, format!("{driver} ≥ {want} ({})", floor.why))
    }
}

/// Device count, per-GPU tensor op, peer copies and NCCL — only once CUDA is available.
fn gpu_checks(f: &DeepFacts, out: &mut Vec<Check>) {
    use Status::*;
    let n = f.device_count;
    out.push(match (n, f.smi_count()) {
        (None, _) => Check::new(
            "device_count",
            Fail,
            f.device_count_error.as_deref().map(reason).unwrap_or_else(|| f.python_cut()),
        ),
        (Some(0), _) => Check::new("device_count", Fail, "torch sees no GPU"),
        (Some(t), Some(s)) if t != s => {
            Check::new("device_count", Fail, format!("torch sees {t} GPU(s), nvidia-smi {s}"))
        }
        (Some(t), Some(_)) => Check::new("device_count", Pass, format!("{t} (matches nvidia-smi)")),
        (Some(t), None) => Check::new("device_count", Pass, format!("{t} (nvidia-smi count unknown)")),
    });

    // One line per GPU torch can see (plus any result reported beyond that count).
    let mut indices: std::collections::BTreeSet<u32> = (0..n.unwrap_or(0)).collect();
    indices.extend(f.tensor.keys());
    for i in indices {
        out.push(match f.tensor.get(&i).map(String::as_str) {
            Some("ok") => Check::new(format!("gpu{i}"), Pass, "tensor op ok"),
            Some(e) => Check::new(format!("gpu{i}"), Fail, reason(e)),
            None => Check::new(format!("gpu{i}"), Fail, format!("no result ({})", f.python_cut())),
        });
    }

    let single = n.is_some_and(|n| n < 2);
    out.push(if let Some(s) = &f.peer_skipped {
        Check::new("peer_copy", Skip, s.clone())
    } else if single {
        Check::new("peer_copy", Skip, "skipped (1 GPU)")
    } else if f.peer.is_empty() {
        Check::new("peer_copy", Fail, format!("not reported ({})", f.python_cut()))
    } else {
        let arrow = |pair: &str| pair.replacen('-', "→", 1);
        let bad: Vec<String> =
            f.peer.iter().filter(|(_, r)| r.as_str() != "ok").map(|(k, r)| format!("{} {}", arrow(k), reason(r))).collect();
        // The script copies around a ring: one pair per GPU.
        let expected = n.unwrap_or(0) as usize;
        if !bad.is_empty() {
            Check::new("peer_copy", Fail, bad.join("; "))
        } else if f.peer.len() < expected {
            Check::new(
                "peer_copy",
                Fail,
                format!("only {} of {expected} copies reported ({})", f.peer.len(), f.python_cut()),
            )
        } else {
            let pairs: Vec<String> = f.peer.keys().map(|k| arrow(k)).collect();
            Check::new("peer_copy", Pass, format!("ok ({})", pairs.join(", ")))
        }
    });

    out.push(match f.nccl.as_deref() {
        Some(s) if s.starts_with("skipped") => Check::new("nccl", Skip, s),
        Some("ok") => Check::new(
            "nccl",
            Pass,
            match f.nccl_ranks {
                Some(r) => format!("all_reduce ok ({r} ranks)"),
                None => "all_reduce ok".into(),
            },
        ),
        Some(e) => Check::new("nccl", Fail, reason(e)),
        None if single => Check::new("nccl", Skip, "skipped (1 GPU)"),
        None => Check::new("nccl", Fail, format!("not reported ({})", f.python_cut())),
    });
}

fn network_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    const FROM: &str = "huggingface.co";
    if f.net_curl_exit.as_deref() == Some("missing") {
        return Check::new("network", Warn, "curl not found: download speed not measured");
    }
    if f.net_curl_exit.is_none() && f.net_bytes.is_none() {
        return Check::new("network", Warn, "not measured");
    }
    let http_ok = f.net_http.is_some_and(|h| (200..300).contains(&h));
    match (http_ok, f.hf_mbps()) {
        (true, Some(mbps)) if mbps < p.min_hf_mbps => {
            Check::new("network", Warn, format!("{mbps:.1} MB/s from {FROM} (< {} MB/s)", p.min_hf_mbps))
        }
        (true, Some(mbps)) => Check::new("network", Pass, format!("{mbps:.1} MB/s from {FROM}")),
        _ => match f.net_http.filter(|&h| h != 0 && !(200..300).contains(&h)) {
            Some(h) => Check::new("network", Warn, format!("{FROM} answered HTTP {h}")),
            None => {
                let why = f
                    .net_error
                    .clone()
                    .or_else(|| f.net_curl_exit.as_ref().map(|c| format!("curl exit {c}")))
                    .unwrap_or_else(|| "nothing downloaded".into());
                Check::new("network", Warn, format!("{FROM} unreachable: {why}"))
            }
        },
    }
}

/// `107 GB`, `3.2 GB` — whole numbers once there's plenty.
fn fmt_gb(gb: f64) -> String {
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else {
        format!("{gb:.1} GB")
    }
}

fn disk_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let gb = |kb: u64| kb as f64 * 1024.0 / 1e9;
    let Some(root) = f.disk_root_avail_kb else {
        return Check::new("disk", Warn, "free space on / unknown");
    };
    let mut mounts = vec![("/", gb(root))];
    if let Some(ws) = f.disk_workspace_avail_kb {
        mounts.push(("/workspace", gb(ws)));
    }
    let low: Vec<String> = mounts
        .iter()
        .filter(|(_, g)| *g < p.min_free_gb)
        .map(|(m, g)| format!("{m} {} free", fmt_gb(*g)))
        .collect();
    if low.is_empty() {
        let all: Vec<String> = mounts.iter().map(|(m, g)| format!("{m} {}", fmt_gb(*g))).collect();
        Check::new("disk", Pass, format!("{} free", all.join(", ")))
    } else {
        Check::new("disk", Warn, format!("{} (< {} GB)", low.join(", "), p.min_free_gb))
    }
}

/// `14d`, `5h`, `12m`.
fn fmt_uptime(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

fn load_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let Some(load) = f.load1 else {
        return Check::new("load", Skip, "unknown");
    };
    let cpus = f.cpus.or(f.nproc);
    let threshold = p.load_floor.max(f64::from(cpus.unwrap_or(0)));
    let on = cpus.map(|c| format!(" on {c} CPUs")).unwrap_or_default();
    let up = f.uptime_secs.map(|s| format!(", host up {}", fmt_uptime(s))).unwrap_or_default();
    if load > threshold {
        Check::new(
            "load",
            Warn,
            format!("host load {load:.1}{on} (> {threshold:.0}): oversubscribed, expect slow/flaky work{up}"),
        )
    } else {
        Check::new("load", Pass, format!("load {load:.1}{on}{up}"))
    }
}

// ---------------------------------------------------------------------------------------
// Per-pod result + rendering
// ---------------------------------------------------------------------------------------

/// One pod's deep-check outcome — also the `pods test --deep --json` element.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PodHealth {
    /// The provider's pod id: tells apart two pods that share a name.
    pub id: String,
    pub name: String,
    pub provider: String,
    pub status: Status,
    pub checks: Vec<Check>,
    /// `None` when the pod couldn't be checked at all (unreachable, timed out).
    pub facts: Option<DeepFacts>,
    /// The pod's machine IP, for the same-host summary only (see [`host_ip`]). Not
    /// serialized: the JSON is per-pod health, and IPs are already in `pods list --json`.
    #[serde(skip)]
    pub host: Option<String>,
}

/// The IP that identifies the machine a pod runs on, for the same-host summary: its SSH
/// IP, but only where that is the machine's own address. Vast pods are reached through
/// Vast's shared SSH proxy (`ssh4.vast.ai`), which says nothing about the machine, so
/// they — and any endpoint that isn't an IP literal — are never grouped: a false "same
/// host?" would send the operator to switch GPU type for nothing.
pub fn host_ip(pod: &Pod) -> Option<String> {
    if pod.provider.eq_ignore_ascii_case("vast") {
        return None;
    }
    pod.ssh_ip.as_deref().filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()).map(String::from)
}

impl PodHealth {
    /// A pod whose script ran: its facts judged by `policy` (GPU or CPU rules per
    /// [`expects_gpu`]), with the pod's maintenance window.
    pub fn checked(pod: &Pod, facts: DeepFacts, policy: &HealthPolicy) -> Self {
        let mut checks = if expects_gpu(pod) {
            evaluate(&facts, policy, pod.maintenance.as_ref())
        } else {
            evaluate_cpu(&facts, policy, pod.maintenance.as_ref())
        };
        // The GPU checks above judge what the machine sees; this says why they may have
        // failed (or that the API's record of a working pod is off).
        if expects_gpu(pod) && pod.gpu_count == Some(0) {
            checks.push(Check::new("provider", Status::Warn, "reports 0 GPUs for this pod (resumed without one?)"));
        }
        Self {
            id: pod.id.clone(),
            name: pod.name.clone(),
            provider: pod.provider.clone(),
            status: overall(&checks),
            checks,
            facts: Some(facts),
            host: host_ip(pod),
        }
    }

    /// A pod that couldn't be checked (no SSH, timed out, no endpoint): a FAIL with why.
    pub fn unreachable(pod: &Pod, why: impl Into<String>) -> Self {
        Self {
            id: pod.id.clone(),
            name: pod.name.clone(),
            provider: pod.provider.clone(),
            status: Status::Fail,
            checks: vec![Check::new("ssh", Status::Fail, why)],
            facts: None,
            host: host_ip(pod),
        }
    }

    /// The NOTES column: failures first, then warnings, as `check: detail`.
    pub fn notes(&self) -> String {
        let pick = |s: Status| self.checks.iter().filter(move |c| c.status == s);
        let notes: Vec<String> =
            pick(Status::Fail).chain(pick(Status::Warn)).map(|c| format!("{}: {}", c.name, c.detail)).collect();
        clip(&notes.join("; "), 110)
    }
}

/// `NAME  RESULT  GPUS  DRIVER  CUDA  NET  NOTES`, one row per pod in the given order.
pub fn render_health_table(results: &[PodHealth]) -> String {
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|h| {
            let f = h.facts.as_ref();
            let or_dash = |s: Option<String>| s.unwrap_or_else(|| "-".into());
            vec![
                h.name.clone(),
                h.status.label().to_string(),
                f.map(DeepFacts::gpu_label).unwrap_or_else(|| "-".into()),
                or_dash(f.and_then(|f| f.driver().map(String::from))),
                or_dash(f.and_then(|f| f.smi_cuda.clone())),
                or_dash(f.and_then(DeepFacts::hf_mbps).map(|m| format!("{m:.1} MB/s"))),
                h.notes(),
            ]
        })
        .collect();
    table::render(
        &["NAME", "RESULT", "GPUS", "DRIVER", "CUDA", "NET", "NOTES"],
        &[Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Left],
        &rows,
    )
}

/// `-v`: every check of one pod, `✓ name  detail` (`!` warn, `✗` fail, `-` skip).
pub fn render_checks(h: &PodHealth) -> String {
    let mut out = format!("── {} ({})\n", h.name, h.status.label());
    let width = h.checks.iter().map(|c| c.name.chars().count()).max().unwrap_or(0);
    for c in &h.checks {
        out.push_str(&format!("  {} {:<width$}  {}\n", c.status.symbol(), c.name, c.detail));
    }
    out
}

/// Failing pods grouped by machine IP ([`PodHealth::host`]), where at least two share one
/// — the ops playbook's "bad hosts break every pod on them": several pods failing on one
/// IP is most likely the machine, and replacing them on the same GPU type can land them
/// right back on it. Sorted by IP; names keep the input order.
pub fn same_host_failures(results: &[PodHealth]) -> Vec<(String, Vec<String>)> {
    let mut by_ip: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for h in results.iter().filter(|h| h.status == Status::Fail) {
        if let Some(ip) = h.host.as_deref().filter(|ip| !ip.is_empty()) {
            by_ip.entry(ip).or_default().push(h.name.clone());
        }
    }
    by_ip.into_iter().filter(|(_, names)| names.len() >= 2).map(|(ip, names)| (ip.to_string(), names)).collect()
}

/// The lines under the table: the tally, then one `same host?` line per shared-IP group.
pub fn render_summary(results: &[PodHealth]) -> Vec<String> {
    let count = |s: Status| results.iter().filter(|h| h.status == s).count();
    let mut lines = vec![format!(
        "{} pass, {} warn, {} fail",
        count(Status::Pass),
        count(Status::Warn),
        count(Status::Fail)
    )];
    for (ip, names) in same_host_failures(results) {
        lines.push(format!(
            "same host? {ip}: {} failing ({}). A bad host breaks every pod on it; \
             a different GPU type draws a different host.",
            names.len(),
            names.join(", ")
        ));
    }
    lines
}

/// The whole human report: table, per-pod checks with `verbose`, then the summary lines.
pub fn render_report(results: &[PodHealth], verbose: bool) -> String {
    let mut out = render_health_table(results);
    if verbose {
        for h in results {
            out.push('\n');
            out.push_str(&render_checks(h));
        }
    }
    out.push('\n');
    for line in render_summary(results) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy 2×A4000 pod on a CUDA 13 host, as the script prints it (one cut of a real
    /// run's shape; python lines come before disk/network because the probe runs in the
    /// background and is collected last).
    const HEALTHY_2GPU: &str = "\
deep_check=1
load1=0.84
load5=1.02
load15=1.10
cpus=64
nproc=16
uptime_secs=1209600
smi=ok
smi_cuda=13.0
gpu.0.name=NVIDIA RTX A4000
gpu.0.driver=580.65.06
gpu.0.mem_total_mib=16376
gpu.0.mem_used_mib=1
gpu.1.name=NVIDIA RTX A4000
gpu.1.driver=580.65.06
gpu.1.mem_total_mib=16376
gpu.1.mem_used_mib=1
smi_gpus=2
python=/root/miniconda3/envs/arena-env/bin/python
torch=ok
torch_version=2.9.0+cu130
torch_cuda=13.0
cuda_available=true
device_count=2
tensor.0=ok
tensor.1=ok
peer.0-1=ok
peer.1-0=ok
nccl_ranks=2
nccl=ok
py_done=1
py_exit=0
disk.root_avail_kb=104857600
disk.workspace_avail_kb=52428800
net.curl_exit=0
net.http=206
net.bytes=33554432
net.secs=0.712
deep_check_end=1
";

    /// `base` with keys replaced (`Some`) or removed (`None`); a key `base` lacks is
    /// appended before the end marker. Keeps each scenario to the lines that differ.
    fn edit(base: &str, edits: &[(&str, Option<&str>)]) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut used = vec![false; edits.len()];
        for line in base.lines() {
            let key = line.split_once('=').map_or(line, |(k, _)| k);
            if key == "deep_check_end" {
                for (i, (k, v)) in edits.iter().enumerate() {
                    if let (false, Some(v)) = (used[i], v) {
                        lines.push(format!("{k}={v}"));
                        used[i] = true;
                    }
                }
            }
            match edits.iter().position(|(k, _)| *k == key) {
                Some(i) => {
                    used[i] = true;
                    if let Some(v) = edits[i].1 {
                        lines.push(format!("{key}={v}"));
                    }
                }
                None => lines.push(line.to_string()),
            }
        }
        lines.join("\n") + "\n"
    }

    /// Remove every key starting with one of `prefixes`.
    fn without(base: &str, prefixes: &[&str]) -> String {
        base.lines()
            .filter(|l| !prefixes.iter().any(|p| l.starts_with(p)))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    fn cuda13() -> HealthPolicy {
        HealthPolicy { min_driver: driver_floor_for_cuda(&["13.0"]), ..Default::default() }
    }

    fn check<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no `{name}` check in {checks:#?}"))
    }

    fn one_gpu() -> String {
        let s = without(HEALTHY_2GPU, &["gpu.1.", "tensor.1", "peer.", "nccl"]);
        edit(
            &s,
            &[
                ("smi_gpus", Some("1")),
                ("device_count", Some("1")),
                ("peer", Some("skipped (1 GPU)")),
                ("nccl", Some("skipped (1 GPU)")),
            ],
        )
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(base64_encode(&[0xff, 0xfe, 0x00, 0x3e, 0x3f]), "//4APj8=");
    }

    #[test]
    fn parse_reads_a_full_run() {
        let f = parse_deep(HEALTHY_2GPU);
        assert!(f.started && f.complete && f.py_done);
        assert_eq!((f.load1, f.cpus, f.nproc, f.uptime_secs), (Some(0.84), Some(64), Some(16), Some(1_209_600)));
        assert_eq!(f.smi.as_deref(), Some("ok"));
        assert_eq!(f.smi_cuda.as_deref(), Some("13.0"));
        assert_eq!(f.gpus.len(), 2);
        assert_eq!(
            f.gpus[1],
            GpuFact {
                index: 1,
                name: Some("NVIDIA RTX A4000".into()),
                driver: Some("580.65.06".into()),
                mem_total_mib: Some(16376),
                mem_used_mib: Some(1),
            }
        );
        assert_eq!(f.driver(), Some("580.65.06"));
        assert_eq!(f.gpu_label(), "2×RTX A4000");
        assert_eq!((f.torch.as_deref(), f.torch_version.as_deref()), (Some("ok"), Some("2.9.0+cu130")));
        assert_eq!((f.cuda_available, f.device_count), (Some(true), Some(2)));
        assert_eq!(f.tensor.get(&1).map(String::as_str), Some("ok"));
        assert_eq!(f.peer.keys().collect::<Vec<_>>(), ["0-1", "1-0"]);
        assert_eq!((f.nccl.as_deref(), f.nccl_ranks), (Some("ok"), Some(2)));
        assert_eq!((f.net_http, f.net_bytes), (Some(206), Some(33_554_432)));
        assert!((f.hf_mbps().unwrap() - 47.13).abs() < 0.01, "{:?}", f.hf_mbps());
        assert_eq!((f.disk_root_avail_kb, f.disk_workspace_avail_kb), (Some(104_857_600), Some(52_428_800)));
        assert!(f.other.is_empty(), "{:?}", f.other);
    }

    #[test]
    fn parse_is_tolerant_of_junk_preamble_and_unknowns() {
        // zshrc chatter before the header (even `key=value`-shaped) is not a fact; junk
        // after it is ignored; `unknown`/unparseable values stay unknown; parsing stops
        // at the end marker; unrecognised keys are kept in `other`.
        let out = "Welcome to your pod!\ntorch=ok\r\ndeep_check=1\r\nload1=unknown\ncpus=0\n\
                   this line has no equals\n=novalue\nbad key=1\ngpu.x.name=odd\ngpu.0.name=NVIDIA A40\n\
                   gpu.0.driver=575.57.08\ndevice_count=two\ntensor.0=ok\npeer.0-x=ok\nerror=mktemp failed\n\
                   deep_check_end=1\ntorch=error: after the end\n";
        let f = parse_deep(out);
        assert!(f.started && f.complete);
        assert_eq!(f.torch, None, "pre-header and post-end lines are ignored");
        assert_eq!((f.load1, f.cpus, f.device_count), (None, None, None));
        assert_eq!(f.gpus.len(), 1);
        assert_eq!(f.gpus[0].driver.as_deref(), Some("575.57.08"));
        assert_eq!(f.gpu_label(), "1×A40");
        assert_eq!(f.tensor.len(), 1);
        assert!(f.peer.is_empty());
        assert_eq!(f.other.get("error").map(String::as_str), Some("mktemp failed"));
        // A malformed per-device key isn't a device; it's kept as an unknown fact.
        assert_eq!(f.other.get("gpu.x.name").map(String::as_str), Some("odd"));
        assert_eq!(f.other.get("peer.0-x").map(String::as_str), Some("ok"));

        // No header at all: nothing is ours.
        let f = parse_deep("zsh: command not found: bash\n");
        assert!(!f.started && !f.complete);
        assert_eq!(f, DeepFacts::default());
        // Header but cut off: started, not complete, missing keys unknown.
        let f = parse_deep("deep_check=1\nsmi=ok\n");
        assert!(f.started && !f.complete);
        assert_eq!((f.smi.as_deref(), f.torch.as_deref()), (Some("ok"), None));
    }

    #[test]
    fn versions_parse_and_compare_numerically() {
        assert_eq!(parse_version("580.65.06"), Some(vec![580, 65, 6]));
        assert_eq!(parse_version(" 580 "), Some(vec![580]));
        for bad in ["", "580.x", "v580", "580..1"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        use std::cmp::Ordering::*;
        assert_eq!(cmp_versions(&[580, 65, 6], &[580]), Greater);
        assert_eq!(cmp_versions(&[580], &[580, 0]), Equal);
        assert_eq!(cmp_versions(&[575, 99], &[580]), Less);
        assert_eq!(cmp_versions(&[535, 129, 3], &[535, 54, 3]), Greater, "numeric, not lexical");
    }

    #[test]
    fn driver_floor_derives_from_config() {
        let cfg = |text: &str| Config::parse(text);
        let floor = |text: &str| HealthPolicy::from_config(&cfg(text)).unwrap().min_driver;
        let v = |d: u32, why: &str| Some(DriverFloor { version: vec![d], why: why.into() });
        let cases: &[(&str, Option<DriverFloor>)] = &[
            ("ALLOWED_CUDA_VERSIONS=\"13.0\"", v(580, "CUDA 13.0")),
            ("ALLOWED_CUDA_VERSIONS=13.1", v(580, "CUDA 13.1")),
            ("ALLOWED_CUDA_VERSIONS=12.8", v(570, "CUDA 12.8")),
            ("ALLOWED_CUDA_VERSIONS=12.4", v(550, "CUDA 12.4")),
            // The lowest listed host CUDA is a legitimate placement, so it sets the floor.
            ("ALLOWED_CUDA_VERSIONS=\"13.0, 12.8\"", v(570, "CUDA 12.8")),
            ("RUNPOD_ALLOWED_CUDA_VERSIONS=13.0", v(580, "CUDA 13.0")),
            ("ALLOWED_CUDA_VERSIONS=12.8\nRUNPOD_ALLOWED_CUDA_VERSIONS=13.0", v(570, "CUDA 12.8")),
            ("ALLOWED_CUDA_VERSIONS=9.9", None),
            ("", None),
            // An explicit floor wins over the derived one; `none` turns the check off.
            ("MIN_DRIVER_VERSION=575.51.03\nALLOWED_CUDA_VERSIONS=13.0", Some(DriverFloor {
                version: vec![575, 51, 3],
                why: "MIN_DRIVER_VERSION".into(),
            })),
            ("MIN_DRIVER_VERSION=none\nALLOWED_CUDA_VERSIONS=13.0", None),
            ("MIN_DRIVER_VERSION=\nALLOWED_CUDA_VERSIONS=13.0", v(580, "CUDA 13.0")),
        ];
        for (text, want) in cases {
            assert_eq!(&floor(text), want, "{text:?}");
        }
        let err = HealthPolicy::from_config(&cfg("MIN_DRIVER_VERSION=latest")).unwrap_err();
        assert!(err.to_string().contains("MIN_DRIVER_VERSION=\"latest\""), "{err}");
    }

    #[test]
    fn healthy_pods_pass() {
        let checks = evaluate(&parse_deep(HEALTHY_2GPU), &cuda13(), None);
        assert_eq!(overall(&checks), Status::Pass, "{checks:#?}");
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "nvidia-smi", "driver", "torch", "cuda", "device_count", "gpu0", "gpu1", "peer_copy", "nccl",
                "network", "disk", "load", "maintenance"
            ]
        );
        assert_eq!(check(&checks, "nvidia-smi").detail, "2×RTX A4000");
        assert_eq!(check(&checks, "driver").detail, "580.65.06 ≥ 580 (CUDA 13.0)");
        assert_eq!(check(&checks, "torch").detail, "2.9.0+cu130 (CUDA 13.0)");
        assert_eq!(check(&checks, "device_count").detail, "2 (matches nvidia-smi)");
        assert_eq!(check(&checks, "peer_copy").detail, "ok (0→1, 1→0)");
        assert_eq!(check(&checks, "nccl").detail, "all_reduce ok (2 ranks)");
        assert_eq!(check(&checks, "network").detail, "47.1 MB/s from huggingface.co");
        assert_eq!(check(&checks, "disk").detail, "/ 107 GB, /workspace 54 GB free");
        assert_eq!(check(&checks, "load").detail, "load 0.8 on 64 CPUs, host up 14d");
        assert_eq!(check(&checks, "maintenance").detail, "none reported");

        // One GPU: nothing to copy between or reduce across — skipped, still a pass.
        let checks = evaluate(&parse_deep(&one_gpu()), &cuda13(), None);
        assert_eq!(overall(&checks), Status::Pass, "{checks:#?}");
        assert_eq!(check(&checks, "peer_copy"), &Check::new("peer_copy", Status::Skip, "skipped (1 GPU)"));
        assert_eq!(check(&checks, "nccl"), &Check::new("nccl", Status::Skip, "skipped (1 GPU)"));
        assert!(!checks.iter().any(|c| c.name == "gpu1"));

        // No driver floor configured: the version is shown, not judged.
        let checks = evaluate(&parse_deep(HEALTHY_2GPU), &HealthPolicy::default(), None);
        assert_eq!(check(&checks, "driver"), &Check::new("driver", Status::Skip, "580.65.06 (no minimum configured)"));
        assert_eq!(overall(&checks), Status::Pass);
    }

    /// Today's real failures (and their neighbours), each as the script reports it.
    #[test]
    fn real_failures_fail_with_the_reason() {
        use Status::*;
        let cuinit_999 = edit(
            &without(HEALTHY_2GPU, &["tensor.", "peer.", "nccl"]),
            &[
                ("cuda_available", Some("false")),
                (
                    "cuda_error",
                    Some(
                        "error: RuntimeError: Unexpected error from cudaGetDeviceCount(). Did you run some cuda \
                         functions before calling NumCudaDevices() that might have already set an error? Error 999: \
                         unknown error",
                    ),
                ),
                ("device_count", Some("0")),
            ],
        );
        let unknown_error = edit(
            HEALTHY_2GPU,
            &[(
                "tensor.1",
                Some(
                    "error: RuntimeError: CUDA error: unknown error CUDA kernel errors might be asynchronously \
                     reported at some other API call, so the stacktrace below might be incorrect.",
                ),
            )],
        );
        let insufficient = edit(
            &one_gpu(),
            &[("tensor.0", Some("error: RuntimeError: CUDA error: CUDA driver version is insufficient for CUDA runtime version"))],
        );
        let peer_mismatch = edit(HEALTHY_2GPU, &[("peer.1-0", Some("mismatch"))]);
        let old_driver = edit(
            &without(HEALTHY_2GPU, &["tensor.", "peer.", "nccl"]),
            &[
                ("smi_cuda", Some("12.2")),
                ("gpu.0.driver", Some("535.129.03")),
                ("gpu.1.driver", Some("535.129.03")),
                ("cuda_available", Some("false")),
                (
                    "cuda_error",
                    Some(
                        "error: RuntimeError: The NVIDIA driver on your system is too old (found version 12020). \
                         Please update your GPU driver",
                    ),
                ),
                ("device_count", Some("0")),
            ],
        );
        let nccl_timeout = edit(HEALTHY_2GPU, &[("nccl", Some("error: timeout after 30s (no result from rank 0, 1)"))]);
        let nccl_error = edit(
            HEALTHY_2GPU,
            &[("nccl", Some("rank 1: error: DistBackendError: NCCL error in: ProcessGroupNCCL.cpp:1970, unhandled cuda error"))],
        );
        let torch_missing = edit(
            &without(HEALTHY_2GPU, &["torch_", "cuda_", "device_count", "tensor.", "peer.", "nccl"]),
            &[("torch", Some("error: ModuleNotFoundError: No module named 'torch'"))],
        );
        let no_python = edit(
            &without(HEALTHY_2GPU, &["torch", "cuda_", "device_count", "tensor.", "peer.", "nccl", "py_"]),
            &[("python", Some("missing"))],
        );
        let smi_missing = edit(
            &without(HEALTHY_2GPU, &["smi_", "gpu.", "tensor.", "peer.", "nccl"]),
            &[
                ("smi", Some("missing")),
                ("cuda_available", Some("false")),
                ("cuda_error", Some("error: RuntimeError: Found no NVIDIA driver on your system.")),
                ("device_count", Some("0")),
            ],
        );
        let smi_error = edit(
            &without(HEALTHY_2GPU, &["smi_", "gpu."]),
            &[("smi", Some("error: NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver."))],
        );
        // The header call worked, the per-GPU query didn't: a GPU fallen off the bus.
        let smi_query_error = edit(
            &without(HEALTHY_2GPU, &["gpu.", "smi_gpus"]),
            &[("smi_query", Some("error: Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error"))],
        );
        let count_mismatch = edit(
            &without(HEALTHY_2GPU, &["tensor.1", "peer.", "nccl"]),
            &[("device_count", Some("1")), ("peer", Some("skipped (1 GPU)")), ("nccl", Some("skipped (1 GPU)"))],
        );
        let py_timeout = edit(
            &without(HEALTHY_2GPU, &["tensor.1", "peer.", "nccl", "py_done"]),
            &[("py_exit", Some("124")), ("py_stderr", Some(""))],
        );

        let cases: &[(&str, &str, &str, Status, &str)] = &[
            ("cuInit 999", &cuinit_999, "cuda", Fail, "RuntimeError: Unexpected error from cudaGetDeviceCount()"),
            ("unknown error on gpu1", &unknown_error, "gpu1", Fail, "RuntimeError: CUDA error: unknown error"),
            ("insufficient driver", &insufficient, "gpu0", Fail, "CUDA driver version is insufficient"),
            ("peer copy wrong", &peer_mismatch, "peer_copy", Fail, "1→0 mismatch"),
            ("old driver", &old_driver, "driver", Fail, "535.129.03 < 580 (CUDA 13.0 needs ≥ 580)"),
            ("old driver also breaks CUDA", &old_driver, "cuda", Fail, "driver on your system is too old"),
            ("nccl timeout", &nccl_timeout, "nccl", Fail, "timeout after 30s (no result from rank 0, 1)"),
            ("nccl error", &nccl_error, "nccl", Fail, "rank 1: error: DistBackendError"),
            ("torch missing", &torch_missing, "torch", Fail, "ModuleNotFoundError: No module named 'torch'"),
            ("torch missing: cuda unknown", &torch_missing, "cuda", Skip, "needs torch"),
            ("torch missing: nccl unknown", &torch_missing, "nccl", Skip, "needs CUDA"),
            ("no python", &no_python, "torch", Fail, "no python on PATH"),
            ("nvidia-smi missing", &smi_missing, "nvidia-smi", Fail, "not found"),
            ("nvidia-smi missing: driver", &smi_missing, "driver", Skip, "needs nvidia-smi"),
            ("nvidia-smi missing: cuda", &smi_missing, "cuda", Fail, "Found no NVIDIA driver"),
            ("nvidia-smi error", &smi_error, "nvidia-smi", Fail, "couldn't communicate with the NVIDIA driver"),
            ("nvidia-smi error: count unknown", &smi_error, "device_count", Pass, "2 (nvidia-smi count unknown)"),
            ("nvidia-smi query error", &smi_query_error, "nvidia-smi", Fail, "Unable to determine the device handle"),
            // Not "nvidia-smi 0": its count is unknown, not zero.
            ("nvidia-smi query error: count unknown", &smi_query_error, "device_count", Pass, "2 (nvidia-smi count unknown)"),
            ("device_count mismatch", &count_mismatch, "device_count", Fail, "torch sees 1 GPU(s), nvidia-smi 2"),
            ("python timed out: missing gpu", &py_timeout, "gpu1", Fail, "no result (python checks timed out)"),
            ("python timed out: peer", &py_timeout, "peer_copy", Fail, "not reported (python checks timed out)"),
            ("python timed out: nccl", &py_timeout, "nccl", Fail, "not reported (python checks timed out)"),
            ("python timed out: gpu0 kept", &py_timeout, "gpu0", Pass, "tensor op ok"),
        ];
        for (label, out, name, status, detail) in cases {
            let checks = evaluate(&parse_deep(out), &cuda13(), None);
            let c = check(&checks, name);
            assert_eq!(c.status, *status, "{label}: {c:?}");
            assert!(c.detail.contains(detail), "{label}: {:?} lacks {detail:?}", c.detail);
            assert_eq!(overall(&checks), Fail, "{label}: the pod fails");
        }

        // The details keep the error and drop torch's advice (the facts keep it all).
        let detail = |out: &str, name: &str| check(&evaluate(&parse_deep(out), &cuda13(), None), name).detail.clone();
        assert_eq!(
            detail(&cuinit_999, "cuda"),
            "RuntimeError: Unexpected error from cudaGetDeviceCount(). Error 999: unknown error"
        );
        assert_eq!(detail(&unknown_error, "gpu1"), "RuntimeError: CUDA error: unknown error");
        assert_eq!(
            detail(&old_driver, "cuda"),
            "RuntimeError: The NVIDIA driver on your system is too old (found version 12020)."
        );
        assert!(parse_deep(&cuinit_999).cuda_error.unwrap().contains("Did you run some cuda functions"));
    }

    #[test]
    fn soft_problems_warn_not_fail() {
        use Status::*;
        let slow = edit(
            HEALTHY_2GPU,
            &[
                ("net.curl_exit", Some("28")),
                ("net.bytes", Some("12000000")),
                ("net.secs", Some("30.001")),
                ("net.error", Some("curl: (28) Operation timed out after 30001 milliseconds with 12000000 out of 33554432 bytes received")),
            ],
        );
        let dns = edit(
            HEALTHY_2GPU,
            &[
                ("net.curl_exit", Some("6")),
                ("net.http", Some("000")),
                ("net.bytes", Some("0")),
                ("net.secs", Some("0.003")),
                ("net.error", Some("curl: (6) Could not resolve host: huggingface.co")),
            ],
        );
        let killed = edit(
            &without(HEALTHY_2GPU, &["net.http", "net.bytes", "net.secs"]),
            &[("net.curl_exit", Some("124"))],
        );
        let rate_limited = edit(HEALTHY_2GPU, &[("net.http", Some("429")), ("net.bytes", Some("812"))]);
        let no_curl = edit(&without(HEALTHY_2GPU, &["net."]), &[("net.curl_exit", Some("missing"))]);
        let low_disk = edit(HEALTHY_2GPU, &[("disk.workspace_avail_kb", Some("3000000"))]);
        let no_disk = without(HEALTHY_2GPU, &["disk."]);
        let busy = edit(HEALTHY_2GPU, &[("load1", Some("41.5")), ("cpus", Some("32"))]);
        let cut_off = without(HEALTHY_2GPU, &["disk.", "net.", "deep_check_end"]);
        let no_smi_driver = without(HEALTHY_2GPU, &["gpu.0.driver", "gpu.1.driver"]);

        let cases: &[(&str, &str, &str, Status, &str)] = &[
            ("slow network", &slow, "network", Warn, "0.4 MB/s from huggingface.co (< 2 MB/s)"),
            ("dns failure", &dns, "network", Warn, "huggingface.co unreachable: curl: (6) Could not resolve host"),
            ("probe killed", &killed, "network", Warn, "huggingface.co unreachable: curl exit 124"),
            ("rate limited", &rate_limited, "network", Warn, "huggingface.co answered HTTP 429"),
            ("no curl", &no_curl, "network", Warn, "curl not found"),
            ("low disk", &low_disk, "disk", Warn, "/workspace 3.1 GB free (< 10 GB)"),
            ("disk unknown", &no_disk, "disk", Warn, "free space on / unknown"),
            ("oversubscribed host", &busy, "load", Warn, "host load 41.5 on 32 CPUs (> 32): oversubscribed"),
            ("cut off", &cut_off, "script", Warn, "output cut off"),
            ("cut off: network", &cut_off, "network", Warn, "not measured"),
            ("driver unparsed", &no_smi_driver, "driver", Warn, "version not reported"),
        ];
        for (label, out, name, status, detail) in cases {
            let checks = evaluate(&parse_deep(out), &cuda13(), None);
            let c = check(&checks, name);
            assert_eq!(c.status, *status, "{label}: {c:?}");
            assert!(c.detail.contains(detail), "{label}: {:?} lacks {detail:?}", c.detail);
            assert_eq!(overall(&checks), Warn, "{label}: warns don't fail the pod: {checks:#?}");
        }

        // The host's maintenance window (API side) warns, with the window.
        let m = Maintenance {
            start: Some("2026-10-09T02:00:00Z".into()),
            end: Some("2026-10-09T06:00:00Z".into()),
            note: Some("host upgrade".into()),
        };
        let checks = evaluate(&parse_deep(HEALTHY_2GPU), &cuda13(), Some(&m));
        assert_eq!(
            check(&checks, "maintenance"),
            &Check::new("maintenance", Warn, "maint 10-09 02:00→06:00 UTC · host upgrade")
        );
        assert_eq!(overall(&checks), Warn);
        // An empty window is no window.
        let checks = evaluate(&parse_deep(HEALTHY_2GPU), &cuda13(), Some(&Maintenance::default()));
        assert_eq!(check(&checks, "maintenance").status, Pass);
    }

    #[test]
    fn load_is_judged_against_the_hosts_cpus_with_a_floor() {
        // (load1, cpus, nproc) -> warns? Load is the host's run queue, so it's compared
        // with the host's CPUs (cpuinfo), not the container's nproc; never below 32.
        let cases: &[(&str, Option<&str>, Option<&str>, bool)] = &[
            ("41.5", Some("32"), Some("8"), true),   // the playbook's "~40" on a 32-CPU host
            ("41.5", Some("128"), Some("8"), false), // the same load on a big host is fine
            ("20", Some("8"), Some("8"), false),     // small box: under the floor
            ("33", Some("8"), Some("8"), true),      // ...but past the floor it warns
            ("33", None, Some("8"), true),           // cpuinfo unknown: nproc stands in
            ("31.9", None, None, false),             // no CPU count at all: the floor alone
            ("200", Some("256"), None, false),
        ];
        for (load, cpus, nproc, warns) in cases {
            let out = edit(HEALTHY_2GPU, &[("load1", Some(load)), ("cpus", *cpus), ("nproc", *nproc)]);
            let c = check(&evaluate(&parse_deep(&out), &cuda13(), None), "load").clone();
            assert_eq!(c.status == Status::Warn, *warns, "load {load} cpus {cpus:?} nproc {nproc:?}: {c:?}");
        }
        let out = without(HEALTHY_2GPU, &["load"]);
        assert_eq!(check(&evaluate(&parse_deep(&out), &cuda13(), None), "load").status, Status::Skip);
    }

    #[test]
    fn no_output_is_one_clear_failure() {
        for out in ["", "Permission denied (publickey).", "zsh:1: command not found: base64\n"] {
            let checks = evaluate(&parse_deep(out), &cuda13(), None);
            assert_eq!(checks, [Check::new("script", Status::Fail, "no output from the check script")], "{out:?}");
        }
    }

    #[test]
    fn cpu_pods_skip_gpu_checks() {
        // A Hetzner VM: no nvidia-smi, CPU torch — healthy for what it is.
        let out = edit(
            &without(HEALTHY_2GPU, &["smi_", "gpu.", "tensor.", "peer.", "nccl", "device_count"]),
            &[
                ("smi", Some("missing")),
                ("torch_version", Some("2.9.0+cpu")),
                ("torch_cuda", Some("none")),
                ("cuda_available", Some("false")),
                ("cuda_error", Some("error: RuntimeError: Found no NVIDIA driver on your system.")),
            ],
        );
        let hetzner = Pod { name: "devtest-cpu".into(), provider: "hetzner".into(), ..Default::default() };
        assert!(!expects_gpu(&hetzner));
        assert!(expects_gpu(&Pod { provider: "runpod".into(), ..Default::default() }));
        // The API's GPU count never switches the GPU checks off on a GPU provider.
        assert!(expects_gpu(&Pod { provider: "vast".into(), gpu_count: Some(0), ..Default::default() }));
        let h = PodHealth::checked(&hetzner, parse_deep(&out), &cuda13());
        assert_eq!(h.status, Status::Pass, "{:#?}", h.checks);
        assert_eq!(check(&h.checks, "gpu"), &Check::new("gpu", Status::Skip, "CPU pod: GPU checks skipped"));
        assert!(!h.checks.iter().any(|c| ["nvidia-smi", "driver", "cuda"].contains(&c.name.as_str())));
        // The same output from a GPU pod is a failure.
        let gpu_pod = Pod { name: "devtest-gpu".into(), provider: "runpod".into(), ..Default::default() };
        assert_eq!(PodHealth::checked(&gpu_pod, parse_deep(&out), &cuda13()).status, Status::Fail);
    }

    #[test]
    fn a_gpu_pod_the_api_reports_with_0_gpus_is_judged_as_a_gpu_pod() {
        // RunPod's `gpuCount: 0` (a pod resumed without a GPU, or one the API lost track
        // of) on a machine that indeed has no usable GPU: FAIL, not a CPU-pod PASS.
        let no_gpu = edit(
            &without(HEALTHY_2GPU, &["smi_", "gpu.", "tensor.", "peer.", "nccl", "device_count"]),
            &[
                ("smi", Some("missing")),
                ("cuda_available", Some("false")),
                ("cuda_error", Some("error: RuntimeError: Found no NVIDIA driver on your system.")),
            ],
        );
        let zero = Pod { name: "devtest-zero".into(), provider: "runpod".into(), gpu_count: Some(0), ..Default::default() };
        let h = PodHealth::checked(&zero, parse_deep(&no_gpu), &cuda13());
        assert_eq!(h.status, Status::Fail, "{:#?}", h.checks);
        assert_eq!(check(&h.checks, "nvidia-smi").status, Status::Fail);
        assert_eq!(check(&h.checks, "cuda").status, Status::Fail);
        assert!(!h.checks.iter().any(|c| c.name == "gpu"), "no CPU-pod skip line: {:#?}", h.checks);
        assert_eq!(
            check(&h.checks, "provider"),
            &Check::new("provider", Status::Warn, "reports 0 GPUs for this pod (resumed without one?)")
        );
        // A machine that does have working GPUs passes its checks; the API mismatch only warns.
        let h = PodHealth::checked(&zero, parse_deep(HEALTHY_2GPU), &cuda13());
        assert_eq!(h.status, Status::Warn, "{:#?}", h.checks);
        // No hint when the provider reports GPUs (or says nothing).
        let one = Pod { gpu_count: Some(1), ..zero.clone() };
        assert!(!PodHealth::checked(&one, parse_deep(HEALTHY_2GPU), &cuda13()).checks.iter().any(|c| c.name == "provider"));
    }

    fn pod(name: &str, ip: &str) -> Pod {
        Pod {
            name: format!("devtest-{name}"),
            provider: "runpod".into(),
            ssh_ip: Some(ip.into()),
            ssh_port: Some(22),
            ..Default::default()
        }
    }

    #[test]
    fn report_table_checks_and_same_host_summary() {
        let slow = edit(HEALTHY_2GPU, &[("net.bytes", Some("12000000")), ("net.secs", Some("30.001"))]);
        let peer = edit(HEALTHY_2GPU, &[("peer.1-0", Some("mismatch"))]);
        let results = vec![
            PodHealth::checked(&pod("apple", "1.1.1.1"), parse_deep(HEALTHY_2GPU), &cuda13()),
            PodHealth::checked(&pod("bloom", "2.2.2.2"), parse_deep(&slow), &cuda13()),
            PodHealth::checked(&pod("cloud", "3.3.3.3"), parse_deep(&peer), &cuda13()),
            PodHealth::unreachable(&pod("dune", "3.3.3.3"), "timed out after 150s"),
            PodHealth::unreachable(&pod("echo", "4.4.4.4"), "exit Some(255): Connection refused"),
        ];
        assert_eq!(
            render_health_table(&results),
            "\
NAME           RESULT  GPUS         DRIVER     CUDA        NET  NOTES
devtest-apple  pass    2×RTX A4000  580.65.06  13.0  47.1 MB/s
devtest-bloom  warn    2×RTX A4000  580.65.06  13.0   0.4 MB/s  network: 0.4 MB/s from huggingface.co (< 2 MB/s)
devtest-cloud  fail    2×RTX A4000  580.65.06  13.0  47.1 MB/s  peer_copy: 1→0 mismatch
devtest-dune   fail    -            -          -             -  ssh: timed out after 150s
devtest-echo   fail    -            -          -             -  ssh: exit Some(255): Connection refused
"
        );
        // Two failing pods on one IP: probably the machine.
        assert_eq!(
            same_host_failures(&results),
            [("3.3.3.3".to_string(), vec!["devtest-cloud".to_string(), "devtest-dune".to_string()])]
        );
        assert_eq!(
            render_summary(&results),
            [
                "1 pass, 1 warn, 3 fail",
                "same host? 3.3.3.3: 2 failing (devtest-cloud, devtest-dune). A bad host breaks every pod on it; \
                 a different GPU type draws a different host.",
            ]
        );
        // A shared IP with only one failure (or none) is not flagged.
        let ok_neighbour = vec![results[0].clone(), PodHealth::unreachable(&pod("fig", "1.1.1.1"), "x")];
        assert!(same_host_failures(&ok_neighbour).is_empty());

        // -v: every check, with its status symbol.
        let v = render_checks(&results[2]);
        assert!(v.starts_with("── devtest-cloud (fail)\n  ✓ nvidia-smi    2×RTX A4000\n"), "{v}");
        assert!(v.contains("\n  ✗ peer_copy     1→0 mismatch\n"), "{v}");
        assert!(render_report(&results, true).contains("── devtest-dune (fail)\n  ✗ ssh  timed out after 150s\n"));
        assert!(!render_report(&results, false).contains("── "));
    }

    #[test]
    fn same_host_groups_only_real_machine_ips() {
        let failing = |p: &Pod| PodHealth::unreachable(p, "timed out after 150s");
        let vast = |name: &str, host: &str| Pod { provider: "vast".into(), ..pod(name, host) };
        // Two Vast pods behind the same SSH proxy are not "the same host".
        let results = vec![failing(&vast("apple", "ssh4.vast.ai")), failing(&vast("bloom", "ssh4.vast.ai"))];
        assert!(same_host_failures(&results).is_empty());
        assert_eq!(render_summary(&results), ["0 pass, 0 warn, 2 fail"]);
        // ...even if Vast hands out an IP-literal proxy address.
        let results = vec![failing(&vast("apple", "203.0.113.9")), failing(&vast("bloom", "203.0.113.9"))];
        assert!(same_host_failures(&results).is_empty());
        // A hostname (not an IP) never groups, on any provider.
        let results = vec![failing(&pod("apple", "proxy.example")), failing(&pod("bloom", "proxy.example"))];
        assert!(same_host_failures(&results).is_empty());
        // Real IPs do, IPv6 included.
        let results = vec![failing(&pod("apple", "2001:db8::7")), failing(&pod("bloom", "2001:db8::7"))];
        assert_eq!(
            same_host_failures(&results),
            [("2001:db8::7".to_string(), vec!["devtest-apple".to_string(), "devtest-bloom".to_string()])]
        );
        assert_eq!(host_ip(&pod("apple", "1.2.3.4")).as_deref(), Some("1.2.3.4"));
        assert_eq!(host_ip(&Pod { ssh_ip: None, ..pod("apple", "") }), None);
    }

    #[test]
    fn json_is_per_pod_health_without_the_ip() {
        let results = vec![
            PodHealth::checked(&pod("apple", "1.1.1.1"), parse_deep(HEALTHY_2GPU), &cuda13()),
            PodHealth::unreachable(&pod("dune", "3.3.3.3"), "timed out after 150s"),
        ];
        let v: serde_json::Value = serde_json::to_value(&results).unwrap();
        let mut keys: Vec<&str> = v[0].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["checks", "facts", "id", "name", "provider", "status"]);
        assert_eq!(v[0]["status"], "pass");
        assert_eq!(v[0]["checks"][0], serde_json::json!({"name": "nvidia-smi", "status": "pass", "detail": "2×RTX A4000"}));
        assert_eq!(v[0]["facts"]["gpus"][0]["driver"], "580.65.06");
        assert_eq!(v[0]["facts"]["tensor"]["1"], "ok");
        assert_eq!(v[1]["status"], "fail");
        assert!(v[1]["facts"].is_null());
        assert!(!v.to_string().contains("1.1.1.1"), "no IPs in the JSON");
    }

    // ---- the script itself (run locally; no network, no GPU) ----

    fn script_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/deep_check.sh")
    }

    fn have(tool: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arena-health-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn script_is_valid_bash() {
        let out = std::process::Command::new("bash").arg("-n").arg(script_path()).output().expect("bash");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn script_python_part_compiles() {
        if !have("python3") {
            eprintln!("python3 not installed — skipping");
            return;
        }
        let start = DEEP_CHECK_SCRIPT.find("<<'PYEOF'\n").expect("python heredoc") + "<<'PYEOF'\n".len();
        let len = DEEP_CHECK_SCRIPT[start..].find("\nPYEOF\n").expect("heredoc end");
        let dir = scratch("pycompile");
        let file = dir.join("deep_check.py");
        std::fs::write(&file, &DEEP_CHECK_SCRIPT[start..start + len]).unwrap();
        let out = std::process::Command::new("python3").args(["-I", "-m", "py_compile"]).arg(&file).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn command_carries_the_script_base64_inside_the_login_wrap() {
        let cmd = deep_check_command(Some("arena-env"));
        assert_eq!(cmd, crate::ssh::login_shell_wrap(&deep_check_inner_command(), Some("arena-env")));
        assert!(cmd.contains(&base64_encode(DEEP_CHECK_SCRIPT.as_bytes())));
        assert!(cmd.contains("conda activate arena-env"));
        // One argv string on the remote side: stay far below Linux's 128 KiB per-arg cap.
        assert!(cmd.len() < 64 * 1024, "{} bytes", cmd.len());
        if have("base64") {
            let decoded = std::process::Command::new("sh")
                .args(["-c", &format!("printf %s {} | base64 -d", base64_encode(DEEP_CHECK_SCRIPT.as_bytes()))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&decoded.stdout), DEEP_CHECK_SCRIPT);
        }
    }

    /// The delivered command, run for real against stub `nvidia-smi` / `curl` / `python`
    /// (first on PATH — so no GPU, no network): the bash plumbing produces facts the
    /// parser and evaluator understand, end to end.
    #[cfg(target_os = "linux")]
    #[test]
    fn script_runs_against_stub_tools_and_parses() {
        use std::os::unix::fs::PermissionsExt;
        if !(have("bash") && have("timeout") && have("base64")) {
            eprintln!("bash/timeout/base64 missing — skipping");
            return;
        }
        let dir = scratch("stubs");
        let stub = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub(
            "nvidia-smi",
            "case \"$*\" in\n\
             *--query-gpu*) printf '0, NVIDIA RTX A4000, 580.65.06, 16376, 1\\n1, NVIDIA RTX A4000, 580.65.06, 16376, 3\\n' ;;\n\
             *) echo '| NVIDIA-SMI 580.65.06    Driver Version: 580.65.06    CUDA Version: 13.0     |' ;;\n\
             esac\n",
        );
        // Only answers the expected byte-range request to the HF file.
        stub(
            "curl",
            "case \"$*\" in *'-r 0-33554431'*huggingface.co/*) printf '206 33554432 0.712' ;; *) echo \"bad args: $*\" >&2; exit 3 ;; esac\n",
        );
        // Stands in for the conda env's python: checks it was handed the script file.
        stub(
            "python",
            "grep -q 'def nccl_check' \"$1\" || { echo 'torch=error: stub got no script'; exit 0; }\n\
             echo 'not a fact line'\n\
             printf 'torch=ok\\ntorch_version=2.9.0+cu130\\ntorch_cuda=13.0\\ncuda_available=true\\ndevice_count=2\\n'\n\
             printf 'tensor.0=ok\\ntensor.1=ok\\npeer.0-1=ok\\npeer.1-0=ok\\nnccl_ranks=2\\nnccl=ok\\npy_done=1\\n'\n",
        );
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(deep_check_inner_command())
            .env("PATH", path)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        // Only key=value lines on stdout (the python stub's junk line aside).
        for line in stdout.lines().filter(|l| *l != "not a fact line") {
            assert!(line.contains('='), "stray stdout line {line:?}");
        }

        let f = parse_deep(&stdout);
        assert!(f.started && f.complete, "{stdout}");
        assert_eq!((f.smi.as_deref(), f.smi_cuda.as_deref(), f.smi_gpus), (Some("ok"), Some("13.0"), Some(2)));
        assert_eq!(f.gpus[1].mem_used_mib, Some(3));
        assert!(f.python.as_deref().is_some_and(|p| p.ends_with("/python")), "{:?}", f.python);
        assert_eq!(f.py_exit, Some(0));
        assert_eq!((f.net_curl_exit.as_deref(), f.net_http, f.net_bytes), (Some("0"), Some(206), Some(33_554_432)));
        assert!(f.load1.is_some() && f.cpus.is_some() && f.uptime_secs.is_some(), "{f:?}");
        assert!(f.disk_root_avail_kb.is_some(), "{f:?}");

        let checks = evaluate(&f, &cuda13(), None);
        for name in ["nvidia-smi", "driver", "torch", "cuda", "device_count", "gpu0", "gpu1", "peer_copy", "nccl", "network"] {
            assert_eq!(check(&checks, name).status, Status::Pass, "{name}: {checks:#?}");
        }
    }
}
