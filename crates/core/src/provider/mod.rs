//! The single abstraction every compute backend implements. Adding Vast.ai (or
//! Lambda, etc.) means writing one more impl of this trait — the CLI/TUI never
//! mention a concrete provider.

use async_trait::async_trait;

use crate::apiextra::{Extra, SOURCES};
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
        "vast" => Ok(Box::new(vast::VastProvider::from_config(cfg)?)),
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

    /// Mutating. On a backend where [`Self::restart_wipes_container_disk`] holds, a
    /// stopped pod's container disk is gone when it starts again (RunPod: "Stopping a pod
    /// keeps no data" — only a volume survives), so callers gate it like a restart.
    async fn stop_pod(&self, id: &str) -> Result<()>;

    /// Mutating, and on most backends DESTRUCTIVE: restart in place (same id, same
    /// machine), for when a pod is wedged. It is *not* a reboot that keeps files: on
    /// RunPod (REST v1 and v2) the container is reset to its image, so the container disk
    /// — everything outside a persistent/network volume: participants' work, `~/.name`,
    /// setup's git remote, distributed keys — is wiped (live-verified on v2, 2026-10-07;
    /// RunPod documents the container disk as ephemeral). Hetzner's hard reset keeps the
    /// VM disk. Callers ask [`Self::restart_wipes_container_disk`] and confirm (and re-run
    /// setup) accordingly.
    async fn restart_pod(&self, id: &str) -> Result<()>;

    /// Whether restarting `pod` — or stopping it and starting it again — resets its
    /// container disk to the image, leaving only a persistent volume (if it has one).
    /// Drives the restart/stop confirm text and the `--wipe-ok` gate.
    ///
    /// Default **true**: a backend that hasn't said otherwise is assumed to wipe, so a new
    /// provider can only err towards one confirmation too many, never towards silent data
    /// loss. Takes the pod (not just `self`) so the fleet provider can route the question
    /// to the backend that owns it, like every per-pod call.
    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        true
    }

    /// Mutating. Start a stopped pod again (same id): RunPod's `start` (REST v1 `POST
    /// /pods/{id}/start`, v2 action `start`), Vast's desired state `running`, Hetzner's
    /// `poweron`. Where [`Self::restart_wipes_container_disk`] holds, the pod comes back as
    /// a fresh image — its container disk went at the stop — so callers re-run setup.
    ///
    /// Separate from [`Self::restart_pod`] because the providers keep them apart: a RunPod
    /// restart needs a running pod (v2 answers 409 for an `EXITED` one; v1 answered 2xx and
    /// left it stopped). Default: unsupported, so a backend that can't start is said, never
    /// silently restarted instead.
    async fn start_pod(&self, _id: &str) -> Result<()> {
        Err(Error::NotImplemented(format!("starting a stopped pod isn't supported on provider `{}`", self.name())))
    }

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

    /// Mutating, idempotent. Authorize `keys` (OpenSSH public keys, one per entry) for SSH on
    /// `pod` through the provider's API — the repair path when a pod refuses the cohort key
    /// (setup and `up` call it once on a `Permission denied (publickey)`, then retry). Only
    /// adds: keys already authorized stay, and so do any others.
    ///
    /// Default: unsupported. RunPod and Hetzner authorize keys only at create (`PUBLIC_KEY`,
    /// the project key), so for them a refused key stays a setup failure for the operator.
    async fn authorize_ssh_keys(&self, _pod: &Pod, _keys: &[String]) -> Result<()> {
        Err(Error::NotImplemented(format!(
            "re-authorizing SSH keys isn't supported on provider `{}`",
            self.name()
        )))
    }

    /// Whether `pod`'s backend can lock and unlock it ([`Self::set_locked`]); `Err` says why
    /// not, in the operator's words. Pure: `pods lock`/`unlock` sort a selection with it
    /// before anything is asked, and `up --lock` checks its create target before creating.
    /// Takes the pod so the fleet provider can route the question to the pod's backend.
    /// Default: unsupported — RunPod REST v2 is the only API here with a pod lock.
    fn lock_support(&self, _pod: &Pod) -> Result<()> {
        Err(lock_unsupported(self.name()))
    }

    /// Mutating, idempotent. Lock (`true`) or unlock a pod in place. A locked pod refuses
    /// stop, restart and terminate — from any client, the provider's console included (see
    /// [`crate::lock`]); nothing else about it changes. Default: unsupported.
    async fn set_locked(&self, _id: &str, _locked: bool) -> Result<()> {
        Err(lock_unsupported(self.name()))
    }

    /// Pure. Check `--api-json` / `CREATE_EXTRA_JSON` against this backend's create request
    /// before anything is listed or created: the fields the tool sets itself are refused
    /// ([`crate::apiextra`]). Default: refused outright — a backend whose create doesn't
    /// merge the extra fields must not silently drop them.
    fn check_create_extra(&self, _extra: &Extra) -> Result<()> {
        Err(Error::Config(format!("{SOURCES} isn't supported on provider `{}`", self.name())))
    }

    /// Pure. The create request body this backend would send for `spec`, with
    /// `spec.api_extra` merged in — for `create`/`up --dry-run` to show exactly what goes
    /// out. Without what the create itself looks up first (RunPod v2's account SSH keys,
    /// Vast's offer). Default: not available.
    fn preview_create_body(&self, _spec: &PodSpec) -> Result<serde_json::Value> {
        Err(Error::NotImplemented(format!("no create-body preview on provider `{}`", self.name())))
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

/// Why a backend can't lock a pod (the [`Provider::lock_support`] default). RunPod pods can
/// be locked only through REST v2; on v1 the RunPod backend says so itself.
pub(crate) fn lock_unsupported(provider: &str) -> Error {
    Error::NotImplemented(format!(
        "not supported on {provider} — only RunPod pods can be locked, with RUNPOD_API=v2"
    ))
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
            max_price: None,
            api_extra: None,
        }
    }

    /// Locking is RunPod v2's alone; every other backend says why not, before any request.
    #[test]
    fn only_runpod_v2_can_lock() {
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        for (name, api, ok, why) in [
            ("runpod", "v2", true, ""),
            ("runpod", "v1", false, "RUNPOD_API=v2"),
            ("vast", "v1", false, "not supported on vast"),
            ("hetzner", "v2", false, "not supported on hetzner"),
        ] {
            let p = build(name, &Config::parse(&format!("{keys}RUNPOD_API={api}"))).unwrap();
            let pod = Pod { provider: name.into(), ..Default::default() };
            match p.lock_support(&pod) {
                Ok(()) => assert!(ok, "{name} {api}"),
                Err(e) => {
                    assert!(!ok, "{name} {api}");
                    assert!(e.to_string().contains(why), "{name} {api}: {e}");
                }
            }
        }
    }

    /// Every backend merges `--api-json` into the body it would send and refuses the name
    /// field it sets itself (each spells it its own way) — checked up front and again when
    /// the body is built.
    #[test]
    fn every_backend_takes_api_extra_and_refuses_its_own_fields() {
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        // (backend, RUNPOD_API, a field it passes through, its name field)
        for (name, api, pass, own) in [
            ("runpod", "v1", "interruptible", "name"),
            ("runpod", "v2", "dataCenterIds", "name"),
            ("vast", "v1", "price", "label"),
            ("hetzner", "v1", "labels", "name"),
        ] {
            let p = build(name, &Config::parse(&format!("{keys}RUNPOD_API={api}"))).unwrap();
            let ok: Extra = serde_json::from_str(&format!(r#"{{"{pass}": {{"x": 1}}}}"#)).unwrap();
            let bad: Extra = serde_json::from_str(&format!(r#"{{"{own}": "other"}}"#)).unwrap();
            p.check_create_extra(&ok).unwrap_or_else(|e| panic!("{name} {api}: {e}"));
            let e = p.check_create_extra(&bad).unwrap_err().to_string();
            assert!(e.contains(&format!("`{own}` is set by arena")), "{name} {api}: {e}");
            let mut s = spec();
            s.api_extra = Some(ok);
            let body = p.preview_create_body(&s).unwrap_or_else(|e| panic!("{name} {api}: {e}"));
            assert_eq!(body[pass], serde_json::json!({"x": 1}), "{name} {api}");
            assert_eq!(body[own], "devtest-apple", "{name} {api}: ours kept");
            s.api_extra = Some(bad);
            assert!(p.preview_create_body(&s).is_err(), "{name} {api}");
        }
    }

    /// The fields each backend sets itself, in its own spelling — the spec's list (name/label,
    /// GPU, tier, image, PUBLIC_KEY, MACHINE_NAME, dropping 22/tcp) plus what would set them
    /// indirectly (a template; v1's CPU compute type) — are refused up front with a config
    /// error naming the field; what the tool doesn't set passes.
    #[test]
    fn each_backend_refuses_its_managed_fields_by_their_own_names() {
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        // (backend, RUNPOD_API, refused extras, accepted extras)
        let cases: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "runpod",
                "v1",
                &[
                    r#"{"name":"x"}"#,
                    r#"{"gpuTypeIds":["NVIDIA H100"]}"#,
                    r#"{"gpuCount":8}"#,
                    r#"{"cloudType":"SECURE"}"#,
                    r#"{"imageName":"x"}"#,
                    r#"{"env":{"PUBLIC_KEY":"k"}}"#,
                    r#"{"env":{"MACHINE_NAME":"x"}}"#,
                    r#"{"ports":["8888/http"]}"#,
                    r#"{"templateId":"t1"}"#,
                    r#"{"computeType":"CPU"}"#,
                    r#"{"locked":true}"#,
                ],
                &[r#"{"dataCenterIds":["EU-RO-1"],"interruptible":true,"env":{"FOO":"1"},"networkVolumeId":"v1"}"#],
            ),
            (
                "runpod",
                "v2",
                &[
                    r#"{"name":"x"}"#,
                    r#"{"gpu":{"id":"NVIDIA H100"}}"#,
                    r#"{"gpu":{"count":8}}"#,
                    r#"{"cloud":"SECURE"}"#,
                    r#"{"image":"x"}"#,
                    r#"{"env":{"PUBLIC_KEY":"k"}}"#,
                    r#"{"env":{"MACHINE_NAME":"x"}}"#,
                    r#"{"ports":["8888/http"]}"#,
                    r#"{"templateId":"t1"}"#,
                    r#"{"locked":true}"#,
                ],
                &[r#"{"dataCenterIds":["EU-RO-1"],"globalNetworking":true,"gpu":{"minRamPerGpu":32},"startJupyter":true}"#],
            ),
            (
                "vast",
                "v1",
                &[
                    r#"{"label":"x"}"#,
                    r#"{"image":"x"}"#,
                    r#"{"env":{"PUBLIC_KEY":"k"}}"#,
                    r#"{"env":{"MACHINE_NAME":"x"}}"#,
                    r#"{"runtype":"jupyter"}"#,
                    r#"{"onstart":"true"}"#,
                    r#"{"template_hash_id":"abc"}"#,
                ],
                &[r#"{"price":0.2,"env":{"FOO":"1"}}"#],
            ),
            (
                "hetzner",
                "v1",
                &[r#"{"name":"x"}"#, r#"{"server_type":"cx53"}"#, r#"{"image":"debian-12"}"#, r#"{"ssh_keys":["other"]}"#],
                &[r##"{"labels":{"cohort":"devtest"},"user_data":"#cloud-config"}"##],
            ),
        ];
        for (name, api, refused, accepted) in cases {
            let p = build(name, &Config::parse(&format!("{keys}RUNPOD_API={api}"))).unwrap();
            for text in *refused {
                let extra: Extra = serde_json::from_str(text).unwrap();
                let e = p.check_create_extra(&extra).unwrap_err();
                assert!(matches!(e, Error::Config(_)), "{name} {api} {text}: {e:?}");
                assert!(e.to_string().contains("set by arena") || e.to_string().contains("22/tcp"), "{name} {api} {text}: {e}");
            }
            for text in *accepted {
                let extra: Extra = serde_json::from_str(text).unwrap();
                p.check_create_extra(&extra).unwrap_or_else(|e| panic!("{name} {api} {text}: {e}"));
            }
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

    /// Restart safety (live finding 2026-10-07): RunPod's restart resets the container to
    /// its image, on both API generations; Vast's stop+start is unverified and treated the
    /// same; only Hetzner's VM disk survives. The CLI/TUI gate restart/stop on this answer.
    #[test]
    fn only_hetzner_keeps_the_disk_across_a_restart() {
        let any = Pod::default();
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        for (name, api, wipes) in [("runpod", "v1", true), ("runpod", "v2", true), ("vast", "v1", true), ("hetzner", "v1", false)] {
            let p = build(name, &Config::parse(&format!("{keys}RUNPOD_API={api}"))).unwrap();
            assert_eq!(p.restart_wipes_container_disk(&any), wipes, "{name} {api}");
        }
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
