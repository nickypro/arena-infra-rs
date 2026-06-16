//! The single abstraction every compute backend implements. Adding Vast.ai (or
//! Lambda, etc.) means writing one more impl of this trait — the CLI/TUI never
//! mention a concrete provider.

use async_trait::async_trait;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

pub mod hetzner;
pub mod runpod;
pub mod vast;

/// Construct a provider by name from config. The single place concrete backends are
/// built, so the CLI and TUI share one source of truth (and one list of known names).
pub fn build(name: &str, cfg: &Config) -> Result<Box<dyn Provider>> {
    match name {
        "runpod" => Ok(Box::new(runpod::RunpodProvider::new(cfg.require("RUNPOD_API_KEY")?))),
        "vast" => Ok(Box::new(vast::VastProvider::new(cfg.require("VAST_API_KEY")?))),
        "hetzner" => {
            let opts = hetzner::HetznerOpts {
                // cx23: newest Intel x86 shared gen (the old cx/cax lines had poor
                // availability). Pin an EU location so placement is deterministic — the
                // x86 shared types live only in the EU DCs, so no-location auto-placement
                // can land on a US DC that lacks them and fail with "error during placement".
                server_type: cfg.get("HETZNER_SERVER_TYPE").unwrap_or("cx23").to_string(),
                image: cfg.get("HETZNER_IMAGE").unwrap_or("ubuntu-24.04").to_string(),
                location: Some(cfg.get("HETZNER_LOCATION").unwrap_or("nbg1").to_string()),
                // Universal default: attach the cohort SSH key, named after
                // MACHINE_NAME_PREFIX (e.g. "arena8"), so every hetzner pod is reachable
                // with the same arena key as the GPU fleet — no per-pod config. Upload it
                // once to the Hetzner project under that name (`arena keys`/console).
                // Override/disable via HETZNER_SSH_KEY.
                ssh_keys: cfg
                    .get("HETZNER_SSH_KEY")
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_else(|| {
                        vec![cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string()]
                    }),
            };
            Ok(Box::new(hetzner::HetznerProvider::new(
                cfg.require("HETZNER_API_KEY")?,
                opts,
            )))
        }
        other => Err(Error::Config(format!(
            "unknown provider `{other}` (known: runpod, vast, hetzner)"
        ))),
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// A human-readable, provider-accurate description of what `create_pod` would do
    /// with this spec — used for dry-run output. Each backend describes only the
    /// fields it actually honors (e.g. Hetzner reports its server type/image, not the
    /// GPU fields it ignores), so the dry-run never misrepresents a create.
    fn describe(&self, spec: &PodSpec) -> String;

    /// Read-only. Safe to call freely.
    async fn list_pods(&self) -> Result<Vec<Pod>>;

    /// Mutating. Callers gate this behind an explicit apply/confirm step.
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod>;

    /// Mutating.
    async fn stop_pod(&self, id: &str) -> Result<()>;

    /// Mutating. Restart in place (preserves the machine/disk where the provider
    /// supports it), for when a pod is wedged.
    async fn restart_pod(&self, id: &str) -> Result<()>;

    /// Mutating and irreversible.
    async fn terminate_pod(&self, id: &str) -> Result<()>;
}
