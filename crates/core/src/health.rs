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

use serde::{Deserialize, Serialize};

use crate::base64::encode as base64_encode;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::fleet::{clip, maintenance_label};
use crate::metrics::normalize_gpu_name;
use crate::pod::{Maintenance, Pod};
use crate::ssh::SshOutput;
use crate::table::{self, Align};

/// The on-pod fact gatherer. Embedded (like `hetzner_setup.sh`) so the binary is all an
/// operator needs, and versioned with the parser that reads its output.
pub const DEEP_CHECK_SCRIPT: &str = include_str!("deep_check.sh");

/// The whole remote call's budget. The script bounds itself to ≈120s worst case (each
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

/// One GPU as torch sees it (`tgpu.<i>.*`, printed only when CUDA is available). Under MIG
/// torch names the slice (`NVIDIA A100-SXM4-40GB MIG 1g.5gb`), and it answers where nvidia-smi
/// can't (a slice's container without NVML access): what labels fall back on.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TorchGpuFact {
    pub index: u32,
    pub name: Option<String>,
    pub mem_total_mib: Option<u64>,
    /// Free after the tensor op (`torch.cuda.mem_get_info`).
    pub mem_free_mib: Option<u64>,
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
    /// The driver version in `/proc/driver/nvidia/version` (its `NVRM version:` line): what the
    /// driver check reads when nvidia-smi gives none — a container usually sees the file even
    /// where NVML is blocked (a MIG slice). Only ever a dotted version.
    pub driver_proc: Option<String>,
    /// Error from the `--query-gpu` call, when the header call worked but that didn't.
    pub smi_query: Option<String>,
    pub smi_gpus: Option<u32>,
    pub gpus: Vec<GpuFact>,
    /// MIG slices `nvidia-smi -L` lists (`MIG 1g.34gb Device 0`), and the first one's
    /// profile: under MIG the pod gets a slice, and `--query-gpu` names only the parent card.
    pub mig_devices: Option<u32>,
    pub mig_profile: Option<String>,
    /// Not measured: what the provider calls this pod's MIG slice (`B300 MIG 34GB`, from its
    /// listing — see [`mig_slice_name`]), so every label names the slice, not the parent
    /// (finding #24). Set by [`PodHealth::checked`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice: Option<String>,
    /// Not measured: how many GPUs the provider reports for the pod ([`Pod::gpu_count`]) — what
    /// torch's count is held to beside nvidia-smi's, so a GPU gone from both (fallen off the
    /// bus, nvidia-smi listing only the survivors or unable to count) still shows. Set by
    /// [`PodHealth::checked`]. Not in `--json`: it is the listing's (`pods list --json`).
    #[serde(skip)]
    pub provider_gpus: Option<u32>,
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
    /// The newest CUDA the driver supports, as the torch step reads it (13020 → `13.2`):
    /// nvidia-smi's "CUDA Version" when nvidia-smi can't answer. From the driver library
    /// (`cuDriverGetVersion` in libcuda), else torch's private `_cuda_getDriverVersion` — which
    /// torch 2.14 doesn't have (live, a B300 MIG slice: it was never known). `major.minor` only.
    pub torch_driver_cuda: Option<String>,
    /// The GPUs as torch sees them (`tgpu.<i>.*`), by index.
    pub torch_gpus: Vec<TorchGpuFact>,
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
    /// The newest pre-installed VS Code server's commit (`none` = none), and how many there
    /// are (setup's warm-up; informational).
    pub vscode_server: Option<String>,
    pub vscode_servers: Option<u32>,
    /// Extensions in `~/.vscode-server/extensions`: lowercased id → version.
    pub vscode_extensions: BTreeMap<String, String>,
    /// `python.defaultInterpreterPath` in the machine settings (`unset` = absent).
    pub vscode_python: Option<String>,
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

    /// The driver version: the lowest any GPU reports to nvidia-smi (they share one driver;
    /// the lowest is the safe one to judge), else `/proc/driver/nvidia/version`'s — there even
    /// when nvidia-smi can't run (a MIG slice).
    pub fn driver(&self) -> Option<&str> {
        self.smi_driver().or(self.driver_proc.as_deref())
    }

    /// The driver version as nvidia-smi reports it (see [`Self::driver`]).
    fn smi_driver(&self) -> Option<&str> {
        self.gpus
            .iter()
            .filter_map(|g| g.driver.as_deref().and_then(|d| parse_version(d).map(|v| (v, d))))
            .min_by(|a, b| cmp_versions(&a.0, &b.0))
            .map(|(_, d)| d)
    }

    /// The newest CUDA the driver supports: nvidia-smi's header, else torch's reading of it —
    /// `true` when it is torch's (the table marks it).
    pub fn driver_cuda(&self) -> Option<(&str, bool)> {
        match (self.smi_cuda.as_deref(), self.torch_driver_cuda.as_deref()) {
            (Some(c), _) => Some((c, false)),
            (None, Some(c)) => Some((c, true)),
            (None, None) => None,
        }
    }

    /// Whether the pod is (on) a MIG slice: the provider's name for its GPU says so (`slice`),
    /// `nvidia-smi -L` lists slices, or torch names one (`… MIG 1g.5gb`) — the last is all
    /// there is when nvidia-smi can't run in the slice's container.
    fn is_mig(&self) -> bool {
        self.slice.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.mig_devices.is_some_and(|n| n > 0)
            || self.torch_gpus.iter().any(|g| g.name.as_deref().is_some_and(names_mig))
    }

    /// MIG slices beside whole GPUs, as far as the facts tell: `--query-gpu` lists more cards
    /// than `nvidia-smi -L` lists slices. Each slice has one parent card, so the rest are whole,
    /// and the slices are no count to hold torch to: such a machine is counted as whole GPUs, as
    /// before the MIG fallback. One whose whole cards don't outnumber its slices can't be told
    /// from a pure MIG one by these facts — not supported (README); it is counted as MIG.
    fn mixed_inventory(&self) -> bool {
        match (self.mig_devices.filter(|&s| s > 0), self.smi_count()) {
            (Some(slices), Some(cards)) => cards > slices,
            _ => false,
        }
    }

    /// The MIG slices seen: those `nvidia-smi -L` lists, else torch's devices named as slices
    /// (`… MIG 1g.5gb`). `None` when neither shows one.
    fn mig_seen(&self) -> Option<u32> {
        let named = self.torch_gpus.iter().filter(|g| g.name.as_deref().is_some_and(names_mig)).count() as u32;
        self.mig_devices.filter(|&n| n > 0).or((named > 0).then_some(named))
    }

    /// Positive evidence that every GPU the pod was given is a MIG slice: what the MIG fallback
    /// ([`smi_check`]) and a provider undercount's WARN ([`held_to_provider`]) need (review of
    /// `db8dff8..da53b0c`, codex). [`Self::is_mig`] shows only that slices *exist*. Take a mixed
    /// pod, a whole card beside a slice, whose whole card fell off the bus: `--query-gpu` fails,
    /// so there is no inventory to call it mixed by ([`Self::mixed_inventory`]), and `-L` and
    /// torch still show the slice. It passed through the fallback, and its missing card read as
    /// a MIG undercount (a WARN, exit 0). So, besides MIG and no whole cards in the inventory:
    /// - the GPUs the provider rented the pod with, when known, are no more than the slices
    ///   seen ([`Self::mig_seen`]);
    /// - with nvidia-smi unable to count them, no more than torch sees;
    /// - torch names no whole card beside a slice.
    ///
    /// Anything short of that is ambiguous, and is judged as whole GPUs: a FAIL. Without the
    /// provider's count, MIG and an inventory that isn't mixed are all there is to go on.
    fn only_slices(&self) -> bool {
        let named_whole = self.torch_gpus.iter().any(|g| g.name.as_deref().is_some_and(|n| !names_mig(n)));
        let named_slice = self.torch_gpus.iter().any(|g| g.name.as_deref().is_some_and(names_mig));
        if !self.is_mig() || self.mixed_inventory() || (named_whole && named_slice) {
            return false;
        }
        let Some(p) = self.provider_gpus else { return true };
        let uncounted_short = self.smi_count().is_none() && self.device_count.is_none_or(|t| p > t);
        p <= self.mig_seen().unwrap_or(0) && !uncounted_short
    }

    /// torch's own proof that the GPUs work: imported, CUDA available, at least one device, and
    /// the tensor op `ok` on every one (none missing, none failed). What stands in for
    /// nvidia-smi when it can't answer on a MIG slice — and nothing short of all of it: a GPU
    /// torch can't use is exactly what nvidia-smi's FAIL would have been about. It says nothing
    /// of a GPU torch doesn't enumerate, which is why it stands in nowhere else ([`smi_check`]).
    fn torch_proves_gpus(&self) -> bool {
        let Some(n) = self.device_count.filter(|&n| n > 0) else { return false };
        self.torch.as_deref() == Some("ok")
            && self.cuda_available == Some(true)
            && (0..n).all(|i| self.tensor.get(&i).is_some_and(|r| r == "ok"))
            && self.tensor.values().all(|r| r == "ok")
    }

    /// The GPU memory torch reports (`4.8 GB`, `80 GB each`; GiB, as GPU memory is sold), when
    /// every GPU it lists says the same — else `None`.
    fn torch_mem(&self) -> Option<String> {
        let first = self.torch_gpus.first()?.mem_total_mib?;
        if self.torch_gpus.iter().any(|g| g.mem_total_mib != Some(first)) {
            return None;
        }
        let each = if self.torch_gpus.len() > 1 { " each" } else { "" };
        Some(format!("{}{each}", fmt_gb(first as f64 / 1024.0)))
    }

    /// GPUs as the machine sees them — `2×RTX A4000`, `1×A100 80GB PCIe+1×H100` for a
    /// mixed box — else torch's count (`2×GPU`), else `-`. A MIG slice is labelled as the
    /// slice (finding #24): the provider's name for it when known (`1×B300 MIG 34GB`), else
    /// the parent plus the profile `nvidia-smi -L` shows (`1×B300 SXM6 AC MIG 1g.34gb`) —
    /// never the bare parent card, which is not what the pod has. Where nvidia-smi names no
    /// GPU (it can't run, or answers `[N/A]`) the names are torch's; torch's win outright when
    /// they name a MIG slice (`1×A100-SXM4-40GB MIG 1g.5gb`), nvidia-smi's row being the parent.
    pub fn gpu_label(&self) -> String {
        let slices = self.mig_devices.filter(|&n| n > 0);
        if let Some(slice) = self.slice.as_deref().filter(|s| !s.trim().is_empty()) {
            let n = slices.or(self.device_count.filter(|&n| n > 0)).unwrap_or(1);
            return format!("{n}×{slice}");
        }
        if let (Some(n), Some(parent)) = (slices, self.gpus.first().and_then(|g| g.name.as_deref())) {
            let profile = self.mig_profile.as_deref().map(|p| format!(" {p}")).unwrap_or_default();
            return format!("{n}×{} MIG{profile}", normalize_gpu_name(parent));
        }
        let torch_named = self.torch_gpus.iter().any(|g| g.name.is_some());
        let torch_mig = self.torch_gpus.iter().any(|g| g.name.as_deref().is_some_and(names_mig));
        let smi_named = self.gpus.iter().any(|g| g.name.is_some());
        let names: Vec<Option<&str>> = if torch_mig || (torch_named && !smi_named) {
            self.torch_gpus.iter().map(|g| g.name.as_deref()).collect()
        } else {
            self.gpus.iter().map(|g| g.name.as_deref()).collect()
        };
        let mut groups: Vec<(String, usize)> = Vec::new();
        for name in names {
            let name = name.map(normalize_gpu_name).unwrap_or_else(|| "GPU".into());
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
    /// "nvidia-smi sees 0 GPUs" next to torch's count. Nor, on a MIG slice, is a listing of
    /// none a count: that is nvidia-smi unable to see the slice from its container, which the
    /// nvidia-smi check judges (FAIL, or torch's proof). On whole GPUs it is one — 0, which
    /// torch's count then disagrees with — as it was before the MIG fallback.
    fn smi_count(&self) -> Option<u32> {
        if self.smi.as_deref() != Some("ok") || self.smi_query.is_some() {
            return None;
        }
        self.smi_gpus
            .or_else(|| (!self.gpus.is_empty()).then_some(self.gpus.len() as u32))
            .filter(|&n| n > 0 || !self.is_mig())
    }

    /// torch imported, built without CUDA (`torch.version.cuda` is None: `torch_cuda=none`).
    fn cpu_only_torch(&self) -> bool {
        self.torch.as_deref() == Some("ok") && self.torch_cuda.as_deref() == Some("none")
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
    let mut tgpus: BTreeMap<u32, TorchGpuFact> = BTreeMap::new();
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
            "driver_proc" => f.driver_proc = text().filter(|v| parse_version(v).is_some_and(|d| d[0] > 0)),
            "smi_query" => f.smi_query = text(),
            "smi_gpus" => f.smi_gpus = small(),
            "mig_devices" => f.mig_devices = small(),
            "mig_profile" => f.mig_profile = text(),
            "python" => f.python = text(),
            "py_exit" => f.py_exit = value.parse().ok(),
            "py_stderr" => f.py_stderr = text(),
            "py_done" => f.py_done = value == "1",
            "torch" => f.torch = text(),
            "torch_version" => f.torch_version = text(),
            "torch_cuda" => f.torch_cuda = text(),
            "torch_driver_cuda" => {
                f.torch_driver_cuda = text().filter(|v| parse_version(v).is_some_and(|c| c.len() == 2 && c[0] > 0))
            }
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
            "vscode.server" => f.vscode_server = text(),
            "vscode.servers" => f.vscode_servers = small(),
            "vscode.python" => f.vscode_python = text(),
            _ => {
                if let Some(v) = text() {
                    if !indexed_fact(&mut f, &mut gpus, &mut tgpus, key, v.clone()) {
                        f.other.insert(key.to_string(), v);
                    }
                }
            }
        }
    }
    f.gpus = gpus.into_values().collect();
    f.torch_gpus = tgpus.into_values().collect();
    f
}

/// `gpu.<i>.<field>`, `tgpu.<i>.<field>`, `tensor.<i>`, `peer.<i>-<j>`: the per-device facts.
/// `false` if `key` isn't one of them.
fn indexed_fact(
    f: &mut DeepFacts,
    gpus: &mut BTreeMap<u32, GpuFact>,
    tgpus: &mut BTreeMap<u32, TorchGpuFact>,
    key: &str,
    value: String,
) -> bool {
    if let Some(rest) = key.strip_prefix("gpu.") {
        let Some((idx, field)) = rest.split_once('.') else { return false };
        let Ok(index) = idx.parse::<u32>() else { return false };
        if !matches!(field, "name" | "driver" | "mem_total_mib" | "mem_used_mib") {
            return false;
        }
        // nvidia-smi's placeholders (`[N/A]`, `[Insufficient Permissions]` — a MIG slice's
        // fields) are no value: the row is there, the field unknown.
        let value = Some(value).filter(|v| !(v.starts_with('[') && v.ends_with(']')));
        let g = gpus.entry(index).or_insert_with(|| GpuFact { index, ..Default::default() });
        match field {
            "name" => g.name = value,
            "driver" => g.driver = value,
            "mem_total_mib" => g.mem_total_mib = value.and_then(|v| v.parse().ok()),
            _ => g.mem_used_mib = value.and_then(|v| v.parse().ok()),
        }
        return true;
    }
    if let Some(rest) = key.strip_prefix("tgpu.") {
        let Some((idx, field)) = rest.split_once('.') else { return false };
        let Ok(index) = idx.parse::<u32>() else { return false };
        if !matches!(field, "name" | "mem_total_mib" | "mem_free_mib") {
            return false;
        }
        let g = tgpus.entry(index).or_insert_with(|| TorchGpuFact { index, ..Default::default() });
        match field {
            "name" => g.name = Some(value),
            "mem_total_mib" => g.mem_total_mib = value.parse().ok(),
            _ => g.mem_free_mib = value.parse().ok(),
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
    if let Some(id) = key.strip_prefix("vscode.ext.") {
        let valid = !id.is_empty();
        if valid {
            f.vscode_extensions.insert(id.to_ascii_lowercase(), value);
        }
        return valid;
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
    /// The host CUDA version the floor was derived from (`13.0`; `None` for an explicit
    /// `MIN_DRIVER_VERSION`): what the CUDA version torch says the driver supports is held to
    /// when no driver version is known (nvidia-smi can't run, no `/proc/driver/nvidia/version`).
    pub cuda: Option<String>,
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
    /// The extensions setup's VS Code warm-up installs (`VSCODE_EXTENSIONS`; none when
    /// `VSCODE_PREINSTALL=0`), for the informational `vscode` line.
    pub vscode_extensions: Vec<String>,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            min_driver: None,
            min_hf_mbps: DEFAULT_MIN_HF_MBPS,
            min_free_gb: DEFAULT_MIN_FREE_GB,
            load_floor: DEFAULT_LOAD_FLOOR,
            vscode_extensions: crate::vscode::DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl HealthPolicy {
    /// The policy from config: `MIN_DRIVER_VERSION` (e.g. `580` or `580.65.06`; `none`
    /// turns the check off) wins; otherwise the floor is derived from the host CUDA floor
    /// pods are created with (`MIN_CUDA_VERSION`, else the lowest of `ALLOWED_CUDA_VERSIONS` /
    /// `RUNPOD_ALLOWED_CUDA_VERSIONS`, or of `EXACT_CUDA_VERSIONS` — what `PodSpec` reads),
    /// see [`driver_floor_for_cuda`]. A malformed `MIN_DRIVER_VERSION` is an error, not a
    /// silently skipped check. The `vscode` line expects what setup's warm-up installs
    /// (`VSCODE_PREINSTALL` / `VSCODE_EXTENSIONS`, validated the same way).
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let mut policy = Self { vscode_extensions: crate::vscode::expected_extensions(cfg)?, ..Self::default() };
        if let Some(raw) = cfg.get("MIN_DRIVER_VERSION").map(str::trim).filter(|s| !s.is_empty()) {
            if raw.eq_ignore_ascii_case("none") {
                return Ok(policy);
            }
            let version = parse_version(raw).ok_or_else(|| {
                Error::Config(format!("MIN_DRIVER_VERSION={raw:?} is not a driver version like 580 or 580.65.06"))
            })?;
            policy.min_driver = Some(DriverFloor { version, why: "MIN_DRIVER_VERSION".into(), cuda: None });
            return Ok(policy);
        }
        // The same host CUDA floor creates ask for (`crate::pod::cuda_from_config`: the
        // floor, or the lowest of an exact set), so the check never fails a host the
        // config deliberately admits.
        let cuda = crate::pod::cuda_from_config(cfg);
        // One that isn't a version fails closed, like a malformed MIN_DRIVER_VERSION: read as
        // "no floor" the check would stop holding hosts to any driver (every create refuses it).
        if let Some(bad) = cuda.bad_value() {
            return Err(Error::Config(format!(
                "{}: `{bad}` isn't a CUDA version like 13.0, so the deep check has no driver floor to derive \
                 (fix it, or set MIN_DRIVER_VERSION)",
                cuda.source.unwrap_or("ALLOWED_CUDA_VERSIONS")
            )));
        }
        let floor = cuda.floor.or_else(|| crate::pod::lowest_cuda(&cuda.exact));
        policy.min_driver = floor.as_deref().and_then(|f| driver_floor_for_cuda(&[f]));
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
        .map(|(d, v)| DriverFloor { version: vec![d], why: format!("CUDA {v}"), cuda: Some(v.trim().to_string()) })
}

/// The CUDA version a driver must support to meet the floor derived from host CUDA `cuda`
/// ([`driver_floor_for_cuda`]): `cuda` itself (`12.4` → `[12, 4]`), except that any 13.x needs
/// only a 13.0 driver — the minor-version compatibility that maps every 13.x to the 580
/// branch there, so the two ways of judging the driver agree. `None` if `cuda` isn't a version.
fn driver_cuda_needed(cuda: &str) -> Option<Vec<u32>> {
    let v = parse_version(cuda)?;
    let major = v[0];
    Some(if major >= 13 { vec![major, 0] } else { vec![major, v.get(1).copied().unwrap_or(0)] })
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
/// (`Deserialize`: the health cache stores it — see `snapshot`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// What a failed check says is broken — which decides whether `up --check` replaces the pod.
/// A fresh pod lands on another machine but boots the **same image** and runs the **same
/// setup**: it cures a bad host, never a bad image. Replacing a pod whose venv can't `import
/// torch` (live: a Hetzner VM whose install had failed) only bills a second pod to fail the
/// same way, and counts a good machine as bad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureKind {
    /// The machine: no driver / `nvidia-smi`, a driver below the floor, CUDA unavailable, the
    /// wrong device count, a failing tensor op, broken peer copies or NCCL — and python dying
    /// before it could say anything (timed out, OOM-killed, crashed). Another host may well pass.
    Host,
    /// The software on it (image or setup), as the facts show it: no python, `import torch`
    /// failing (python said so), a CPU-only torch on a GPU pod, the check script itself not
    /// running. A new pod from the same image fails the same way.
    Software,
}

impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::Host => "host",
            FailureKind::Software => "software/setup",
        }
    }
}

/// The kind of failure a FAILed check named `name` reports ([`FailureKind`]), from the facts it
/// was judged by. `Software` only where those facts put it on the image/setup:
/// - `script`: the check script never started.
/// - `torch`: python said so itself — no python on PATH, or `import torch` raised. When python
///   never got to say (killed by its timeout or the OOM killer, a crash — SIGILL on a CPU
///   without the instructions torch was built for, a segfault — or its output cut off), the
///   machine is as likely as the image: `Host`, replaced like any bad host.
/// - `cuda`: torch imported but is a CPU-only build (`torch.version.cuda` is None) — on a GPU
///   pod every pod from that image fails the same way.
///
/// Everything else — the GPU/driver/CUDA checks, and any check this function doesn't know (a new
/// one keeps the old "a FAIL is a bad host" meaning) — is `Host`.
pub fn failure_kind_of(name: &str, facts: Option<&DeepFacts>) -> FailureKind {
    let software = match (name, facts) {
        ("script", _) => true,
        ("torch", Some(f)) => f.python.as_deref() == Some("missing") || f.torch.as_deref().is_some_and(|t| t != "ok"),
        ("cuda", Some(f)) => f.cpu_only_torch(),
        _ => false,
    };
    if software {
        FailureKind::Software
    } else {
        FailureKind::Host
    }
}

/// Whether the FAILed check `c` is definite evidence against the machine itself — what `up`
/// may remember for days (`snapshot::bad_host_record`). Narrower than [`failure_kind_of`]'s
/// `Host`, which only decides whether to replace the pod now: there "as likely the machine"
/// is reason enough, but a remembered false "bad" turns a good machine away for days, and a
/// regression of our own (image, check script, env) would ban every machine it touched.
/// Counted, from what was actually reported:
/// - `nvidia-smi` answered with an error, or listed no GPUs;
/// - `cuda`: torch imported, isn't CPU-only, and CUDA failed to initialise (torch's error,
///   e.g. `CUDA unknown error`) while nvidia-smi lists the GPU;
/// - `device_count`: torch's count disagrees with nvidia-smi's (or is 0), falls short of the
///   GPUs the provider rented the pod with, or it raised;
/// - `gpuN`: the tensor op reported an error or a wrong result (NaN matmul);
/// - `peer_copy`: a copy reported garbage (`mismatch`) or an error.
///
/// Never: anything that timed out (nvidia-smi's 10 s, python's 75 s, NCCL's), python dying
/// or being killed before it reported (a crash, the OOM killer — `torch`, and any GPU check
/// left `not reported`), a driver below the configured floor or one torch calls too old for
/// its CUDA (policy: placement filters by it), NCCL at all, and any check this doesn't know.
/// Pure.
pub fn condemns_machine(c: &Check, f: &DeepFacts) -> bool {
    let timed_out = |s: &str| {
        let s = s.to_ascii_lowercase();
        s.contains("timed out") || s.contains("timeout")
    };
    let error = |s: &str| !timed_out(s);
    if c.status != Status::Fail {
        return false;
    }
    let smi_lists_gpus = f.smi.as_deref() == Some("ok") && !f.gpus.is_empty();
    match c.name.as_str() {
        "nvidia-smi" => match f.smi.as_deref() {
            Some("ok") => f.gpus.is_empty() && f.smi_query.as_deref().is_none_or(error),
            Some("missing") | None => false,
            Some(e) => error(e),
        },
        "cuda" => {
            f.torch.as_deref() == Some("ok")
                && !f.cpu_only_torch()
                && f.cuda_available == Some(false)
                && smi_lists_gpus
                && f.cuda_error.as_deref().is_some_and(|e| error(e) && !e.to_ascii_lowercase().contains("too old"))
        }
        "device_count" => f.device_count.is_some() || f.device_count_error.as_deref().is_some_and(error),
        "peer_copy" => f.peer.values().any(|r| r == "mismatch" || (r.starts_with("error") && error(r))),
        name => match name.strip_prefix("gpu").and_then(|i| i.parse::<u32>().ok()) {
            Some(i) => f.tensor.get(&i).is_some_and(|r| r != "ok" && error(r)),
            None => false,
        },
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
            Some(false) if f.cpu_only_torch() => Check::new(
                "cuda",
                Fail,
                format!(
                    "torch {} is a CPU-only build (no CUDA in it) — the image/setup installed the wrong torch",
                    f.torch_version.as_deref().unwrap_or("?")
                ),
            ),
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
    out.push(vscode_check(f, p));
    out
}

/// Setup's VS Code warm-up, as found: the newest server, the expected extensions, the
/// default interpreter. **Informational only** — Pass when it's all there, else Skip with
/// what's missing; never Warn or Fail. A missing pre-install only means the participant's
/// first connect is slower, which is no reason to doubt (or `up --check`-replace) a pod.
fn vscode_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let Some(server) = f.vscode_server.as_deref() else {
        return Check::new("vscode", Skip, "not reported");
    };
    let server = Some(server).filter(|s| *s != "none");
    let python = f.vscode_python.as_deref().filter(|p| *p != "unset");
    if server.is_none() && f.vscode_extensions.is_empty() && python.is_none() {
        return Check::new(
            "vscode",
            Skip,
            "not pre-installed (the first Remote-SSH connect downloads the server + extensions)",
        );
    }
    let missing: Vec<&str> = p
        .vscode_extensions
        .iter()
        .map(String::as_str)
        .filter(|id| !f.vscode_extensions.contains_key(*id))
        .collect();
    let mut parts = vec![match server {
        Some(c) => {
            let older = f.vscode_servers.unwrap_or(1).saturating_sub(1);
            let more = if older > 0 { format!(" (+{older} other)") } else { String::new() };
            format!("server {}{more}", c.get(..7).unwrap_or(c))
        }
        None => "no server".into(),
    }];
    if !p.vscode_extensions.is_empty() {
        parts.push(if missing.is_empty() {
            format!("{} extensions", p.vscode_extensions.len())
        } else {
            format!("missing {}", missing.join(", "))
        });
    }
    parts.push(match python {
        Some(py) => format!("python {py}"),
        None => "no default interpreter".into(),
    });
    let status = if server.is_some() && missing.is_empty() { Pass } else { Skip };
    Check::new("vscode", status, parts.join("; "))
}

/// nvidia-smi as the machine's view of its GPUs. When it can't answer (missing, an error, a
/// timeout, no GPUs listed, its query failing) on a **MIG slice**, the GPUs may still be fine:
/// in a slice's container NVML is often blocked (`Insufficient Permissions`, `No devices were
/// found`) while CUDA works. There, torch's proof that the GPUs work
/// ([`DeepFacts::torch_proves_gpus`]) is a PASS. Anywhere else it FAILs whatever torch says,
/// as before the fallback: on whole cards the same symptoms are a GPU fallen off the bus
/// (`--query-gpu`'s `Unable to determine the device handle`), and torch, which enumerates only
/// the GPUs that survived, proves nothing about the one that didn't (review of `27adfe4..db8dff8`,
/// #1: an 8-GPU pod with one lost read as a WARN — exit 0, not replaced, not remembered). A
/// slice is not enough either: every GPU of the pod must be one ([`DeepFacts::only_slices`]),
/// else a mixed pod's lost whole card hides behind its surviving slice.
fn smi_check(f: &DeepFacts) -> Check {
    use Status::*;
    // (the FAIL detail; what the fallback's detail says went wrong, short)
    let error = |e: &str| (reason(e), clip(&reason(e), 60));
    let same = |s: &str| (s.to_string(), s.to_string());
    let (fail, short) = match f.smi.as_deref() {
        Some("ok") if f.gpus.is_empty() => match f.smi_query.as_deref() {
            Some(e) => error(e),
            None => same("lists no GPUs"),
        },
        Some("ok") => {
            // Under MIG, say what the slice is cut from (the parent nvidia-smi names).
            let parent = f.gpus.first().and_then(|g| g.name.as_deref()).map(normalize_gpu_name);
            let detail = match (f.is_mig(), parent) {
                (true, Some(parent)) => format!("{} (MIG slice of {parent})", f.gpu_label()),
                _ => f.gpu_label(),
            };
            return Check::new("nvidia-smi", Pass, detail);
        }
        Some("missing") => ("not found (no NVIDIA driver in the container)".into(), "not found".into()),
        Some(e) => error(e),
        None => same("not reported"),
    };
    if !(f.only_slices() && f.torch_proves_gpus()) {
        return Check::new("nvidia-smi", Fail, fail);
    }
    let (label, mem) = (f.gpu_label(), f.torch_mem().map(|m| format!(", {m}")).unwrap_or_default());
    Check::new("nvidia-smi", Pass, format!("nvidia-smi can't read this MIG slice ({short}) — torch: {label}{mem}"))
}

/// The driver against the floor: nvidia-smi's version, else `/proc/driver/nvidia/version`'s
/// (said so), else [`driver_check_by_cuda`].
fn driver_check(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let (driver, from) = match (f.smi_driver(), f.driver_proc.as_deref()) {
        (Some(d), _) => (d, ""),
        (None, Some(d)) => (d, "; from /proc/driver/nvidia/version"),
        (None, None) => return driver_check_by_cuda(f, p),
    };
    let Some(floor) = &p.min_driver else {
        return Check::new("driver", Skip, format!("{driver} (no minimum configured{from})"));
    };
    let want = dotted(&floor.version);
    let have = parse_version(driver).unwrap_or_default();
    if cmp_versions(&have, &floor.version).is_lt() {
        Check::new("driver", Fail, format!("{driver} < {want} ({} needs ≥ {want}{from})", floor.why))
    } else {
        Check::new("driver", Pass, format!("{driver} ≥ {want} ({}{from})", floor.why))
    }
}

/// No driver version anywhere: judge the CUDA version torch says the driver supports against
/// the CUDA floor the minimum was derived from ([`driver_cuda_needed`]). An explicit
/// `MIN_DRIVER_VERSION` can't be judged that way (a CUDA version only bounds the driver
/// branch), nor can anything without torch's reading: today's WARN where nvidia-smi answered
/// or torch stood in for it (a MIG slice, [`smi_check`]), else a Skip — the nvidia-smi check
/// FAILs anyway.
fn driver_check_by_cuda(f: &DeepFacts, p: &HealthPolicy) -> Check {
    use Status::*;
    let smi_ok = f.smi.as_deref() == Some("ok");
    let via = if smi_ok { "via torch; nvidia-smi gave no driver version" } else { "via torch; nvidia-smi unavailable" };
    if let Some(have) = f.torch_driver_cuda.as_deref() {
        let Some(floor) = &p.min_driver else {
            return Check::new("driver", Skip, format!("driver supports CUDA {have} (no minimum configured; {via})"));
        };
        if let (Some(need), Some(got)) = (floor.cuda.as_deref().and_then(driver_cuda_needed), parse_version(have)) {
            let need_s = dotted(&need);
            return if cmp_versions(&got, &need).is_lt() {
                Check::new("driver", Fail, format!("driver supports CUDA {have} < {need_s} (needed for {}; {via})", floor.why))
            } else {
                Check::new("driver", Pass, format!("driver supports CUDA {have} ≥ {need_s} ({via})"))
            };
        }
    }
    if smi_ok || (f.only_slices() && f.torch_proves_gpus()) {
        let torch = f
            .torch_driver_cuda
            .as_deref()
            .map(|c| format!(" (torch: the driver supports CUDA {c}, which can't be held to MIN_DRIVER_VERSION)"))
            .unwrap_or_default();
        Check::new("driver", Warn, format!("version not reported{torch}"))
    } else {
        Check::new("driver", Skip, "needs nvidia-smi")
    }
}

/// `[580, 65, 6]` → `580.65.6`.
fn dotted(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

/// What a MIG undercount (torch seeing fewer slices than the pod has) says: why it may be
/// fine, and how to find out.
const MIG_UNDERCOUNT_HINT: &str = "CUDA may show a process one slice; check each with CUDA_VISIBLE_DEVICES=<MIG uuid>";

/// The `device_count` check `c` (torch against nvidia-smi) with torch's count also held to the
/// provider's ([`DeepFacts::provider_gpus`]): a GPU gone from nvidia-smi's view too — fallen
/// off the bus, nvidia-smi listing only the survivors or unable to count — is still one fewer
/// than the pod was rented with (review of `27adfe4..db8dff8`, #1). Fewer FAILs, unless every
/// GPU of the pod is shown to be a slice ([`DeepFacts::only_slices`]: else the missing one may be
/// a whole card). There it is `-L`'s undercount, already a WARN — `only_slices` needs `-L` to list
/// as many slices as the provider rented — and stays as it is. As many or more says nothing (an
/// API count of 0 is [`PodHealth::checked`]'s `provider` WARN). It only ever makes `c` worse:
/// never a FAIL into a PASS, and a FAIL keeps nvidia-smi's detail.
fn held_to_provider(f: &DeepFacts, c: Check) -> Check {
    use Status::*;
    let (Some(t), Some(p)) = (f.device_count, f.provider_gpus) else { return c };
    if t >= p || f.only_slices() || c.status == Fail {
        return c;
    }
    Check::new(c.name, Fail, format!("torch sees {t} GPU(s), the provider reports {p}"))
}

/// Device count, per-GPU tensor op, peer copies and NCCL — only once CUDA is available.
fn gpu_checks(f: &DeepFacts, out: &mut Vec<Check>) {
    use Status::*;
    let n = f.device_count;
    // Under MIG torch sees the slices, and `--query-gpu` lists only their parent card(s): the
    // count to match is the slices `nvidia-smi -L` lists — unless whole cards sit beside them.
    let slices = f.mig_devices.filter(|&m| m > 0 && !f.mixed_inventory());
    let smi = slices.map(|m| (m, format!("{m} MIG slice(s)"))).or_else(|| f.smi_count().map(|s| (s, s.to_string())));
    let count = match (n, smi) {
        (None, _) => Check::new(
            "device_count",
            Fail,
            f.device_count_error.as_deref().map(reason).unwrap_or_else(|| f.python_cut()),
        ),
        (Some(0), _) => Check::new("device_count", Fail, "torch sees no GPU"),
        // CUDA may show a process one MIG slice (NVIDIA's MIG guide: a single process enumerates
        // a single MIG device, on older drivers), so a pod given two slices can have torch see 1
        // — or the second slice is inaccessible or broken, and nothing here tells which. Not a
        // PASS (review of `27adfe4..db8dff8`, codex #2): a WARN, saying how to check each slice.
        (Some(t), Some((s, _))) if slices.is_some() && t < s => {
            Check::new("device_count", Warn, format!("torch sees {t} of the {s} MIG slices — {MIG_UNDERCOUNT_HINT}"))
        }
        (Some(t), Some((s, said))) if t != s => {
            Check::new("device_count", Fail, format!("torch sees {t} GPU(s), nvidia-smi {said}"))
        }
        (Some(t), Some((_, said))) if slices.is_some() => {
            Check::new("device_count", Pass, format!("{t} (matches nvidia-smi's {said})"))
        }
        (Some(t), Some(_)) => Check::new("device_count", Pass, format!("{t} (matches nvidia-smi)")),
        (Some(t), None) if f.is_mig() => {
            Check::new("device_count", Pass, format!("{t} (MIG slice(s); nvidia-smi count unknown)"))
        }
        (Some(t), None) => Check::new("device_count", Pass, format!("{t} (nvidia-smi count unknown)")),
    };
    out.push(held_to_provider(f, count));

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
    /// The pod's machine, for the same-host summary only (see [`host_key`]). Not
    /// serialized: the JSON is per-pod health, and IPs are already in `pods list --json`.
    #[serde(skip)]
    pub host: Option<String>,
}

/// The provider's name for `pod`'s GPU when it is a MIG slice (`B300 MIG 34GB`, `NVIDIA B300
/// SXM6 AC MIG 1g.34gb` → `B300 SXM6 AC MIG 1g.34gb`) — the word `MIG` in its GPU type —
/// else `None`. What labels show instead of the parent card nvidia-smi names. Pure.
pub fn mig_slice_name(pod: &Pod) -> Option<String> {
    let ty = pod.gpu_type.as_deref()?.trim();
    names_mig(ty).then(|| normalize_gpu_name(ty))
}

/// Whether a GPU name names a MIG slice: the word `MIG` in it (`B300 MIG 34GB`, torch's
/// `NVIDIA A100-SXM4-40GB MIG 1g.5gb`) — a word, not a substring (`Migrator`).
fn names_mig(name: &str) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("mig"))
}

/// What names the machine a pod runs on, for the same-host summary and `up --check`'s
/// rejection of a replacement that lands on a machine that already failed — `None` when
/// nothing does. A false "same host" is the costly mistake (it sends the operator to switch
/// GPU type for nothing, and `up` terminates a good pod), so only a real machine identity
/// counts:
/// - The provider's own machine id ([`Pod::machine_id`]), where it reports one — as
///   `vast machine 31540`, so ids never collide across providers or with an IP.
/// - Vast without one: nothing. Its IPs don't name a machine — a host often runs several
///   machines behind one public IP, each with its own direct port range — and an endpoint
///   on Vast's SSH proxy says nothing about the machine at all.
/// - Elsewhere, the SSH IP where it is the machine's own address — an IP literal (RunPod's
///   public IP). A hostname never groups: that's what a shared proxy looks like.
pub fn host_key(pod: &Pod) -> Option<String> {
    if let Some(key) = machine_key(pod) {
        return Some(key);
    }
    if pod.provider.eq_ignore_ascii_case("vast") {
        return None;
    }
    pod.ssh_ip.as_deref().filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()).map(String::from)
}

/// The provider's own name for `pod`'s machine (`runpod machine ue6mmk7dp8qi`) — [`host_key`]
/// without its IP fallback; `None` when the provider reports no [`Pod::machine_id`]. The only
/// identity a bad machine is remembered by across runs (`snapshot::bad_host_record`): an IP
/// is shared by several machines (Vast; RunPod — finding #23) or handed to another VM once a
/// Hetzner one is deleted — a guess one run can afford, but not one that lasts for days.
pub fn machine_key(pod: &Pod) -> Option<String> {
    let id = pod.machine_id.as_deref().map(str::trim).filter(|id| !id.is_empty())?;
    Some(format!("{} machine {id}", pod.provider.to_ascii_lowercase()))
}

impl PodHealth {
    /// A pod whose script ran: its facts judged by `policy` (GPU or CPU rules per
    /// [`expects_gpu`]), with the pod's maintenance window, its MIG slice name and its GPU
    /// count as the provider reports them.
    pub fn checked(pod: &Pod, mut facts: DeepFacts, policy: &HealthPolicy) -> Self {
        if facts.slice.is_none() {
            facts.slice = mig_slice_name(pod);
        }
        if facts.provider_gpus.is_none() {
            facts.provider_gpus = pod.gpu_count;
        }
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
            host: host_key(pod),
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
            host: host_key(pod),
        }
    }

    /// What its failures say is broken: `Host` as soon as one failed check is about the
    /// machine, `Software` when every one is about the image/setup ([`failure_kind_of`]),
    /// `None` when nothing failed. A pod that couldn't be checked at all (`ssh`) is `Host` here
    /// — `up --check` never judges such a pod by its checks (it re-runs the check instead).
    pub fn failure_kind(&self) -> Option<FailureKind> {
        let facts = self.facts.as_ref();
        let mut kinds =
            self.checks.iter().filter(|c| c.status == Status::Fail).map(|c| failure_kind_of(&c.name, facts)).peekable();
        kinds.peek()?;
        Some(if kinds.any(|k| k == FailureKind::Host) { FailureKind::Host } else { FailureKind::Software })
    }

    /// The NOTES column: failures first, then warnings, as `check: detail`.
    pub fn notes(&self) -> String {
        let pick = |s: Status| self.checks.iter().filter(move |c| c.status == s);
        let notes: Vec<String> =
            pick(Status::Fail).chain(pick(Status::Warn)).map(|c| format!("{}: {}", c.name, c.detail)).collect();
        clip(&notes.join("; "), 110)
    }
}

/// One pod's deep-check call → its verdict: the script's facts judged by `policy`, or a
/// FAIL saying why there are none (no answer, ssh itself failed, the script never
/// started). `call` is the [`crate::remote::Remote::exec`] outcome with a failed call
/// already described (`remote::describe_error`, e.g. `timed out after 150s`). Shared by
/// `pods test --deep`, `up --check` and the dashboard, so a pod is judged the same way by
/// all three.
pub fn judge_deep_call(pod: &Pod, call: std::result::Result<SshOutput, String>, policy: &HealthPolicy) -> PodHealth {
    match call {
        Err(why) => PodHealth::unreachable(pod, why),
        Ok(out) => {
            let facts = parse_deep(&out.stdout);
            let stderr = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            if !facts.started && crate::ssh::login_shell_missing(out.code, &out.stderr) {
                // No zsh to run it in: a bare VM before setup (live: Hetzner `up --check
                // --no-setup`). SSH worked and the script never started — the pod's software,
                // like a missing `base64` below: a `script` FAIL (software/setup), with the fix.
                let mut health = PodHealth::checked(pod, facts, policy);
                for c in health.checks.iter_mut().filter(|c| c.name == "script") {
                    c.detail = crate::ssh::login_shell_missing_hint(&pod.name);
                }
                health
            } else if !facts.started && !out.success {
                // ssh itself failed (255 = refused/auth/…): say that, not "no output".
                PodHealth::unreachable(pod, format!("{}: {stderr}", out.exit_label()))
            } else {
                let started = facts.started;
                let mut health = PodHealth::checked(pod, facts, policy);
                // The script never started (e.g. no `base64` on the pod): stderr says why.
                if !started && !stderr.is_empty() {
                    for c in health.checks.iter_mut().filter(|c| c.name == "script") {
                        c.detail = format!("{} ({stderr})", c.detail);
                    }
                }
                health
            }
        }
    }
}

/// What marks a CUDA version read by torch rather than nvidia-smi, and the footnote saying so.
const TORCH_CUDA_MARK: &str = "*";
const TORCH_CUDA_NOTE: &str = "* from torch: nvidia-smi unavailable";

/// `NAME  RESULT  GPUS  DRIVER  CUDA  NET  NOTES`, one row per pod in the given order. Where
/// nvidia-smi can't answer, the columns are torch's and `/proc`'s facts ([`DeepFacts::gpu_label`],
/// [`DeepFacts::driver`]); a CUDA version torch read is marked, with a footnote under the table.
pub fn render_health_table(results: &[PodHealth]) -> String {
    let mut marked = false;
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|h| {
            let f = h.facts.as_ref();
            let or_dash = |s: Option<String>| s.unwrap_or_else(|| "-".into());
            let cuda = f.and_then(DeepFacts::driver_cuda).map(|(c, torch)| {
                marked |= torch;
                if torch {
                    format!("{c}{TORCH_CUDA_MARK}")
                } else {
                    c.to_string()
                }
            });
            vec![
                h.name.clone(),
                h.status.label().to_string(),
                f.map(DeepFacts::gpu_label).unwrap_or_else(|| "-".into()),
                or_dash(f.and_then(|f| f.driver().map(String::from))),
                or_dash(cuda),
                or_dash(f.and_then(DeepFacts::hf_mbps).map(|m| format!("{m:.1} MB/s"))),
                h.notes(),
            ]
        })
        .collect();
    let mut out = table::render(
        &["NAME", "RESULT", "GPUS", "DRIVER", "CUDA", "NET", "NOTES"],
        &[Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Left],
        &rows,
    );
    if marked {
        out.push_str(TORCH_CUDA_NOTE);
        out.push('\n');
    }
    out
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
    // Only a failure about the machine says anything about the host: two pods of one image
    // failing `import torch` on a shared IP is the image, not that host.
    for h in results.iter().filter(|h| h.status == Status::Fail && h.failure_kind() == Some(FailureKind::Host)) {
        if let Some(ip) = h.host.as_deref().filter(|ip| !ip.is_empty()) {
            by_ip.entry(ip).or_default().push(h.name.clone());
        }
    }
    by_ip.into_iter().filter(|(_, names)| names.len() >= 2).map(|(ip, names)| (ip.to_string(), names)).collect()
}

/// The lines under the table: the tally, one `same host?` line per shared-IP group of host
/// failures, then the pods whose failures are all software/setup ([`FailureKind`]).
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
    let software: Vec<&str> = results
        .iter()
        .filter(|h| h.failure_kind() == Some(FailureKind::Software))
        .map(|h| h.name.as_str())
        .collect();
    if !software.is_empty() {
        lines.push(format!(
            "software/setup failure, not the host: {} — a new pod from the same image fails the same way \
             (re-run `arena pods setup`, or fix the image), so replacing it won't help.",
            software.join(", ")
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

    /// Finding #24 (live B300 MIG pod, 2026-10-08): nvidia-smi names the parent card (`NVIDIA
    /// B300 SXM6 AC`) and the summary/deep check said `1×B300 SXM6 AC` — not what the pod has.
    /// Labels name the slice: the provider's name for it, else parent + `-L`'s profile.
    #[test]
    fn a_mig_slice_is_labelled_as_the_slice() {
        let mig = edit(
            &one_gpu(),
            &[("gpu.0.name", Some("NVIDIA B300 SXM6 AC")), ("mig_devices", Some("1")), ("mig_profile", Some("1g.34gb"))],
        );
        let f = parse_deep(&mig);
        assert_eq!((f.mig_devices, f.mig_profile.as_deref()), (Some(1), Some("1g.34gb")));
        assert_eq!(f.gpu_label(), "1×B300 SXM6 AC MIG 1g.34gb", "no provider name: parent + profile");
        // With the provider's name for the slice (v1's display name, or v2's id).
        for (gpu_type, want) in [("B300 MIG 34GB", "1×B300 MIG 34GB"), ("NVIDIA B300 SXM6 AC MIG 1g.34gb", "1×B300 SXM6 AC MIG 1g.34gb")] {
            let p = Pod { gpu_type: Some(gpu_type.into()), gpu_count: Some(1), ..pod("delta", "1.2.3.4") };
            let h = PodHealth::checked(&p, parse_deep(&mig), &cuda13());
            assert_eq!(h.facts.as_ref().unwrap().gpu_label(), want);
            assert_eq!(check(&h.checks, "nvidia-smi").detail, format!("{want} (MIG slice of B300 SXM6 AC)"));
            assert!(render_health_table(std::slice::from_ref(&h)).contains(want), "GPUS column");
            assert_eq!(h.status, Status::Pass);
        }
        // Not MIG: unchanged.
        let a4000 = Pod { gpu_type: Some("RTX A4000".into()), ..pod("apple", "1.2.3.4") };
        assert_eq!(mig_slice_name(&a4000), None);
        assert_eq!(PodHealth::checked(&a4000, parse_deep(&one_gpu()), &cuda13()).facts.unwrap().gpu_label(), "1×RTX A4000");
        assert_eq!(mig_slice_name(&Pod { gpu_type: Some("PRO 6000 MIG 24GB".into()), ..Default::default() }).as_deref(), Some("PRO 6000 MIG 24GB"));
        assert_eq!(mig_slice_name(&Pod { gpu_type: Some("NVIDIA Migrator X".into()), ..Default::default() }), None, "a word, not a substring");
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
        let v = |d: u32, why: &str| {
            Some(DriverFloor { version: vec![d], why: why.into(), cuda: why.strip_prefix("CUDA ").map(String::from) })
        };
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
                cuda: None,
            })),
            ("MIN_DRIVER_VERSION=none\nALLOWED_CUDA_VERSIONS=13.0", None),
            ("MIN_DRIVER_VERSION=\nALLOWED_CUDA_VERSIONS=13.0", v(580, "CUDA 13.0")),
            // Finding #10: the floor keys follow the create's floor — MIN_CUDA_VERSION wins,
            // an exact set's lowest version counts.
            ("MIN_CUDA_VERSION=12.8\nALLOWED_CUDA_VERSIONS=13.0", v(570, "CUDA 12.8")),
            ("MIN_CUDA_VERSION=13.0", v(580, "CUDA 13.0")),
            ("EXACT_CUDA_VERSIONS=\"13.1, 13.2\"", v(580, "CUDA 13.1")),
            ("EXACT_CUDA_VERSIONS=12.8,13.0\nMIN_CUDA_VERSION=12.4", v(570, "CUDA 12.8")),
        ];
        for (text, want) in cases {
            assert_eq!(&floor(text), want, "{text:?}");
        }
        let err = HealthPolicy::from_config(&cfg("MIN_DRIVER_VERSION=latest")).unwrap_err();
        assert!(err.to_string().contains("MIN_DRIVER_VERSION=\"latest\""), "{err}");
        // Review fix: a CUDA value that isn't a version fails closed, as every create refuses
        // it — it used to leave the check with no driver floor at all (`"13.0, x"` gave none,
        // where before finding #10's floor it gave 580).
        for text in ["ALLOWED_CUDA_VERSIONS=\"13.0, x\"", "MIN_CUDA_VERSION=newest", "EXACT_CUDA_VERSIONS=13.0,13.x"] {
            let err = HealthPolicy::from_config(&cfg(text)).unwrap_err().to_string();
            assert!(err.contains("isn't a CUDA version like 13.0") && err.contains("MIN_DRIVER_VERSION"), "{text}: {err}");
        }
        // An explicit driver floor doesn't need one.
        assert_eq!(floor("MIN_DRIVER_VERSION=580\nALLOWED_CUDA_VERSIONS=x"), Some(DriverFloor { version: vec![580], why: "MIN_DRIVER_VERSION".into(), cuda: None }));
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
                "network", "disk", "load", "maintenance", "vscode"
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
        assert_eq!(check(&checks, "vscode"), &Check::new("vscode", Status::Skip, "not reported"));

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
        // python gone before `import torch` could report (no torch line at all): killed by its
        // 75s timeout (124) or the OOM killer (137), SIGILL (132), a segfault (139), cut off.
        let py_died = |code: &str, stderr: &str| {
            edit(
                &without(HEALTHY_2GPU, &["torch", "cuda_", "device_count", "tensor.", "peer.", "nccl", "py_done"]),
                &[("py_exit", Some(code)), ("py_stderr", Some(stderr))],
            )
        };
        let (py_timeout_early, py_oom, py_sigill, py_segv, py_cut) = (
            py_died("124", ""),
            py_died("137", ""),
            py_died("132", "Illegal instruction (core dumped)"),
            py_died("139", "Segmentation fault (core dumped)"),
            py_died("0", ""),
        );
        // A CPU-only torch on a GPU pod (the wrong wheel in the image/setup): with or without the
        // error `torch.cuda.init()` raises.
        let cpu_torch = edit(
            &without(HEALTHY_2GPU, &["device_count", "tensor.", "peer.", "nccl"]),
            &[
                ("torch_version", Some("2.9.0+cpu")),
                ("torch_cuda", Some("none")),
                ("cuda_available", Some("false")),
                ("cuda_error", Some("error: AssertionError: Torch not compiled with CUDA enabled")),
            ],
        );
        let cpu_torch_quiet = edit(&cpu_torch, &[("cuda_error", None)]);
        let smi_missing = edit(
            &without(HEALTHY_2GPU, &["smi_", "gpu.", "tensor.", "peer.", "nccl"]),
            &[
                ("smi", Some("missing")),
                ("cuda_available", Some("false")),
                ("cuda_error", Some("error: RuntimeError: Found no NVIDIA driver on your system.")),
                ("device_count", Some("0")),
            ],
        );
        // torch working on every GPU it sees changes neither: off a MIG slice there is no
        // fallback on torch's proof (`a_gpu_fallen_off_the_bus_fails_whatever_torch_says`).
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
            ("python timed out before torch", &py_timeout_early, "torch", Fail, "python checks timed out"),
            ("python SIGILL", &py_sigill, "torch", Fail, "python exited 132: Illegal instruction"),
            ("python output cut", &py_cut, "torch", Fail, "python output cut off"),
            ("cpu-only torch", &cpu_torch, "cuda", Fail, "torch 2.9.0+cpu is a CPU-only build"),
            ("cpu-only torch, no error", &cpu_torch_quiet, "cuda", Fail, "CPU-only build"),
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

        // What each failure says is broken: `up --check` replaces a pod only for its machine — a
        // new pod from the same image can't fix a missing torch (live: a Hetzner VM whose
        // install failed was recreated, and failed the same way).
        use FailureKind::{Host, Software};
        let kinds: &[(&str, &str, &str, FailureKind)] = &[
            ("cuInit 999", &cuinit_999, "runpod", Host),
            ("unknown error on gpu1", &unknown_error, "runpod", Host),
            ("insufficient driver", &insufficient, "runpod", Host),
            ("peer copy wrong", &peer_mismatch, "runpod", Host),
            ("old driver", &old_driver, "runpod", Host),
            ("nccl timeout", &nccl_timeout, "runpod", Host),
            ("nccl error", &nccl_error, "runpod", Host),
            ("nvidia-smi missing", &smi_missing, "runpod", Host),
            ("nvidia-smi error", &smi_error, "runpod", Host),
            ("nvidia-smi query error", &smi_query_error, "vast", Host),
            ("device_count mismatch", &count_mismatch, "runpod", Host),
            ("python timed out mid-GPU", &py_timeout, "runpod", Host),
            ("torch missing", &torch_missing, "runpod", Software),
            ("torch missing on a CPU VM", &torch_missing, "hetzner", Software),
            ("no python", &no_python, "runpod", Software),
            ("no python on a CPU VM", &no_python, "hetzner", Software),
            // python never said: the machine as likely as the image — replaced, as before.
            ("python timed out before torch", &py_timeout_early, "runpod", Host),
            ("python OOM-killed", &py_oom, "runpod", Host),
            ("python SIGILL (old CPU)", &py_sigill, "runpod", Host),
            ("python segfault", &py_segv, "runpod", Host),
            ("python output cut off", &py_cut, "runpod", Host),
            ("python timed out on a CPU VM", &py_timeout_early, "hetzner", Host),
            // A CPU-only torch on a GPU pod: the image's, not the host's.
            ("cpu-only torch", &cpu_torch, "runpod", Software),
            ("cpu-only torch, no error", &cpu_torch_quiet, "vast", Software),
        ];
        for (label, out, provider, kind) in kinds {
            let p = Pod { provider: provider.to_string(), ..pod("apple", "1.2.3.4") };
            let h = PodHealth::checked(&p, parse_deep(out), &cuda13());
            assert_eq!(h.failure_kind(), Some(*kind), "{label}: {:?}", h.checks);
        }
        // A host failure next to a software one is still a host failure (it may be the cause).
        let both = edit(&smi_missing, &[("torch", Some("error: ImportError: libcuda.so.1: cannot open shared object file"))]);
        assert_eq!(PodHealth::checked(&pod("apple", "1.2.3.4"), parse_deep(&both), &cuda13()).failure_kind(), Some(Host));
        assert_eq!(PodHealth::checked(&pod("apple", "1.2.3.4"), parse_deep(HEALTHY_2GPU), &cuda13()).failure_kind(), None);
        assert_eq!(PodHealth::unreachable(&pod("apple", "1.2.3.4"), "timed out").failure_kind(), Some(Host));
        // Unknown check names keep the old meaning of a FAIL (the machine).
        assert_eq!(failure_kind_of("some_future_check", Some(&parse_deep(&torch_missing))), Host);
    }

    #[test]
    fn software_failures_are_summarised_apart_and_never_group_a_host() {
        let torch_missing = edit(
            &without(HEALTHY_2GPU, &["torch_", "cuda_", "device_count", "tensor.", "peer.", "nccl"]),
            &[("torch", Some("error: ModuleNotFoundError: No module named 'torch'"))],
        );
        let sick = |name: &str| PodHealth::checked(&pod(name, "1.2.3.4"), parse_deep(&torch_missing), &cuda13());
        // Two pods of one image failing `import torch` on one IP: the image, not that host.
        let results = vec![sick("apple"), sick("bloom")];
        assert!(same_host_failures(&results).is_empty());
        let lines = render_summary(&results);
        assert_eq!(lines[0], "0 pass, 0 warn, 2 fail");
        assert!(
            lines[1].starts_with("software/setup failure, not the host: devtest-apple, devtest-bloom")
                && lines[1].contains("replacing it won't help"),
            "{lines:?}"
        );
        // Host failures on a shared IP still group (and aren't called software).
        let results = vec![
            PodHealth::unreachable(&pod("apple", "1.2.3.4"), "x"),
            PodHealth::unreachable(&pod("bloom", "1.2.3.4"), "x"),
        ];
        assert_eq!(render_summary(&results).len(), 2);
        assert!(render_summary(&results)[1].starts_with("same host? 1.2.3.4"));
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
    fn vscode_line_is_informational_only() {
        use Status::*;
        const C: &str = "2a59476c9bfcb90b3ddc372c36762471b7dfad1c";
        let all = [
            ("vscode.ext.ms-python.python", Some("2026.1.0")),
            ("vscode.ext.ms-python.vscode-pylance", Some("2026.1.1")),
            ("vscode.ext.ms-toolsai.jupyter", Some("2026.2.0")),
        ];
        let with = |extra: &[(&str, Option<&str>)]| edit(HEALTHY_2GPU, extra);
        let full: Vec<(&str, Option<&str>)> =
            [("vscode.server", Some(C)), ("vscode.servers", Some("2")), ("vscode.python", Some("/opt/arena-env/bin/python"))]
                .into_iter()
                .chain(all)
                .collect();
        let no_jupyter: Vec<(&str, Option<&str>)> = full.iter().copied().filter(|(k, _)| !k.ends_with("jupyter")).collect();
        let off = HealthPolicy { vscode_extensions: vec![], ..cuda13() };
        // (label, script output, policy, status, detail)
        let cases: Vec<(&str, String, HealthPolicy, Status, &str)> = vec![
            ("all there", with(&full), cuda13(), Pass, "server 2a59476 (+1 other); 3 extensions; python /opt/arena-env/bin/python"),
            ("one missing", with(&no_jupyter), cuda13(), Skip, "server 2a59476 (+1 other); missing ms-toolsai.jupyter; python"),
            (
                "nothing installed",
                with(&[("vscode.server", Some("none")), ("vscode.servers", Some("0")), ("vscode.python", Some("unset"))]),
                cuda13(),
                Skip,
                "not pre-installed (the first Remote-SSH connect downloads",
            ),
            (
                "extensions only (e.g. the server download failed)",
                with(&[("vscode.server", Some("none")), all[0], all[1], all[2], ("vscode.python", Some("unset"))]),
                cuda13(),
                Skip,
                "no server; 3 extensions; no default interpreter",
            ),
            ("pre-install off", with(&[("vscode.server", Some(C)), ("vscode.python", Some("unset"))]), off, Pass, "server 2a59476; no default interpreter"),
            ("older script", HEALTHY_2GPU.to_string(), cuda13(), Skip, "not reported"),
        ];
        for (label, out, policy, status, detail) in cases {
            let checks = evaluate(&parse_deep(&out), &policy, None);
            let c = check(&checks, "vscode");
            assert_eq!(c.status, status, "{label}: {c:?}");
            assert!(c.detail.contains(detail), "{label}: {:?} lacks {detail:?}", c.detail);
            // Never moves the pod's verdict.
            assert_eq!(overall(&checks), Pass, "{label}");
        }
        // The policy's expectations come from the same config as setup's warm-up.
        assert_eq!(HealthPolicy::default().vscode_extensions, crate::vscode::DEFAULT_EXTENSIONS);
        let cfg = |s: &str| Config::parse(s);
        assert!(HealthPolicy::from_config(&cfg("VSCODE_PREINSTALL=0\nMIN_DRIVER_VERSION=580")).unwrap().vscode_extensions.is_empty());
        assert_eq!(HealthPolicy::from_config(&cfg("VSCODE_EXTENSIONS=a.b")).unwrap().vscode_extensions, ["a.b"]);
        assert!(HealthPolicy::from_config(&cfg("VSCODE_EXTENSIONS=a")).is_err());
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

    /// Every way a deep-check call can end, judged once for all three callers (`pods test
    /// --deep`, `up --check`, the dashboard): facts → their verdict; no answer, or ssh
    /// itself failing → a FAIL saying so; a script that never started → why, from stderr.
    #[test]
    fn judge_deep_call_table() {
        let p = pod("apple", "1.1.1.1");
        let out = |success: bool, code: i32, stdout: &str, stderr: &str| SshOutput {
            success,
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        // (call, verdict, the check that says why, its detail contains)
        let cases: Vec<(std::result::Result<SshOutput, String>, Status, &str, &str)> = vec![
            (Ok(out(true, 0, HEALTHY_2GPU, "")), Status::Pass, "cuda", ""),
            (Err("timed out after 150s".into()), Status::Fail, "ssh", "timed out after 150s"),
            (Ok(out(false, 255, "", "\nssh: connect to host 1.1.1.1 port 22: Connection refused\n")), Status::Fail, "ssh", "exit 255: ssh: connect to host"),
            (Ok(out(true, 0, "", "bash: base64: command not found")), Status::Fail, "script", "(bash: base64: command not found)"),
            // Live BPF F1: a bare VM before setup has no zsh to run the check in.
            (Ok(out(false, 127, "", "bash: line 1: zsh: command not found")), Status::Fail, "script", "`arena pods setup devtest-apple`"),
        ];
        for (call, want, name, detail) in cases {
            let what = format!("{call:?}");
            let h = judge_deep_call(&p, call, &cuda13());
            assert_eq!(h.status, want, "{what}: {:#?}", h.checks);
            assert!(check(&h.checks, name).detail.contains(detail), "{what}: {:#?}", h.checks);
            assert_eq!((h.name.as_str(), h.provider.as_str()), ("devtest-apple", "runpod"));
        }
    }

    /// Live BPF F1: `up --check --no-setup` on a bare Hetzner VM ended "couldn't run the deep
    /// check twice (ssh: exit 127: … zsh: command not found)". No zsh is the pod's software
    /// (setup installs it), never its machine: one `script` FAIL, software/setup, saying the
    /// fix — and SSH having worked, the facts are there (unstarted), unlike an unreachable pod.
    #[test]
    fn no_login_shell_is_a_software_failure_with_the_setup_hint() {
        let vm = Pod { provider: "hetzner".into(), ..pod("hz2", "5.6.7.8") };
        let call = Ok(SshOutput {
            success: false,
            code: Some(127),
            stdout: String::new(),
            stderr: "bash: line 1: zsh: command not found\n".into(),
        });
        let h = judge_deep_call(&vm, call, &cuda13());
        assert_eq!(h.status, Status::Fail);
        assert_eq!(h.checks.len(), 1, "{:#?}", h.checks);
        assert_eq!(h.checks[0].name, "script");
        assert!(h.checks[0].detail.contains("no zsh") && h.checks[0].detail.contains("`arena pods setup devtest-hz2`"), "{:#?}", h.checks);
        assert_eq!(h.failure_kind(), Some(FailureKind::Software));
        assert!(h.facts.as_ref().is_some_and(|f| !f.started), "{h:#?}");
        assert!(h.notes().contains("`arena pods setup devtest-hz2`"), "the hint survives the NOTES clip: {}", h.notes());
        // Any other exit 127 (ssh itself, a missing tool after zsh started) is not this.
        let other = Ok(SshOutput { success: false, code: Some(255), stdout: String::new(), stderr: "zsh: command not found".into() });
        assert_eq!(judge_deep_call(&vm, other, &cuda13()).checks[0].name, "ssh");
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
            PodHealth::unreachable(&pod("echo", "4.4.4.4"), "exit 255: Connection refused"),
        ];
        assert_eq!(
            render_health_table(&results),
            "\
NAME           RESULT  GPUS         DRIVER     CUDA        NET  NOTES
devtest-apple  pass    2×RTX A4000  580.65.06  13.0  47.1 MB/s
devtest-bloom  warn    2×RTX A4000  580.65.06  13.0   0.4 MB/s  network: 0.4 MB/s from huggingface.co (< 2 MB/s)
devtest-cloud  fail    2×RTX A4000  580.65.06  13.0  47.1 MB/s  peer_copy: 1→0 mismatch
devtest-dune   fail    -            -          -             -  ssh: timed out after 150s
devtest-echo   fail    -            -          -             -  ssh: exit 255: Connection refused
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
        // Two Vast pods behind the same SSH proxy hostname are not "the same host".
        let results = vec![failing(&vast("apple", "ssh4.vast.ai")), failing(&vast("bloom", "ssh4.vast.ai"))];
        assert!(same_host_failures(&results).is_empty());
        assert_eq!(render_summary(&results), ["0 pass, 0 warn, 2 fail"]);
        // Nor two on one public IP: a Vast host often runs several machines behind it (each
        // with its own port range), and a proxy address can be an IP literal too.
        let results = vec![failing(&vast("apple", "203.0.113.9")), failing(&vast("bloom", "203.0.113.9"))];
        assert!(same_host_failures(&results).is_empty());
        // Vast's machine id is what names the machine: same id groups (whatever the IPs),
        // different ids behind one IP don't.
        let on = |p: Pod, m: &str| Pod { machine_id: Some(m.into()), ..p };
        let results = vec![
            failing(&on(vast("apple", "203.0.113.9"), "31540")),
            failing(&on(vast("bloom", "198.51.100.4"), "31540")),
            failing(&on(vast("cloud", "203.0.113.9"), "31541")),
        ];
        assert_eq!(
            same_host_failures(&results),
            [("vast machine 31540".to_string(), vec!["devtest-apple".to_string(), "devtest-bloom".to_string()])]
        );
        assert_eq!(render_summary(&results)[1].split(':').next(), Some("same host? vast machine 31540"));
        assert_eq!(host_key(&on(vast("apple", "ssh4.vast.ai"), "31540")).as_deref(), Some("vast machine 31540"));
        // A hostname (not an IP) never groups, on any provider.
        let results = vec![failing(&pod("apple", "proxy.example")), failing(&pod("bloom", "proxy.example"))];
        assert!(same_host_failures(&results).is_empty());
        // Real IPs do, IPv6 included.
        let results = vec![failing(&pod("apple", "2001:db8::7")), failing(&pod("bloom", "2001:db8::7"))];
        assert_eq!(
            same_host_failures(&results),
            [("2001:db8::7".to_string(), vec!["devtest-apple".to_string(), "devtest-bloom".to_string()])]
        );
        assert_eq!(host_key(&pod("apple", "1.2.3.4")).as_deref(), Some("1.2.3.4"));
        assert_eq!(host_key(&Pod { ssh_ip: None, ..pod("apple", "") }), None);
        assert_eq!(host_key(&Pod { machine_id: Some(" ".into()), ..pod("apple", "1.2.3.4") }).as_deref(), Some("1.2.3.4"));
        // `machine_key` is the machine-id half only: never an IP, on any provider.
        assert_eq!(machine_key(&on(pod("apple", "1.2.3.4"), "ue6mmk7dp8qi")).as_deref(), Some("runpod machine ue6mmk7dp8qi"));
        assert_eq!(machine_key(&on(vast("apple", "ssh4.vast.ai"), "31540")), host_key(&on(vast("apple", "ssh4.vast.ai"), "31540")));
        assert_eq!(machine_key(&pod("apple", "1.2.3.4")), None);
        assert_eq!(machine_key(&Pod { provider: "hetzner".into(), ..pod("apple", "1.2.3.4") }), None);
        assert_eq!(machine_key(&Pod { machine_id: Some(" ".into()), ..pod("apple", "1.2.3.4") }), None);
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

    // ---- MIG: nvidia-smi can't answer, torch can (lane MIG, 2026-10-20) ----

    /// A 12.4 host CUDA floor: driver ≥ 550.
    fn cuda124() -> HealthPolicy {
        HealthPolicy { min_driver: driver_floor_for_cuda(&["12.4"]), ..Default::default() }
    }

    /// One GPU whose nvidia-smi says `smi` (and lists nothing), torch naming it `torch_name` and
    /// proving it works, `/proc` giving the driver and torch the CUDA it supports.
    fn smi_down(smi: &str, torch_name: &str) -> String {
        edit(
            &without(&one_gpu(), &["smi_", "gpu."]),
            &[
                ("smi", Some(smi)),
                ("driver_proc", Some("570.86.15")),
                ("torch_driver_cuda", Some("12.8")),
                ("tgpu.0.name", Some(torch_name)),
                ("tgpu.0.mem_total_mib", Some("4864")),
                ("tgpu.0.mem_free_mib", Some("4600")),
            ],
        )
    }

    const A100_SLICE: &str = "NVIDIA A100-SXM4-40GB MIG 1g.5gb";

    #[test]
    fn parse_reads_torchs_gpus_and_the_proc_driver() {
        let out = edit(
            HEALTHY_2GPU,
            &[
                ("gpu.0.name", Some("[N/A]")),
                ("gpu.0.driver", Some("[Insufficient Permissions]")),
                ("gpu.1.mem_total_mib", Some("[N/A]")),
                ("driver_proc", Some("570.86.15")),
                ("torch_driver_cuda", Some("12.8")),
                ("tgpu.0.name", Some(A100_SLICE)),
                ("tgpu.0.mem_total_mib", Some("4864")),
                ("tgpu.0.mem_free_mib", Some("4600")),
                ("tgpu.1.mem_total_mib", Some("lots")),
                ("tgpu.x.name", Some("odd")),
                ("tgpu.0.bogus", Some("1")),
            ],
        );
        let f = parse_deep(&out);
        assert_eq!(f.driver_proc.as_deref(), Some("570.86.15"));
        assert_eq!(f.torch_driver_cuda.as_deref(), Some("12.8"));
        assert_eq!(
            f.torch_gpus,
            [
                TorchGpuFact { index: 0, name: Some(A100_SLICE.into()), mem_total_mib: Some(4864), mem_free_mib: Some(4600) },
                TorchGpuFact { index: 1, ..Default::default() },
            ]
        );
        // nvidia-smi's placeholders are no value: the row stays, the field is unknown.
        assert_eq!((f.gpus.len(), f.gpus[0].name.as_deref(), f.gpus[0].driver.as_deref()), (2, None, None));
        assert_eq!((f.gpus[1].mem_total_mib, f.driver()), (None, Some("580.65.06")));
        // A malformed per-device key is kept as an unknown fact, as `gpu.x.name` is.
        assert_eq!(f.other.get("tgpu.x.name").map(String::as_str), Some("odd"));
        assert_eq!(f.other.get("tgpu.0.bogus").map(String::as_str), Some("1"));
        // `--json` (the facts as they are) carries them.
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!((v["driver_proc"].as_str(), v["torch_driver_cuda"].as_str()), (Some("570.86.15"), Some("12.8")));
        assert_eq!(
            v["torch_gpus"][0],
            serde_json::json!({"index": 0, "name": A100_SLICE, "mem_total_mib": 4864, "mem_free_mib": 4600})
        );

        // Garbage versions are unknown.
        for bad in ["missing", "unknown", "NVIDIA UNIX", "580.x", "0", ""] {
            assert_eq!(parse_deep(&edit(HEALTHY_2GPU, &[("driver_proc", Some(bad))])).driver_proc, None, "{bad:?}");
        }
        for bad in ["12080", "12", "12.8.1", "12.x", "0.0", "none", "unknown"] {
            assert_eq!(parse_deep(&edit(HEALTHY_2GPU, &[("torch_driver_cuda", Some(bad))])).torch_driver_cuda, None, "{bad:?}");
        }
        // An older script prints none of it.
        let f = parse_deep(HEALTHY_2GPU);
        assert_eq!((f.driver_proc, f.torch_driver_cuda, f.torch_gpus.len()), (None, None, 0));
    }

    /// The false FAILs on a MIG slice (nvidia-smi can't run in its container: NVML blocked,
    /// no devices found) and their neighbours: torch's proof that the GPUs work turns an
    /// nvidia-smi that can't answer into a PASS on a MIG slice. Without the whole proof, or on
    /// whole cards whatever torch says, it FAILs as before.
    #[test]
    fn nvidia_smi_failing_falls_back_on_torchs_proof() {
        use Status::*;
        let nvml = "error: Failed to initialize NVML: Insufficient Permissions";
        let no_devices = "error: No devices were found";
        let mig_nvml = smi_down(nvml, A100_SLICE);
        let a4000_down = smi_down(no_devices, "NVIDIA RTX A4000");
        let unknown_error = smi_down("error: Failed to initialize NVML: Unknown Error", "NVIDIA RTX A4000");
        let cuda_off = edit(
            &without(&mig_nvml, &["tensor.", "tgpu.", "torch_driver_cuda"]),
            &[("cuda_available", Some("false")), ("cuda_error", Some("error: RuntimeError: CUDA unknown error"))],
        );
        let op_failed = edit(&mig_nvml, &[("tensor.0", Some("error: RuntimeError: CUDA error: unknown error"))]);
        // Two devices, the second never reported (python cut off): not a proof.
        let op_missing = edit(&mig_nvml, &[("device_count", Some("2")), ("py_done", None)]);
        let no_torch = edit(
            &without(&mig_nvml, &["torch_", "cuda_", "device_count", "tensor.", "tgpu.", "peer", "nccl"]),
            &[("torch", Some("error: ModuleNotFoundError: No module named 'torch'"))],
        );
        let smi_missing = edit(&a4000_down, &[("smi", Some("missing"))]);
        // nvidia-smi runs, but lists nothing / its query fails (a slice's NVML half-blocked).
        let listed_none = edit(&mig_nvml, &[("smi", Some("ok")), ("smi_gpus", Some("0"))]);
        let listed_none_whole = edit(&a4000_down, &[("smi", Some("ok")), ("smi_gpus", Some("0"))]);
        let query_failed = edit(
            &a4000_down,
            &[("smi", Some("ok")), ("smi_query", Some("error: Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error"))],
        );
        // `nvidia-smi -L` lists the slice though `--query-gpu` fails: MIG by that alone.
        let l_lists_slice = edit(&query_failed, &[("mig_devices", Some("1")), ("mig_profile", Some("1g.5gb"))]);

        // (label, output, nvidia-smi's status, its detail contains, overall)
        let cases: &[(&str, &str, Status, &str, Status)] = &[
            (
                "MIG slice, NVML blocked",
                &mig_nvml,
                Pass,
                "nvidia-smi can't read this MIG slice (Failed to initialize NVML: Insufficient Permissions) — torch: \
                 1×A100-SXM4-40GB MIG 1g.5gb, 4.8 GB",
                Pass,
            ),
            ("MIG slice, nvidia-smi lists none", &listed_none, Pass, "can't read this MIG slice (lists no GPUs)", Pass),
            ("MIG by nvidia-smi -L", &l_lists_slice, Pass, "can't read this MIG slice (Unable to determine the device handle", Pass),
            // A whole card: no fallback, whatever torch says — exactly as before.
            ("whole card, no devices found", &a4000_down, Fail, "No devices were found", Fail),
            ("whole card, NVML unknown error", &unknown_error, Fail, "Failed to initialize NVML: Unknown Error", Fail),
            ("whole card, nvidia-smi missing", &smi_missing, Fail, "not found (no NVIDIA driver in the container)", Fail),
            ("whole card, query failed", &query_failed, Fail, "Unable to determine the device handle for GPU 0000:41:00.0", Fail),
            ("whole card, lists none", &listed_none_whole, Fail, "lists no GPUs", Fail),
            // Without torch's whole proof: exactly as before.
            ("CUDA unavailable", &cuda_off, Fail, "Failed to initialize NVML: Insufficient Permissions", Fail),
            ("a tensor op failed", &op_failed, Fail, "Failed to initialize NVML: Insufficient Permissions", Fail),
            ("a tensor op missing", &op_missing, Fail, "Failed to initialize NVML: Insufficient Permissions", Fail),
            ("torch missing", &no_torch, Fail, "Failed to initialize NVML: Insufficient Permissions", Fail),
            // CUDA not said to be available (its line lost), whatever else came: no proof.
            ("CUDA not reported", &edit(&mig_nvml, &[("cuda_available", None)]), Fail, "Failed to initialize NVML: Insufficient Permissions", Fail),
            ("nvidia-smi missing, CUDA unavailable", &edit(&cuda_off, &[("smi", Some("missing"))]), Fail, "not found (no NVIDIA driver in the container)", Fail),
        ];
        for (label, out, status, detail, verdict) in cases {
            let checks = evaluate(&parse_deep(out), &cuda124(), None);
            let c = check(&checks, "nvidia-smi");
            assert_eq!(c.status, *status, "{label}: {c:?}");
            assert!(c.detail.contains(detail), "{label}: {:?} lacks {detail:?}", c.detail);
            assert_eq!(overall(&checks), *verdict, "{label}: {checks:#?}");
        }
        // A whole card's FAIL says what nvidia-smi said, as it did before the fallback; and with
        // nvidia-smi listing none, torch's count disagrees with its 0 (on a MIG slice that 0 is
        // no count: `MIG slice, nvidia-smi lists none` passes).
        let whole = evaluate(&parse_deep(&a4000_down), &cuda124(), None);
        assert_eq!(check(&whole, "nvidia-smi"), &Check::new("nvidia-smi", Fail, "No devices were found"));
        assert_eq!(
            check(&evaluate(&parse_deep(&listed_none_whole), &cuda124(), None), "device_count"),
            &Check::new("device_count", Fail, "torch sees 1 GPU(s), nvidia-smi 0")
        );
        // No driver version anywhere: the driver check waits on nvidia-smi off a MIG slice (as
        // before), and WARNs where torch stood in for it.
        let no_driver = |out: &str| edit(out, &[("driver_proc", None), ("torch_driver_cuda", None)]);
        assert_eq!(
            check(&evaluate(&parse_deep(&no_driver(&a4000_down)), &cuda124(), None), "driver"),
            &Check::new("driver", Skip, "needs nvidia-smi")
        );
        assert_eq!(check(&evaluate(&parse_deep(&no_driver(&mig_nvml)), &cuda124(), None), "driver").status, Warn);

        // nvidia-smi lists the parent card but not the slice (no `-L` line), torch names the
        // slice: MIG by torch's name, and said so.
        let parent_row = edit(&mig_nvml, &[("smi", Some("ok")), ("gpu.0.name", Some("NVIDIA A100-SXM4-40GB")), ("smi_gpus", Some("1"))]);
        let h = PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(&parent_row), &cuda124());
        assert_eq!(check(&h.checks, "nvidia-smi").detail, "1×A100-SXM4-40GB MIG 1g.5gb (MIG slice of A100-SXM4-40GB)");
        assert_eq!(check(&h.checks, "device_count").detail, "1 (matches nvidia-smi)");

        // nvidia-smi missing on a pod the provider lists as a MIG slice: PASS, named as the slice.
        let p = Pod { gpu_type: Some("B300 MIG 34GB".into()), ..pod("delta", "1.2.3.4") };
        let b300 = edit(&smi_missing, &[("tgpu.0.name", Some("NVIDIA B300 SXM6 AC MIG 1g.34gb")), ("tgpu.0.mem_total_mib", Some("34560"))]);
        let h = PodHealth::checked(&p, parse_deep(&b300), &cuda124());
        assert_eq!(check(&h.checks, "nvidia-smi"), &Check::new("nvidia-smi", Pass, "nvidia-smi can't read this MIG slice (not found) — torch: 1×B300 MIG 34GB, 34 GB"));
        assert_eq!(h.status, Pass, "{:#?}", h.checks);
        // ...and by the provider's name alone, torch naming the bare card.
        let h = PodHealth::checked(&p, parse_deep(&smi_missing), &cuda124());
        assert_eq!(check(&h.checks, "nvidia-smi").status, Pass, "{:#?}", h.checks);
        // The same output from a pod nothing calls MIG: a FAIL.
        assert_eq!(PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(&smi_missing), &cuda124()).status, Fail);

        // Only a FAIL replaces a pod or condemns a machine: the fallback's PASS does neither; a
        // whole card's nvidia-smi error, or a MIG slice's without torch's proof, does both.
        let h = PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(&mig_nvml), &cuda124());
        assert_eq!(h.failure_kind(), None, "{:#?}", h.checks);
        for out in [&a4000_down, &op_failed] {
            let h = PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(out), &cuda124());
            assert_eq!(h.failure_kind(), Some(FailureKind::Host), "{:#?}", h.checks);
            assert!(condemns_machine(check(&h.checks, "nvidia-smi"), h.facts.as_ref().unwrap()));
        }
    }

    /// An H100 pod where torch sees `n` GPUs and every one works: tensor ops, and with more
    /// than one a ring of peer copies and NCCL across them. nvidia-smi says `smi=ok` and lists
    /// nothing: what it lists is the caller's (`smi_rows`).
    fn torch_ok(n: u32) -> String {
        let mut out = without(HEALTHY_2GPU, &["smi_", "gpu.", "device_count", "tensor.", "peer.", "nccl", "deep_check_end"]);
        out += &format!("smi_cuda=13.0\ndevice_count={n}\n");
        for i in 0..n {
            out += &format!("tensor.{i}=ok\ntgpu.{i}.name=NVIDIA H100 80GB HBM3\n");
            if n > 1 {
                out += &format!("peer.{i}-{}=ok\n", (i + 1) % n);
            }
        }
        out += &if n > 1 { format!("nccl=ok\nnccl_ranks={n}\n") } else { "peer=skipped (1 GPU)\nnccl=skipped (1 GPU)\n".into() };
        out + "deep_check_end=1\n"
    }

    /// `out` with nvidia-smi listing `rows` H100s.
    fn smi_rows(out: &str, rows: u32) -> String {
        let mut edits: Vec<(String, Option<String>)> = vec![("smi_gpus".into(), Some(rows.to_string()))];
        for i in 0..rows {
            edits.push((format!("gpu.{i}.name"), Some("NVIDIA H100 80GB HBM3".into())));
            edits.push((format!("gpu.{i}.driver"), Some("580.65.06".into())));
        }
        let edits: Vec<(&str, Option<&str>)> = edits.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
        edit(out, &edits)
    }

    /// Review of `27adfe4..db8dff8`, #1: an 8×H100 pod takes an Xid 79 on one GPU —
    /// `--query-gpu` fails (`Unable to determine the device handle … Unknown Error`), CUDA
    /// enumerates the 7 survivors, and the tensor ops, peer copies and NCCL all pass on those 7.
    /// The MIG fallback, applied to every machine, made that a WARN: exit 0, not replaced, not
    /// remembered. Off a MIG slice there is no fallback: torch proves nothing about a GPU it
    /// doesn't enumerate, so every way nvidia-smi can fail (an error, its query, a timeout, no
    /// GPUs listed, missing) is a FAIL whatever torch says, as before the fallback.
    #[test]
    fn a_gpu_fallen_off_the_bus_fails_whatever_torch_says() {
        use Status::*;
        let off_the_bus = edit(
            &torch_ok(7),
            &[("smi", Some("ok")), ("smi_query", Some("error: Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error"))],
        );
        let f = parse_deep(&off_the_bus);
        assert_eq!((f.device_count, f.tensor.len(), f.peer.len(), f.smi_count()), (Some(7), 7, 7, None), "{off_the_bus}");

        let checks = evaluate(&f, &cuda13(), None);
        assert_eq!(
            check(&checks, "nvidia-smi"),
            &Check::new("nvidia-smi", Fail, "Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error")
        );
        assert_eq!(overall(&checks), Fail, "{checks:#?}");
        // A FAIL about the machine: replaced now, and remembered (`snapshot::bad_host_record`
        // keeps what `condemns_machine` names).
        let p = Pod { machine_id: Some("m8x2k".into()), ..pod("hotel", "1.2.3.4") };
        let h = PodHealth::checked(&p, f, &cuda13());
        assert_eq!((h.status, h.failure_kind()), (Fail, Some(FailureKind::Host)), "{:#?}", h.checks);
        let condemned: Vec<&str> =
            h.checks.iter().filter(|c| condemns_machine(c, h.facts.as_ref().unwrap())).map(|c| c.name.as_str()).collect();
        assert_eq!(condemned, ["nvidia-smi"]);
        assert!(crate::snapshot::bad_host_record(&p, &h, 1_760_000_000).is_some(), "{:#?}", h.checks);

        // Every other way nvidia-smi can't answer on a whole card, torch proving the GPUs it sees.
        let one = torch_ok(1);
        // (what, the edits, nvidia-smi's FAIL detail)
        type Case<'a> = (&'a str, &'a [(&'a str, Option<&'a str>)], &'a str);
        let cases: &[Case] = &[
            ("an error", &[("smi", Some("error: Failed to initialize NVML: Unknown Error"))], "Failed to initialize NVML: Unknown Error"),
            ("a timeout", &[("smi", Some("error: nvidia-smi timed out after 10s"))], "nvidia-smi timed out after 10s"),
            ("no GPUs listed", &[("smi", Some("ok")), ("smi_gpus", Some("0"))], "lists no GPUs"),
            ("missing", &[("smi", Some("missing"))], "not found (no NVIDIA driver in the container)"),
        ];
        for (what, edits, detail) in cases {
            let checks = evaluate(&parse_deep(&edit(&one, edits)), &cuda13(), None);
            assert_eq!(check(&checks, "nvidia-smi"), &Check::new("nvidia-smi", Fail, *detail), "{what}");
            assert_eq!(overall(&checks), Fail, "{what}: {checks:#?}");
        }
    }

    /// torch's count held to the GPUs the provider reports for the pod as well as to
    /// nvidia-smi's (review of `27adfe4..db8dff8`, #1's third fix): fewer FAILs on whole GPUs —
    /// above all where nvidia-smi lost the GPU too — and under MIG unless every GPU rented is
    /// shown to be a slice (FIX10), where `-L`'s undercount WARNs. It never turns a FAIL into a
    /// PASS, and as many or more GPUs than the provider says changes nothing.
    #[test]
    fn torchs_count_is_held_to_the_providers() {
        use Status::*;
        let rented = |n: Option<u32>| Pod { gpu_count: n, machine_id: Some("m8x2k".into()), ..pod("hotel", "1.2.3.4") };
        let slices = |n: Option<u32>| Pod { gpu_type: Some("A100 MIG 1g.5gb".into()), ..rented(n) };
        let query_error = |out: &str| {
            edit(out, &[("smi_query", Some("error: Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error"))])
        };
        let mig_two_listed = edit(
            &smi_down("ok", A100_SLICE),
            &[("gpu.0.name", Some("NVIDIA A100-SXM4-40GB")), ("smi_gpus", Some("1")), ("mig_devices", Some("2")), ("mig_profile", Some("1g.5gb"))],
        );
        let mig_unknown = smi_down("error: Failed to initialize NVML: Insufficient Permissions", A100_SLICE);
        let (whole_7, whole_8) = ("torch sees 7 GPU(s), the provider reports 8", "torch sees 8 GPU(s), nvidia-smi 9");
        let mig_l = "torch sees 1 of the 2 MIG slices — CUDA may show a process one slice; check each with \
                     CUDA_VISIBLE_DEVICES=<MIG uuid>";
        // (label, pod, output, device_count's status and detail, the pod's verdict)
        let cases: Vec<(&str, Pod, String, Status, &str, Status)> = vec![
            ("8 rented, nvidia-smi 8, torch 7", rented(Some(8)), smi_rows(&torch_ok(7), 8), Fail, "torch sees 7 GPU(s), nvidia-smi 8", Fail),
            ("8 rented, nvidia-smi failing, torch 7", rented(Some(8)), query_error(&torch_ok(7)), Fail, whole_7, Fail),
            // nvidia-smi lost the GPU as well: only the provider's count shows it (a PASS before).
            ("8 rented, nvidia-smi 7, torch 7", rented(Some(8)), smi_rows(&torch_ok(7), 7), Fail, whole_7, Fail),
            ("same, no provider count", rented(None), smi_rows(&torch_ok(7), 7), Pass, "7 (matches nvidia-smi)", Pass),
            ("8 rented, all 8 there", rented(Some(8)), smi_rows(&torch_ok(8), 8), Pass, "8 (matches nvidia-smi)", Pass),
            ("1 rented, torch 2", rented(Some(1)), smi_rows(&torch_ok(2), 2), Pass, "2 (matches nvidia-smi)", Pass),
            // A FAIL stays the FAIL it was.
            ("8 rented, nvidia-smi 9, torch 8", rented(Some(8)), smi_rows(&torch_ok(8), 9), Fail, whole_8, Fail),
            // MIG: fewer slices stay `-L`'s WARN where every GPU rented is shown to be a slice;
            // the same count PASSes.
            ("MIG, 1 rented, torch 1", slices(Some(1)), mig_unknown.clone(), Pass, "1 (MIG slice(s); nvidia-smi count unknown)", Pass),
            ("MIG, 2 rented, -L lists 2, torch 1", slices(Some(2)), mig_two_listed.clone(), Warn, mig_l, Warn),
            // Not shown (FIX10): the 1 slice torch names is all there is to see, nvidia-smi can't
            // count, and the provider rented 2 — the other may be a whole card, lost. As on whole
            // GPUs (no fallback either: nvidia-smi FAILs).
            ("MIG, 2 rented, nvidia-smi can't count, torch 1", slices(Some(2)), mig_unknown.clone(), Fail, "torch sees 1 GPU(s), the provider reports 2", Fail),
            // More rented than `-L` lists slices: the rest aren't slices.
            ("MIG, 3 rented, -L lists 2, torch 1", slices(Some(3)), mig_two_listed, Fail, "torch sees 1 GPU(s), the provider reports 3", Fail),
        ];
        // (A 12.4 floor: `smi_down`'s /proc driver is 570.)
        for (label, p, out, status, detail, verdict) in cases {
            let h = PodHealth::checked(&p, parse_deep(&out), &cuda124());
            assert_eq!(check(&h.checks, "device_count"), &Check::new("device_count", status, detail), "{label}");
            assert_eq!(h.status, verdict, "{label}: {:#?}", h.checks);
        }
        // No driver version anywhere: the driver check stands in for nvidia-smi only where the
        // fallback does — a WARN with the provider's 1, waiting on nvidia-smi with its 2.
        let no_driver = edit(&mig_unknown, &[("driver_proc", None), ("torch_driver_cuda", None)]);
        let driver = |n: u32| check(&PodHealth::checked(&slices(Some(n)), parse_deep(&no_driver), &cuda124()).checks, "driver").clone();
        assert_eq!((driver(1).status, driver(2)), (Warn, Check::new("driver", Skip, "needs nvidia-smi")));
        // The case only the provider's count catches: a host failure, and evidence enough to
        // remember the machine by.
        let h = PodHealth::checked(&rented(Some(8)), parse_deep(&smi_rows(&torch_ok(7), 7)), &cuda124());
        assert_eq!(h.failure_kind(), Some(FailureKind::Host));
        assert!(condemns_machine(check(&h.checks, "device_count"), h.facts.as_ref().unwrap()));
        // The live MIG slice, its provider counting 1: PASS.
        let live = Pod { gpu_type: Some("B300 MIG 34GB".into()), gpu_count: Some(1), ..pod("mig8", "1.2.3.4") };
        assert_eq!(PodHealth::checked(&live, parse_deep(&live8_b300_slice()), &cuda13()).status, Pass);
        // Not measured, so not in the facts' JSON (the listing has it).
        let v = serde_json::to_value(&h.facts).unwrap();
        assert!(v.get("provider_gpus").is_none(), "{v}");
    }

    /// Two MIG slices of one A100, torch seeing and proving both: `--query-gpu` lists the parent,
    /// `-L` the 2 slices.
    fn mig_two_slices() -> String {
        edit(
            HEALTHY_2GPU,
            &[
                ("gpu.0.name", Some("NVIDIA A100-SXM4-40GB")),
                ("gpu.1.name", None),
                ("gpu.1.driver", None),
                ("gpu.1.mem_total_mib", None),
                ("gpu.1.mem_used_mib", None),
                ("smi_gpus", Some("1")),
                ("mig_devices", Some("2")),
                ("mig_profile", Some("1g.5gb")),
            ],
        )
    }

    /// A mixed machine: one A100 cut into a slice, one whole (`--query-gpu` lists both cards,
    /// `-L` the one slice), torch seeing and proving both.
    fn mig_mixed() -> String {
        edit(&mig_two_slices(), &[("gpu.1.name", Some("NVIDIA A100-SXM4-40GB")), ("smi_gpus", Some("2")), ("mig_devices", Some("1"))])
    }

    /// Review of `db8dff8..da53b0c`, codex: the MIG fallback took a slice's presence for proof
    /// that the pod is all slices. The mixed pod loses its whole card: `--query-gpu` fails, so no
    /// inventory says it is mixed; `-L` still lists the slice, and torch sees and proves just
    /// that one. With the provider counting 2, nvidia-smi PASSed through the fallback and the
    /// missing card read as a MIG undercount (a WARN, exit 0: not replaced). Now the fallback and
    /// that WARN need every GPU of the pod shown to be a slice: the provider's 2 is more than the
    /// 1 slice seen, so it FAILs as a whole-GPU pod does. Where the count fits the slices seen,
    /// both stand; torch naming a whole card beside a slice is no pure-MIG pod whatever the
    /// provider says.
    #[test]
    fn a_mixed_pod_that_lost_its_whole_gpu_fails_though_its_slice_survives() {
        use Status::*;
        let query = "Unable to determine the device handle for GPU 0000:41:00.0: Unknown Error";
        let query_error = format!("error: {query}");
        let query_failed = edit(
            &without(&mig_mixed(), &["gpu.", "smi_gpus"]),
            &[("smi_query", Some(query_error.as_str())), ("driver_proc", Some("580.65.06")), ("tgpu.0.name", Some(A100_SLICE))],
        );
        let lost = edit(
            &without(&query_failed, &["tensor.1", "peer.", "nccl"]),
            &[("device_count", Some("1")), ("peer", Some("skipped (1 GPU)")), ("nccl", Some("skipped (1 GPU)"))],
        );
        let f = parse_deep(&lost);
        assert_eq!((f.smi_count(), f.mig_devices, f.device_count, f.mixed_inventory()), (None, Some(1), Some(1), false), "{lost}");
        let rented = |n: Option<u32>| Pod { gpu_count: n, machine_id: Some("m8x2k".into()), ..pod("india", "1.2.3.4") };

        let h = PodHealth::checked(&rented(Some(2)), f.clone(), &cuda124());
        assert_eq!(check(&h.checks, "nvidia-smi"), &Check::new("nvidia-smi", Fail, query));
        assert_eq!(check(&h.checks, "device_count"), &Check::new("device_count", Fail, "torch sees 1 GPU(s), the provider reports 2"));
        assert_eq!((h.status, h.failure_kind()), (Fail, Some(FailureKind::Host)), "{:#?}", h.checks);

        // The provider's count fits the one slice seen: the fallback stands, nothing is short.
        let h = PodHealth::checked(&rented(Some(1)), f.clone(), &cuda124());
        assert_eq!(check(&h.checks, "nvidia-smi").status, Pass, "{:#?}", h.checks);
        assert_eq!(check(&h.checks, "device_count"), &Check::new("device_count", Pass, "1 (matches nvidia-smi's 1 MIG slice(s))"));
        assert_eq!(h.status, Pass, "{:#?}", h.checks);

        // torch sees a whole card beside the slice (both proven): a mixed pod by torch's own
        // names, with or without the provider's count.
        let named_whole = edit(&query_failed, &[("tgpu.1.name", Some("NVIDIA A100-SXM4-40GB"))]);
        for n in [None, Some(2)] {
            let h = PodHealth::checked(&rented(n), parse_deep(&named_whole), &cuda124());
            assert_eq!(check(&h.checks, "nvidia-smi"), &Check::new("nvidia-smi", Fail, query), "provider {n:?}");
            assert_eq!(h.status, Fail, "provider {n:?}: {:#?}", h.checks);
        }
    }

    /// Two slices of one parent: `--query-gpu` lists the 1 parent — the count to match is the
    /// slices `nvidia-smi -L` lists. CUDA may show a process just one of them, or the other may
    /// be inaccessible or broken: torch seeing fewer WARNs (codex #2 of the review of
    /// `27adfe4..db8dff8`); more FAILs. Whole cards beside the slices: counted as before MIG.
    #[test]
    fn mig_slices_are_counted_against_nvidia_smi_l() {
        use Status::*;
        let two_slices = mig_two_slices();
        let one_seen = edit(
            &without(&two_slices, &["tensor.1", "peer.", "nccl"]),
            &[("device_count", Some("1")), ("peer", Some("skipped (1 GPU)")), ("nccl", Some("skipped (1 GPU)"))],
        );
        let three_seen = edit(&two_slices, &[("device_count", Some("3")), ("tensor.2", Some("ok"))]);
        let mig_unknown = smi_down("error: Failed to initialize NVML: Insufficient Permissions", A100_SLICE);
        // More cards than slices: counted as whole GPUs, as before MIG.
        let mixed = mig_mixed();
        let mixed_one_seen = edit(
            &without(&mixed, &["tensor.1", "peer.", "nccl"]),
            &[("device_count", Some("1")), ("peer", Some("skipped (1 GPU)")), ("nccl", Some("skipped (1 GPU)"))],
        );
        let mixed_three_seen = edit(&mixed, &[("device_count", Some("3")), ("tensor.2", Some("ok"))]);
        let undercount = "torch sees 1 of the 2 MIG slices — CUDA may show a process one slice; check each with \
                          CUDA_VISIBLE_DEVICES=<MIG uuid>";
        // (label, output, status, detail, overall)
        let cases: &[(&str, &str, Status, &str, Status)] = &[
            ("2 slices, 1 parent row", &two_slices, Pass, "2 (matches nvidia-smi's 2 MIG slice(s))", Pass),
            // One slice inaccessible (or CUDA showing one): not a PASS.
            ("2 slices, torch sees 1", &one_seen, Warn, undercount, Warn),
            ("2 slices, torch sees 3", &three_seen, Fail, "torch sees 3 GPU(s), nvidia-smi 2 MIG slice(s)", Fail),
            ("a slice, nvidia-smi can't count", &mig_unknown, Pass, "1 (MIG slice(s); nvidia-smi count unknown)", Pass),
            ("mixed, torch sees both", &mixed, Pass, "2 (matches nvidia-smi)", Pass),
            // By the slices alone this would match (1 of 1 slice) and hide the whole card.
            ("mixed, torch sees 1", &mixed_one_seen, Fail, "torch sees 1 GPU(s), nvidia-smi 2", Fail),
            ("mixed, torch sees 3", &mixed_three_seen, Fail, "torch sees 3 GPU(s), nvidia-smi 2", Fail),
            // Not MIG: as before.
            ("whole cards", HEALTHY_2GPU, Pass, "2 (matches nvidia-smi)", Pass),
        ];
        // (A 12.4 floor: `smi_down`'s /proc driver is 570.)
        for (label, out, status, detail, verdict) in cases {
            let checks = evaluate(&parse_deep(out), &cuda124(), None);
            let c = check(&checks, "device_count");
            assert_eq!((c.status, c.detail.as_str()), (*status, *detail), "{label}");
            assert_eq!(overall(&checks), *verdict, "{label}: {checks:#?}");
        }
        assert_eq!(parse_deep(&two_slices).gpu_label(), "2×A100-SXM4-40GB MIG 1g.5gb");
        // A WARN replaces no pod and condemns no machine.
        let h = PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(&one_seen), &cuda13());
        assert_eq!((h.status, h.failure_kind()), (Warn, None), "{:#?}", h.checks);
    }

    /// Live 8 (2026-10-20, a RunPod `1×B300 MIG 34GB` pod), its facts as the script printed
    /// them: nvidia-smi answers (driver 595.91.07, CUDA 13.2) but `--query-gpu`'s memory is
    /// `[Insufficient Permissions]`, `-L` lists the slice, `/proc` has the driver too, and torch
    /// 2.14.1 sees the slice — without `_cuda_getDriverVersion`, so no driver CUDA from it.
    fn live8_b300_slice() -> String {
        edit(
            &one_gpu(),
            &[
                ("smi_cuda", Some("13.2")),
                ("gpu.0.name", Some("NVIDIA B300 SXM6 AC")),
                ("gpu.0.driver", Some("595.91.07")),
                ("gpu.0.mem_total_mib", Some("[Insufficient Permissions]")),
                ("gpu.0.mem_used_mib", Some("[Insufficient Permissions]")),
                ("mig_devices", Some("1")),
                ("mig_profile", Some("1g.34gb")),
                ("driver_proc", Some("595.91.07")),
                ("torch_version", Some("2.14.1+cu130")),
                ("tgpu.0.name", Some("NVIDIA B300 SXM6 AC MIG 1g.34gb")),
                ("tgpu.0.mem_total_mib", Some("31232")),
                ("tgpu.0.mem_free_mib", Some("30955")),
            ],
        )
    }

    /// The live MIG slice PASSes as it did live, the driver judged by nvidia-smi's version.
    #[test]
    fn a_live_b300_mig_slice_passes_with_the_driver_from_nvidia_smi() {
        let p = Pod { gpu_type: Some("B300 MIG 34GB".into()), gpu_count: Some(1), ..pod("mig8", "1.2.3.4") };
        let f = parse_deep(&live8_b300_slice());
        assert_eq!((f.gpus[0].mem_total_mib, f.gpus[0].mem_used_mib, f.torch_driver_cuda.as_deref()), (None, None, None));
        let h = PodHealth::checked(&p, f, &cuda13());
        assert_eq!(h.status, Status::Pass, "{:#?}", h.checks);
        let detail = |name: &str| check(&h.checks, name).detail.clone();
        assert_eq!(detail("nvidia-smi"), "1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)");
        assert_eq!(detail("driver"), "595.91.07 ≥ 580 (CUDA 13.0)");
        assert_eq!(detail("device_count"), "1 (matches nvidia-smi's 1 MIG slice(s))");
        let table = render_health_table(std::slice::from_ref(&h));
        assert!(table.contains("1×B300 MIG 34GB") && table.contains("595.91.07") && table.contains("13.2"), "{table}");
        assert!(!table.contains('*'), "nvidia-smi's CUDA, unmarked: {table}");
    }

    /// No driver version from nvidia-smi: `/proc/driver/nvidia/version`'s, judged the same way;
    /// else the CUDA version torch says the driver supports, against the CUDA floor the minimum
    /// was derived from; else as before.
    #[test]
    fn the_driver_falls_back_on_proc_then_on_torchs_cuda() {
        use Status::*;
        let down = smi_down("error: Failed to initialize NVML: Insufficient Permissions", A100_SLICE);
        let proc_only = |v: &str| edit(&down, &[("driver_proc", Some(v)), ("torch_driver_cuda", None)]);
        let torch_only = |v: &str| edit(&down, &[("driver_proc", Some("missing")), ("torch_driver_cuda", Some(v))]);
        let neither = edit(&down, &[("driver_proc", Some("missing")), ("torch_driver_cuda", None)]);
        // nvidia-smi answers, but with no driver version: its own placeholders.
        let smi_no_version = edit(
            HEALTHY_2GPU,
            &[("gpu.0.driver", Some("[N/A]")), ("gpu.1.driver", Some("[N/A]")), ("torch_driver_cuda", Some("12.8"))],
        );
        let floor = |cuda: &str| HealthPolicy { min_driver: driver_floor_for_cuda(&[cuda]), ..Default::default() };
        let explicit = HealthPolicy {
            min_driver: Some(DriverFloor { version: vec![550], why: "MIN_DRIVER_VERSION".into(), cuda: None }),
            ..Default::default()
        };
        let none = HealthPolicy::default();
        let (c124, c13, c131) = (cuda124(), cuda13(), floor("13.1"));
        // (label, output, policy, status, detail)
        let cases: Vec<(&str, String, &HealthPolicy, Status, &str)> = vec![
            ("/proc ≥ floor", proc_only("570.86.15"), &c124, Pass, "570.86.15 ≥ 550 (CUDA 12.4; from /proc/driver/nvidia/version)"),
            ("/proc < floor", proc_only("535.129.03"), &c13, Fail, "535.129.03 < 580 (CUDA 13.0 needs ≥ 580; from /proc/driver/nvidia/version)"),
            ("/proc, no floor", proc_only("570.86.15"), &none, Skip, "570.86.15 (no minimum configured; from /proc/driver/nvidia/version)"),
            ("/proc vs an explicit floor", proc_only("545.23.08"), &explicit, Fail, "545.23.08 < 550 (MIN_DRIVER_VERSION needs ≥ 550; from /proc/driver/nvidia/version)"),
            ("nvidia-smi's beats /proc's", edit(HEALTHY_2GPU, &[("driver_proc", Some("535.129.03"))]), &c13, Pass, "580.65.06 ≥ 580 (CUDA 13.0)"),
            ("torch's CUDA ≥ floor", torch_only("12.8"), &c124, Pass, "driver supports CUDA 12.8 ≥ 12.4 (via torch; nvidia-smi unavailable)"),
            ("torch's CUDA = floor", torch_only("12.4"), &c124, Pass, "driver supports CUDA 12.4 ≥ 12.4 (via torch; nvidia-smi unavailable)"),
            ("torch's CUDA < floor", torch_only("12.2"), &c124, Fail, "driver supports CUDA 12.2 < 12.4 (needed for CUDA 12.4; via torch; nvidia-smi unavailable)"),
            ("torch's CUDA < a 13 floor", torch_only("12.9"), &c13, Fail, "driver supports CUDA 12.9 < 13.0 (needed for CUDA 13.0; via torch"),
            // Any 13.x runs on a 13.0 driver (the 580 branch), as the version check says.
            ("13.0 driver, 13.1 floor", torch_only("13.0"), &c131, Pass, "driver supports CUDA 13.0 ≥ 13.0 (via torch; nvidia-smi unavailable)"),
            ("13.2 driver, 13.0 floor", torch_only("13.2"), &c13, Pass, "driver supports CUDA 13.2 ≥ 13.0"),
            ("torch's CUDA, no floor", torch_only("12.8"), &none, Skip, "driver supports CUDA 12.8 (no minimum configured; via torch; nvidia-smi unavailable)"),
            ("torch's CUDA vs an explicit floor", torch_only("12.8"), &explicit, Warn, "version not reported (torch: the driver supports CUDA 12.8"),
            ("nvidia-smi's [N/A]: torch's CUDA", smi_no_version, &c124, Pass, "driver supports CUDA 12.8 ≥ 12.4 (via torch; nvidia-smi gave no driver version)"),
            ("neither, torch proves the GPUs", neither.clone(), &c124, Warn, "version not reported"),
            (
                "neither, no proof",
                edit(&neither, &[("tensor.0", Some("error: RuntimeError: CUDA error: unknown error"))]),
                &c124,
                Skip,
                "needs nvidia-smi",
            ),
        ];
        for (label, out, policy, status, detail) in cases {
            let c = check(&evaluate(&parse_deep(&out), policy, None), "driver").clone();
            assert_eq!(c.status, status, "{label}: {c:?}");
            assert!(c.detail.contains(detail), "{label}: {:?} lacks {detail:?}", c.detail);
        }
        // The floor remembers the CUDA it came from (none for an explicit one).
        assert_eq!(cuda124().min_driver.unwrap().cuda.as_deref(), Some("12.4"));
        assert_eq!(driver_cuda_needed("13.1"), Some(vec![13, 0]));
        assert_eq!(driver_cuda_needed("12.8"), Some(vec![12, 8]));
        assert_eq!(driver_cuda_needed("x"), None);
    }

    /// Labels and the table's columns where nvidia-smi can't name the GPUs: torch's names
    /// (MIG-aware), `/proc`'s driver, torch's CUDA (marked, with a footnote).
    #[test]
    fn labels_and_columns_fall_back_on_torch() {
        let label = |out: &str| parse_deep(out).gpu_label();
        let mig = smi_down("error: Failed to initialize NVML: Insufficient Permissions", A100_SLICE);
        let a4000 = smi_down("error: No devices were found", "NVIDIA RTX A4000");
        let both = edit(HEALTHY_2GPU, &[("tgpu.0.name", Some("NVIDIA RTX A4000")), ("tgpu.1.name", Some("NVIDIA RTX A4000"))]);
        let na_names = edit(&both, &[("gpu.0.name", Some("[N/A]")), ("gpu.1.name", Some("[N/A]")), ("tgpu.1.name", Some("NVIDIA RTX A5000"))]);
        // nvidia-smi lists the parent card (no `-L` slices), torch names the slice.
        let parent_row = edit(&mig, &[("smi", Some("ok")), ("gpu.0.name", Some("NVIDIA A100-SXM4-40GB")), ("smi_gpus", Some("1"))]);
        let cases: &[(&str, &str, &str)] = &[
            ("nvidia-smi down, MIG", &mig, "1×A100-SXM4-40GB MIG 1g.5gb"),
            ("nvidia-smi down, whole card", &a4000, "1×RTX A4000"),
            ("nvidia-smi's [N/A] names", &na_names, "1×RTX A4000+1×RTX A5000"),
            ("torch's slice over nvidia-smi's parent", &parent_row, "1×A100-SXM4-40GB MIG 1g.5gb"),
            // nvidia-smi's names, when it has them, as before.
            ("both name it", &both, "2×RTX A4000"),
            ("nothing named", &without(&a4000, &["tgpu."]), "1×GPU"),
        ];
        for (what, out, want) in cases {
            assert_eq!(label(out), *want, "{what}");
        }

        // The table: torch's label, /proc's driver, torch's CUDA marked — and the footnote once.
        let results = vec![
            PodHealth::checked(&pod("apple", "1.1.1.1"), parse_deep(HEALTHY_2GPU), &cuda124()),
            PodHealth::checked(&pod("delta", "1.2.3.4"), parse_deep(&mig), &cuda124()),
            PodHealth::checked(&pod("echo", "1.2.3.5"), parse_deep(&a4000), &cuda124()),
        ];
        let table = render_health_table(&results);
        let row = |name: &str| -> Vec<String> {
            let line = table.lines().find(|l| l.starts_with(name)).unwrap_or_else(|| panic!("no {name} row in\n{table}"));
            line.split("  ").map(str::trim).filter(|c| !c.is_empty()).map(String::from).collect()
        };
        // NAME, RESULT, then GPUS DRIVER CUDA.
        assert_eq!(row("devtest-apple")[2..5], ["2×RTX A4000", "580.65.06", "13.0"]);
        assert_eq!(row("devtest-delta")[2..5], ["1×A100-SXM4-40GB MIG 1g.5gb", "570.86.15", "12.8*"]);
        assert_eq!(row("devtest-echo")[2..5], ["1×RTX A4000", "570.86.15", "12.8*"]);
        assert!(table.ends_with("\n* from torch: nvidia-smi unavailable\n"), "{table}");
        assert_eq!(table.matches("* from torch").count(), 1);
        // Nothing read by torch, no mark and no footnote (the table as before).
        let plain = render_health_table(&results[..1]);
        assert!(!plain.contains('*'), "{plain}");
        assert!(render_report(&results, false).contains("\n* from torch: nvidia-smi unavailable\n\n"), "under the table");
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

    /// Finding #24: the script reads the MIG slice from `nvidia-smi -L` (the recorded live
    /// output of a B300 MIG pod), run for real against a stub nvidia-smi.
    #[cfg(target_os = "linux")]
    #[test]
    fn script_reports_a_mig_slice_from_nvidia_smi_l() {
        if !(have("bash") && have("timeout") && have("base64")) {
            eprintln!("bash/timeout/base64 missing — skipping");
            return;
        }
        let dir = scratch("stubs-mig");
        let stub = |name: &str, body: &str| {
            let p = dir.join(name);
            crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}")).unwrap();
        };
        stub(
            "nvidia-smi",
            "case \"$*\" in\n\
             *--query-gpu*) printf '0, NVIDIA B300 SXM6 AC, 595.91.07, [Insufficient Permissions], [N/A]\\n' ;;\n\
             -L) printf 'GPU 0: NVIDIA B300 SXM6 AC (UUID: GPU-03979fe0)\\n  MIG 1g.34gb     Device  0: (UUID: MIG-c121b7f0)\\n' ;;\n\
             *) echo '| NVIDIA-SMI 595.91.07    Driver Version: 595.91.07    CUDA Version: 13.2     |' ;;\n\
             esac\n",
        );
        // No network, no python: only the GPU facts matter here.
        stub("curl", "exit 3\n");
        stub("python", "exit 0\n");
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(deep_check_inner_command())
            .env("PATH", path)
            .env("HOME", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let f = parse_deep(&String::from_utf8_lossy(&out.stdout));
        assert_eq!((f.mig_devices, f.mig_profile.as_deref()), (Some(1), Some("1g.34gb")), "{}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(f.gpu_label(), "1×B300 SXM6 AC MIG 1g.34gb");
    }

    /// The delivered command, run for real against stub `nvidia-smi` / `curl` / `python`
    /// (first on PATH — so no GPU, no network): the bash plumbing produces facts the
    /// parser and evaluator understand, end to end.
    #[cfg(target_os = "linux")]
    #[test]
    fn script_runs_against_stub_tools_and_parses() {
        if !(have("bash") && have("timeout") && have("base64")) {
            eprintln!("bash/timeout/base64 missing — skipping");
            return;
        }
        let dir = scratch("stubs");
        let stub = |name: &str, body: &str| {
            let p = dir.join(name);
            crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}")).unwrap();
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
        // A home with what setup's VS Code warm-up leaves behind (not the operator's own).
        let home = dir.join("home");
        let vs = home.join(".vscode-server");
        let server = vs.join("cli/servers/Stable-2a59476c9bfcb90b3ddc372c36762471b7dfad1c/server/bin");
        std::fs::create_dir_all(&server).unwrap();
        stub("home/.vscode-server/cli/servers/Stable-2a59476c9bfcb90b3ddc372c36762471b7dfad1c/server/bin/code-server", "");
        for ext in ["ms-python.python-2026.1.0", "ms-python.vscode-pylance-2026.1.1", "ms-toolsai.jupyter-2026.2.0", "MS-Python.debugpy-2025.10.0-linux-x64", "not-an-extension"] {
            std::fs::create_dir_all(vs.join("extensions").join(ext)).unwrap();
        }
        std::fs::create_dir_all(vs.join("data/Machine")).unwrap();
        std::fs::write(
            vs.join("data/Machine/settings.json"),
            "{\n    \"editor.fontSize\": 13,\n    \"python.defaultInterpreterPath\": \"/opt/arena-env/bin/python\"\n}\n",
        )
        .unwrap();
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(deep_check_inner_command())
            .env("PATH", path)
            .env("HOME", &home)
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

        assert_eq!(f.vscode_server.as_deref(), Some("2a59476c9bfcb90b3ddc372c36762471b7dfad1c"));
        assert_eq!(f.vscode_servers, Some(1));
        assert_eq!(f.vscode_python.as_deref(), Some("/opt/arena-env/bin/python"));
        let exts: Vec<(&str, &str)> = f.vscode_extensions.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            exts,
            [
                ("ms-python.debugpy", "2025.10.0-linux-x64"),
                ("ms-python.python", "2026.1.0"),
                ("ms-python.vscode-pylance", "2026.1.1"),
                ("ms-toolsai.jupyter", "2026.2.0"),
            ]
        );

        let checks = evaluate(&f, &cuda13(), None);
        for name in ["nvidia-smi", "driver", "torch", "cuda", "device_count", "gpu0", "gpu1", "peer_copy", "nccl", "network", "vscode"] {
            assert_eq!(check(&checks, name).status, Status::Pass, "{name}: {checks:#?}");
        }
        assert_eq!(check(&checks, "vscode").detail, "server 2a59476; 3 extensions; python /opt/arena-env/bin/python");
    }

    /// A stand-in `torch` for the script test (first on PYTHONPATH): one CUDA device that is a
    /// MIG slice, without a GPU — so the inline python's new facts (torch's names, memory, the
    /// driver's CUDA) run for real. Just enough tensor for `tensor_check`: `arange(n) * 2 + 1`
    /// sums to n², a matmul's sum is finite. `FAKE_TORCH_DRIVER_API=gone`: no private
    /// `_cuda_getDriverVersion`, as in torch 2.14.
    const FAKE_TORCH: &str = r#"
import os

__version__ = "2.9.0+cu128"
int64 = "int64"


class version:
    cuda = "12.8"


class _C:
    @staticmethod
    def _cuda_getDriverVersion():
        if os.environ.get("FAKE_TORCH_DRIVER_API") == "gone":
            raise AttributeError("module 'torch._C' has no attribute '_cuda_getDriverVersion'")
        return 12080


class _T:
    def __init__(self, n=0, a=1, b=0):
        self.n, self.a, self.b = n, a, b

    def __mul__(self, k):
        return _T(self.n, self.a * k, self.b * k)

    def __add__(self, k):
        return _T(self.n, self.a, self.b + k)

    def __matmul__(self, other):
        return self

    def abs(self):
        return self

    def sum(self):
        return _T(0, 0, self.a * self.n * (self.n - 1) // 2 + self.b * self.n)

    def item(self):
        return self.b


def arange(n, device=None, dtype=None):
    return _T(n)


def randn(*shape, device=None):
    return _T(0, 0, 1)


class _Props:
    total_memory = 4864 << 20


class cuda:
    @staticmethod
    def is_available():
        return True

    @staticmethod
    def device_count():
        return 1

    @staticmethod
    def get_device_name(i):
        return "NVIDIA A100-SXM4-40GB MIG 1g.5gb"

    @staticmethod
    def get_device_properties(i):
        return _Props()

    @staticmethod
    def mem_get_info(i):
        return (4600 << 20, 4864 << 20)

    @staticmethod
    def synchronize(i=None):
        pass
"#;

    /// A stand-in `ctypes` beside [`FAKE_TORCH`] (stdlib modules come after PYTHONPATH): the
    /// script's libcuda read, without a GPU or a libcuda — and none of the test machine's own,
    /// which would make the result the machine's. `FAKE_LIBCUDA`: the version
    /// `cuDriverGetVersion` reports, or `error` for a call that fails; unset, no libcuda.
    const FAKE_CTYPES: &str = r#"
import os


class c_int:
    def __init__(self, value=0):
        self.value = value


def byref(obj):
    return obj


class CDLL:
    def __init__(self, name):
        self._v = os.environ.get("FAKE_LIBCUDA")
        if name != "libcuda.so.1" or not self._v:
            raise OSError(f"{name}: cannot open shared object file: No such file or directory")

    def cuDriverGetVersion(self, ref):
        if self._v == "error":
            return 100  # CUDA_ERROR_NO_DEVICE
        ref.value = int(self._v)
        return 0
"#;

    /// The MIG fallback end to end: nvidia-smi failing as it does in a slice's container (exit
    /// 6 "No devices were found"; NVML "Insufficient Permissions"), torch working (the fake
    /// module above, run by the real python), the driver in a fake `/proc/driver/nvidia/version`
    /// (or none) — the script's facts give a PASS for the slice, the driver judged from `/proc`,
    /// else from the CUDA version the driver supports: libcuda's (the fake `ctypes`), else
    /// torch's private reading, else unknown (live, torch 2.14 on a B300 MIG slice: no
    /// `_cuda_getDriverVersion`).
    #[cfg(target_os = "linux")]
    #[test]
    fn script_falls_back_on_torch_when_nvidia_smi_cant_answer() {
        use Status::*;
        if !(have("bash") && have("timeout") && have("base64") && have("python3")) {
            eprintln!("bash/timeout/base64/python3 missing — skipping");
            return;
        }
        let nvrm = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  570.86.15  Release Build  \
                    (dvs-builder@U16-I3-B03-4-3)  Thu Jan 23 00:43:02 UTC 2025\n\
                    GCC version:  gcc version 12.3.0 (Ubuntu 12.3.0-1ubuntu1~22.04)\n";
        let (no_devices, nvml) = ((6, "No devices were found"), (4, "Failed to initialize NVML: Insufficient Permissions"));
        let via = |c: &str| Check::new("driver", Pass, format!("driver supports CUDA {c} ≥ 12.4 (via torch; nvidia-smi unavailable)"));
        // (nvidia-smi's exit and message, /proc file present, FAKE_LIBCUDA, torch's private API
        // there, the driver's CUDA fact, the driver check)
        type Case<'a> = ((i32, &'a str), bool, Option<&'a str>, bool, Option<&'a str>, Check);
        let cases: Vec<Case> = vec![
            (no_devices, true, Some("13020"), true, Some("13.2"), Check::new("driver", Pass, "570.86.15 ≥ 550 (CUDA 12.4; from /proc/driver/nvidia/version)")),
            // libcuda first; torch's reading (12.8) only when libcuda can't say.
            (nvml, false, Some("13020"), true, Some("13.2"), via("13.2")),
            (nvml, false, None, true, Some("12.8"), via("12.8")),
            (nvml, false, Some("error"), true, Some("12.8"), via("12.8")),
            (nvml, false, Some("13020"), false, Some("13.2"), via("13.2")),
            // Neither: unknown, the script and every other fact unharmed.
            (nvml, false, None, false, None, Check::new("driver", Warn, "version not reported")),
        ];
        for (n, ((code, msg), proc_file, libcuda, torch_api, cuda, driver)) in cases.into_iter().enumerate() {
            let dir = scratch(&format!("stubs-mig-down-{n}"));
            let stub = |name: &str, body: &str| {
                crate::remote::write_test_script(dir.join(name), format!("#!/bin/sh\n{body}")).unwrap();
            };
            stub("nvidia-smi", &format!("echo '{msg}'\nexit {code}\n"));
            stub("curl", "exit 3\n");
            stub("python", "exec python3 \"$@\"\n");
            std::fs::create_dir_all(dir.join("pylib")).unwrap();
            std::fs::write(dir.join("pylib/torch.py"), FAKE_TORCH).unwrap();
            std::fs::write(dir.join("pylib/ctypes.py"), FAKE_CTYPES).unwrap();
            let version = dir.join("nvidia-version");
            if proc_file {
                std::fs::write(&version, nvrm).unwrap();
            }
            let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c")
                .arg(deep_check_inner_command())
                .env("PATH", path)
                .env("HOME", &dir)
                .env("PYTHONPATH", dir.join("pylib"))
                .env("ARENA_TEST_NVIDIA_VERSION_FILE", &version)
                .env_remove("FAKE_LIBCUDA")
                .env("FAKE_TORCH_DRIVER_API", if torch_api { "there" } else { "gone" });
            if let Some(v) = libcuda {
                cmd.env("FAKE_LIBCUDA", v);
            }
            let out = cmd.output().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            for line in stdout.lines() {
                assert!(line.contains('='), "stray stdout line {line:?}\n{stdout}");
            }
            let f = parse_deep(&stdout);
            assert!(f.started && f.complete && f.py_done, "{stdout}\n{stderr}");
            assert_eq!(f.smi, Some(format!("error: {msg}")), "{stdout}");
            assert_eq!(f.driver_proc.as_deref(), proc_file.then_some("570.86.15"), "{stdout}");
            assert!(stdout.contains(if proc_file { "\ndriver_proc=570.86.15\n" } else { "\ndriver_proc=missing\n" }), "{stdout}");
            assert_eq!(f.torch_driver_cuda.as_deref(), cuda, "{stdout}");
            assert_eq!(
                f.torch_gpus,
                [TorchGpuFact { index: 0, name: Some(A100_SLICE.into()), mem_total_mib: Some(4864), mem_free_mib: Some(4600) }],
                "{stdout}"
            );
            assert_eq!((f.device_count, f.tensor.get(&0).map(String::as_str)), (Some(1), Some("ok")), "{stdout}");

            let checks = evaluate(&f, &cuda124(), None);
            assert_eq!(
                check(&checks, "nvidia-smi"),
                &Check::new(
                    "nvidia-smi",
                    Pass,
                    format!("nvidia-smi can't read this MIG slice ({msg}) — torch: 1×A100-SXM4-40GB MIG 1g.5gb, 4.8 GB")
                ),
                "{stdout}"
            );
            assert_eq!(check(&checks, "driver"), &driver, "{stdout}");
            for name in ["torch", "cuda", "device_count", "gpu0"] {
                assert_eq!(check(&checks, name).status, Pass, "{name}: {checks:#?}");
            }
        }
    }

    /// The driver's CUDA version as the driver API encodes it (1000 × major + 10 × minor) →
    /// `major.minor`, and the order it is asked in: libcuda, then torch's private API. The
    /// script's own python functions, imported (its `main` is behind `__main__`), with stand-in
    /// `ctypes` and `torch` objects — no GPU, no libcuda.
    #[test]
    fn script_reads_the_drivers_cuda_from_libcuda_then_torch() {
        if !have("python3") {
            eprintln!("python3 not installed — skipping");
            return;
        }
        let start = DEEP_CHECK_SCRIPT.find("<<'PYEOF'\n").expect("python heredoc") + "<<'PYEOF'\n".len();
        let len = DEEP_CHECK_SCRIPT[start..].find("\nPYEOF\n").expect("heredoc end");
        let dir = scratch("pydriver");
        std::fs::write(dir.join("deep_check.py"), &DEEP_CHECK_SCRIPT[start..start + len]).unwrap();
        let probe = r#"
import sys, types
sys.path.insert(0, sys.argv[1])
import deep_check as d


def show(f, *a):
    try:
        return f(*a)
    except Exception as e:
        return type(e).__name__


for v in [13020, 12080, 12040, 13000, 9020, "13020", 0, -1, "x", None]:
    print("version", v, show(d.cuda_version, v))


def libcuda(code, value):
    m = types.ModuleType("ctypes")

    class c_int:
        def __init__(self, value=0):
            self.value = value

    class CDLL:
        def __init__(self, name):
            if code is None:
                raise OSError(name)

        def cuDriverGetVersion(self, ref):
            ref.value = value
            return code

    m.c_int, m.byref, m.CDLL = c_int, lambda x: x, CDLL
    return m


def torch(v):
    t = types.SimpleNamespace(_C=types.SimpleNamespace())
    if v is not None:
        t._C._cuda_getDriverVersion = lambda: v
    return t


for what, lib, tv in [
    ("libcuda", libcuda(0, 13020), 12080),
    ("libcuda, torch gone", libcuda(0, 13020), None),
    ("libcuda fails", libcuda(100, 0), 12080),
    ("libcuda says 0", libcuda(0, 0), 12080),
    ("no libcuda", libcuda(None, 0), 12080),
    ("neither", libcuda(None, 0), None),
]:
    sys.modules["ctypes"] = lib
    print("driver", what, show(d.driver_cuda, torch(tv)))
"#;
        let out = std::process::Command::new("python3").arg("-I").arg("-c").arg(probe).arg(&dir).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        let want = "\
version 13020 13.2
version 12080 12.8
version 12040 12.4
version 13000 13.0
version 9020 9.2
version 13020 13.2
version 0 ValueError
version -1 ValueError
version x ValueError
version None TypeError
driver libcuda 13.2
driver libcuda, torch gone 13.2
driver libcuda fails 12.8
driver libcuda says 0 12.8
driver no libcuda 12.8
driver neither AttributeError
";
        assert_eq!(stdout, want);
        // What the parser keeps of it: `major.minor`, nothing else.
        for (v, want) in [("13.2", Some("13.2")), ("9.2", Some("9.2")), ("13020", None), ("13.2.0", None)] {
            assert_eq!(parse_deep(&edit(HEALTHY_2GPU, &[("torch_driver_cuda", Some(v))])).torch_driver_cuda.as_deref(), want, "{v}");
        }
    }
}
