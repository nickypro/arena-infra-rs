use serde::Serialize;

use crate::config::Config;

/// A provider-agnostic view of a running (or requested) machine.
#[derive(Debug, Clone, Serialize)]
pub struct Pod {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub status: String,
    pub gpu_type: Option<String>,
    pub cost_per_hr: Option<f64>,
    pub ssh_ip: Option<String>,
    pub ssh_port: Option<u16>,
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
        let mut env = Vec::new();
        if let Some(pubkey) = shared_public_key(cfg) {
            env.push(("PUBLIC_KEY".to_string(), pubkey));
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
        }
    }
}

/// The shared SSH key's public half, for injecting as the pod's `PUBLIC_KEY`. Prefers
/// an existing `<key>.pub`; if absent, derives it from the private key via
/// `ssh-keygen -y`. None if no key is configured / it can't be read.
fn shared_public_key(cfg: &Config) -> Option<String> {
    let keypath = cfg.get("SHARED_SSH_KEY_PATH").filter(|s| !s.is_empty())?;
    let resolved = crate::ssh::resolve_key_path(keypath);
    let pubkey = std::fs::read_to_string(format!("{resolved}.pub")).ok().or_else(|| {
        std::process::Command::new("ssh-keygen")
            .args(["-y", "-f", &resolved])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    })?;
    let pubkey = pubkey.trim().to_string();
    (!pubkey.is_empty()).then_some(pubkey)
}
