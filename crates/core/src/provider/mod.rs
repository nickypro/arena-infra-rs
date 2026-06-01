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
        "vast" => Ok(Box::new(vast::VastProvider::new(
            cfg.get("VAST_API_KEY").unwrap_or_default(),
        ))),
        "hetzner" => {
            let opts = hetzner::HetznerOpts {
                server_type: cfg.get("HETZNER_SERVER_TYPE").unwrap_or("cx22").to_string(),
                image: cfg.get("HETZNER_IMAGE").unwrap_or("ubuntu-24.04").to_string(),
                location: cfg.get("HETZNER_LOCATION").map(String::from),
                ssh_keys: cfg
                    .get("HETZNER_SSH_KEY")
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default(),
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

    /// Mutating and irreversible.
    async fn terminate_pod(&self, id: &str) -> Result<()>;
}
