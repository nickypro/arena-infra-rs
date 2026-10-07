use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A provider-agnostic view of a running (or requested) machine.
///
/// `Default` exists so tests and fakes can spell only the fields they care about
/// (`Pod { name, ..Default::default() }`); providers always fill `id`/`name`/`status`.
/// `Deserialize` lets `pods list --json` output round-trip (scripts, snapshots).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pod {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub status: String,
    pub gpu_type: Option<String>,
    /// GPUs attached to the pod, when the provider reports it (RunPod `gpuCount`, Vast
    /// `num_gpus`). `None` for CPU VMs and providers that don't say.
    #[serde(default)]
    pub gpu_count: Option<u32>,
    pub cost_per_hr: Option<f64>,
    pub ssh_ip: Option<String>,
    pub ssh_port: Option<u16>,
    /// The host's scheduled maintenance window, when the provider exposes one (RunPod's
    /// GraphQL `machine.maintenanceStart/End/Note`). A pod on a host going into
    /// maintenance will be stopped by the provider, so this is surfaced in `pods list`.
    #[serde(default)]
    pub maintenance: Option<Maintenance>,
}

/// A provider-reported maintenance window for the host a pod runs on. Times are kept as
/// the provider's own strings (RunPod sends ISO-8601) — they're displayed, not computed on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintenance {
    pub start: Option<String>,
    pub end: Option<String>,
    pub note: Option<String>,
}

/// What we want when creating a machine. Provider implementations translate this
/// into their own API payloads.
#[derive(Debug, Clone)]
pub struct PodSpec {
    pub name: String,
    pub image: String,
    pub gpu_type: String,
    pub gpu_count: u32,
    pub cloud_type: String,
    pub disk_gb: u32,
    pub volume_gb: u32,
    /// e.g. "8888/http,22/tcp"
    pub ports: String,
    pub env: Vec<(String, String)>,
    /// Optional container start command as an argv (RunPod REST `dockerStartCmd`, which
    /// overrides the image's CMD but keeps its ENTRYPOINT). `None` => use the image's own
    /// CMD (correct for the prebuilt arena image, which already starts sshd). `Some(..)`
    /// overrides it — e.g. the `--bootstrap` start script (`["bash","-c", …]`) that installs
    /// + launches sshd on a non-arena base image (NVIDIA NGC, etc.) so the pod is reachable.
    pub docker_args: Option<Vec<String>>,
    /// Host CUDA versions the pod may land on (RunPod `allowedCudaVersions`, e.g. "13.0").
    /// Empty = any host. The cu130 arena image needs a CUDA 13 driver (>= 580).
    pub allowed_cuda: Vec<String>,
}

impl PodSpec {
    /// Build the base spec from config, preferring provider-neutral keys and falling
    /// back to the legacy `RUNPOD_*` names (which is what existing config.env files
    /// have). So a Vast/Hetzner operator can set `GPU_TYPE`/`DISK_GB`/etc. without
    /// touching the RunPod keys, while a pure-RunPod config keeps working unchanged.
    /// `name` is left empty for the caller to fill per machine. `env` is seeded with
    /// `PUBLIC_KEY` (the shared SSH key's public half) so the created pod authorizes it
    /// in `~/.ssh/authorized_keys` — without this, pods we create reject the shared key
    /// and SSH (metrics/backup/setup) fails with "Permission denied".
    pub fn from_config(cfg: &Config) -> PodSpec {
        let first = |keys: &[&str]| keys.iter().find_map(|k| cfg.get(k).filter(|v| !v.is_empty()));
        let first_parsed =
            |keys: &[&str], default| keys.iter().find_map(|k| cfg.get_parsed(k)).unwrap_or(default);
        // RunPod adds PUBLIC_KEY (newline-joined) to ~/.ssh/authorized_keys at boot, so
        // the created pod authorizes the shared key + the deploy key (the account's own
        // key, e.g. arena_admin, is injected by the provider automatically).
        let mut env = Vec::new();
        let pubkeys = crate::ssh::authorized_pubkeys(cfg);
        if !pubkeys.is_empty() {
            env.push(("PUBLIC_KEY".to_string(), pubkeys.join("\n")));
        }
        PodSpec {
            name: String::new(),
            image: first(&["IMAGE", "RUNPOD_DOCKER_IMAGE"]).unwrap_or_default().to_string(),
            gpu_type: first(&["GPU_TYPE", "RUNPOD_GPU_TYPE"]).unwrap_or_default().to_string(),
            gpu_count: first_parsed(&["NUM_GPUS", "RUNPOD_NUM_GPUS"], 1),
            cloud_type: first(&["CLOUD_TYPE", "RUNPOD_CLOUD_TYPE"]).unwrap_or("COMMUNITY").to_string(),
            disk_gb: first_parsed(&["DISK_GB", "RUNPOD_DISK_SPACE_IN_GB"], 100),
            volume_gb: first_parsed(&["VOLUME_GB", "RUNPOD_VOLUME_SPACE_IN_GB"], 0),
            ports: "8888/http,22/tcp".to_string(),
            env,
            docker_args: None,
            allowed_cuda: first(&["ALLOWED_CUDA_VERSIONS", "RUNPOD_ALLOWED_CUDA_VERSIONS"])
                .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default(),
        }
    }
}
