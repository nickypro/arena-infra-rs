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
pub mod runpod_v2;
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

/// The day RunPod retires REST v1 (`rest.runpod.io/v1`, the [`runpod`] backend), per its
/// v1→v2 migration guide. Shown by `config check` while v1 is still selected.
pub const RUNPOD_V1_RETIREMENT: &str = "2026-11-15";

/// Which RunPod REST generation the `runpod` provider talks to, from `RUNPOD_API`.
///
/// v1 stays the default until v2 has been live-tested — then the operator flips it (one
/// config line, or `RUNPOD_API=v2` in the environment), and can flip back just as fast.
/// Both report themselves as provider `runpod`, so pod names, the proxy's provider tags
/// and `--provider runpod` don't change with the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunpodApi {
    V1,
    V2,
}

impl RunpodApi {
    /// Parse `RUNPOD_API`: unset/empty → v1; `v1`/`v2` (any case); anything else is a
    /// config error — never a silent fallback, since a typo would otherwise keep a fleet
    /// on the retiring API (or quietly move it) without anyone noticing.
    ///
    /// The error quotes the bad value only when it's short and version-like (`v3`,
    /// `2.0`): `RUNPOD_API` is one suffix away from `RUNPOD_API_KEY`, so a pasted key is
    /// the likely long value — and this error reaches every fleet command's output, cron
    /// logs and `config check`, which must never show a secret.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        match cfg.get("RUNPOD_API").map(str::trim) {
            None | Some("") => Ok(Self::V1),
            Some(v) if v.eq_ignore_ascii_case("v1") => Ok(Self::V1),
            Some(v) if v.eq_ignore_ascii_case("v2") => Ok(Self::V2),
            Some(other) => {
                let got = if other.len() <= 4 && other.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
                    format!("`{other}`")
                } else {
                    format!("an unrecognised value of {} chars, not shown in case it's a key", other.chars().count())
                };
                Err(Error::Config(format!("RUNPOD_API must be `v1` or `v2` (got {got})")))
            }
        }
    }
}

/// Construct a provider by name from config. The single place concrete backends are
/// built, so the CLI and TUI share one source of truth (and one list of known names).
pub fn build(name: &str, cfg: &Config) -> Result<Box<dyn Provider>> {
    match name {
        "runpod" => {
            let key = cfg.require("RUNPOD_API_KEY")?;
            Ok(match RunpodApi::from_config(cfg)? {
                RunpodApi::V1 => Box::new(runpod::RunpodProvider::new(key)),
                RunpodApi::V2 => Box::new(runpod_v2::RunpodV2Provider::new(key)),
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> PodSpec {
        PodSpec {
            name: "devtest-apple".into(),
            image: "img:1".into(),
            gpu_type: "NVIDIA RTX A4000".into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            disk_gb: 50,
            volume_gb: 0,
            ports: "22/tcp".into(),
            env: Vec::new(),
            docker_args: None,
            allowed_cuda: Vec::new(),
        }
    }

    #[test]
    fn runpod_api_parses_known_values_and_rejects_the_rest() {
        let api = |text: &str| RunpodApi::from_config(&Config::parse(text));
        assert_eq!(api("").unwrap(), RunpodApi::V1); // unset → default
        assert_eq!(api("RUNPOD_API=\"\"").unwrap(), RunpodApi::V1); // empty = unset
        assert_eq!(api("RUNPOD_API=v1").unwrap(), RunpodApi::V1);
        assert_eq!(api("RUNPOD_API=v2").unwrap(), RunpodApi::V2);
        assert_eq!(api("RUNPOD_API=\"V2\" # new backend").unwrap(), RunpodApi::V2);
        for bad in ["RUNPOD_API=v3", "RUNPOD_API=2", "RUNPOD_API=rest", "RUNPOD_API=rpa_SECRETKEY123"] {
            let e = api(bad).unwrap_err();
            assert!(matches!(e, Error::Config(_)), "{bad}: {e}");
            assert!(e.to_string().contains("RUNPOD_API must be `v1` or `v2`"), "{bad}: {e}");
        }
        // A short version-like typo is quoted back; anything else (a pasted key, most
        // likely — it's one suffix from RUNPOD_API_KEY) is described, never echoed.
        assert!(api("RUNPOD_API=v3").unwrap_err().to_string().contains("(got `v3`)"));
        assert!(api("RUNPOD_API=2.0").unwrap_err().to_string().contains("(got `2.0`)"));
        for secret in ["rpa_SECRETKEY123", "abcde", "v 2", "rpa_ÄÖÜ"] {
            let e = api(&format!("RUNPOD_API=\"{secret}\"")).unwrap_err().to_string();
            assert!(!e.contains(secret), "{secret} echoed: {e}");
            assert!(e.contains("unrecognised value of") && e.contains("not shown"), "{e}");
        }
        assert!(api("RUNPOD_API=rpa_SECRETKEY123").unwrap_err().to_string().contains("of 16 chars"));
    }

    /// `build("runpod")` follows RUNPOD_API. Both report as `runpod`; the v2 backend says so
    /// in its dry-run description, which is how an operator (and this test) tells them apart.
    #[test]
    fn build_selects_runpod_backend_by_runpod_api() {
        let v1 = build("runpod", &Config::parse("RUNPOD_API_KEY=k")).unwrap();
        assert_eq!(v1.name(), "runpod");
        assert!(!v1.describe(&spec()).contains("API v2"), "{}", v1.describe(&spec()));
        let v2 = build("runpod", &Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v2")).unwrap();
        assert_eq!(v2.name(), "runpod");
        assert!(v2.describe(&spec()).contains("RunPod API v2"), "{}", v2.describe(&spec()));

        let bad = build("runpod", &Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v3")).err().unwrap();
        assert!(bad.to_string().contains("RUNPOD_API"), "{bad}");
        // No key is still the "not configured" error, whatever RUNPOD_API says.
        let nokey = build("runpod", &Config::parse("RUNPOD_API=v2")).err().unwrap();
        assert!(nokey.to_string().contains("RUNPOD_API_KEY"), "{nokey}");
    }

    /// A bad RUNPOD_API must fail the fleet even when RunPod isn't the primary: secondary
    /// backends are built best-effort (`.ok()`), which would otherwise drop RunPod from the
    /// fleet silently — every RunPod pod vanishing from `pods list` over a typo.
    #[test]
    fn build_fleet_rejects_bad_runpod_api_even_as_a_secondary() {
        let cfg = Config::parse("RUNPOD_API_KEY=k\nHETZNER_API_KEY=h\nRUNPOD_API=v3");
        let e = build_fleet("hetzner", &cfg, false).err().unwrap();
        assert!(e.to_string().contains("RUNPOD_API"), "{e}");
        // The error every fleet command (and cron) prints never echoes a pasted key.
        let pasted = Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=rpa_SECRETKEY123");
        let e = build_fleet("runpod", &pasted, false).err().unwrap().to_string();
        assert!(e.contains("RUNPOD_API must be") && !e.contains("SECRETKEY"), "{e}");
        let ok = Config::parse("RUNPOD_API_KEY=k\nHETZNER_API_KEY=h\nRUNPOD_API=v2");
        assert!(build_fleet("hetzner", &ok, false).is_ok());
    }
}
