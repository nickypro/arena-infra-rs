//! The single abstraction every compute backend implements. Adding Vast.ai (or
//! Lambda, etc.) means writing one more impl of this trait — the CLI/TUI never
//! mention a concrete provider.

use async_trait::async_trait;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

pub mod hetzner;
pub mod multi;
pub mod runpod;
pub mod vast;

pub use multi::build_fleet;

/// A short, char-boundary-safe excerpt of a JSON body for an error message. Used by the
/// list-shape checks: an unexpected 2xx body is reported (bounded — bodies get printed),
/// never silently read as an empty list.
pub(crate) fn body_excerpt(body: &serde_json::Value) -> String {
    const MAX: usize = 120;
    let s = body.to_string();
    if s.chars().count() > MAX {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    } else {
        s
    }
}

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

/// Upper bound on one backend's list call in [`Provider::list_by_provider`]. The HTTP
/// clients have no timeout of their own, so a provider API that accepts the connection
/// and then stalls would hang the caller forever — and a `*/5` `proxy apply` cron would
/// pile up one stuck process per tick. Only *listing* is bounded: it's read-only, so a
/// timeout just reads as "that provider failed to list" (the proxy merge keeps its
/// forwards). Mutating calls deliberately aren't — a create that timed out client-side
/// may still have happened server-side, and the transport-error retry would then make a
/// duplicate (billed) pod.
pub const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Await a backend's list call, failing as that backend's error once `limit` passes.
pub async fn bounded_list<F>(name: &str, list: F, limit: std::time::Duration) -> Result<Vec<Pod>>
where
    F: std::future::Future<Output = Result<Vec<Pod>>>,
{
    match tokio::time::timeout(limit, list).await {
        Ok(res) => res,
        Err(_) => Err(Error::provider(format!("{name} list timed out after {}s", limit.as_secs_f64()))),
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

    /// Read-only. Every backend's *own* listing outcome, as `(provider name, result)`.
    ///
    /// `list_pods` on the fleet provider swallows partial failures (one backend 429ing
    /// just vanishes from the list) — fine for a dashboard, but fatal for anything that
    /// treats "absent from the list" as "terminated": the proxy merge must tell "vast
    /// listed OK without this pod" apart from "vast didn't answer". Default: a single
    /// backend reports its own `list_pods` under its own name.
    ///
    /// Each backend's list is bounded by [`LIST_TIMEOUT`] here (a stall becomes that
    /// backend's `Err`), because these callers include the unattended proxy re-sync.
    async fn list_by_provider(&self) -> Vec<(String, Result<Vec<Pod>>)> {
        vec![(self.name().to_string(), bounded_list(self.name(), self.list_pods(), LIST_TIMEOUT).await)]
    }

    /// Read-only, best-effort. Fill in display details the cheap `list_pods` call leaves
    /// out — RunPod's REST list returns an empty `machine`, so GPU type, gpu count, $/h and
    /// the host's maintenance window come from one extra GraphQL query. Pods this backend
    /// doesn't know are left untouched; on `Err` the pods may be partially filled and
    /// remain valid to display.
    ///
    /// Deliberately separate from `list_pods` (and only called by `pods list`): the TUI
    /// poll loop and every mutating command list pods often, and folding this in would
    /// double their API call volume against rate-limited providers. Default: no-op.
    async fn enrich(&self, _pods: &mut [Pod]) -> Result<()> {
        Ok(())
    }

    /// Mutating. Callers gate this behind an explicit apply/confirm step.
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod>;

    /// Mutating.
    async fn stop_pod(&self, id: &str) -> Result<()>;

    /// Mutating. Restart in place (preserves the machine/disk where the provider
    /// supports it), for when a pod is wedged.
    async fn restart_pod(&self, id: &str) -> Result<()>;

    /// Mutating and irreversible.
    async fn terminate_pod(&self, id: &str) -> Result<()>;

    /// Mutating. Rename a pod in place (the cloud-side `name`). The blue-green `replace`
    /// flow uses this to swap a freshly-built pod into the canonical machine name while
    /// parking the old one under `<name>-old`. The pod's identity/disk/endpoint are
    /// untouched — only the name changes.
    ///
    /// Default: unsupported. A provider whose API can't rename a live pod inherits this,
    /// and `replace` refuses up front rather than half-running the swap.
    async fn rename_pod(&self, _id: &str, _new_name: &str) -> Result<()> {
        Err(Error::NotImplemented(format!(
            "rename not supported on provider `{}`",
            self.name()
        )))
    }

    /// DESTRUCTIVE. Swap a pod's image and replace its env in place (same host, same id).
    /// The container is reset, so its disk is wiped — callers must confirm first.
    /// Default: unsupported.
    async fn reimage_pod(&self, _id: &str, _image: &str, _env: &[(String, String)]) -> Result<()> {
        Err(Error::NotImplemented(format!(
            "reimage not supported on provider `{}`",
            self.name()
        )))
    }

    /// Read-only. Best-effort recovery of the spec needed to recreate this pod — the
    /// "same spec by default" half of `replace`. `name` is returned empty for the caller
    /// to fill; fields the provider can't recover are left at their spec defaults (the
    /// caller layers config + CLI overrides on top). Default: unsupported.
    async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
        Err(Error::NotImplemented(format!(
            "spec snapshot not supported on provider `{}`",
            self.name()
        )))
    }
}
