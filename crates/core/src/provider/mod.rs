//! The single abstraction every compute backend implements. Adding Vast.ai (or
//! Lambda, etc.) means writing one more impl of this trait — the CLI/TUI never
//! mention a concrete provider.

use async_trait::async_trait;

use crate::apiextra::{Extra, SOURCES};
use crate::config::Config;
use crate::error::{Error, ProviderErrorKind as ErrorKind, Result};
use crate::pod::{Pod, PodSpec};

pub mod hetzner;
pub mod multi;
pub mod runpod;
pub mod runpod_v2;
pub mod shadeform;
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

/// The day RunPod sunsets REST v1 (`rest.runpod.io/v1`, the [`runpod`] backend): a Monday,
/// per its service email of 2026-10-28 (the migration guide had said the 15th). From then
/// every v1 request answers `410 Gone` (`runpod::v1_error` says to leave v1). `config check`
/// warns with it while v1 is still selected.
pub const RUNPOD_V1_RETIREMENT: &str = "2026-11-16";

/// What to tell an operator whose config still says `RUNPOD_API=v1`, wherever v1 is the
/// reason something can't be done (a CUDA floor above 13.0, a GPU v1's create rejects,
/// locks, the reaper). v2 is the default, so the fix is deleting the line, not setting one.
pub const RUNPOD_V1_REMEDY: &str = "remove RUNPOD_API=v1 (v2 is the default)";

/// Which RunPod REST generation the `runpod` provider talks to, from `RUNPOD_API`.
///
/// v2 is the default. RunPod retires REST v1 on [`RUNPOD_V1_RETIREMENT`], and v1 can't
/// express a host CUDA floor: its create takes an exact `allowedCudaVersions` list whose
/// schema stops at 13.0, so a v1 fleet can only ask for hosts on CUDA ≤ 13.0 — every
/// 13.1+ host is out of reach, which starved RunPod placement ("no capacity" all day,
/// 2026-10-19). v2 sends a real floor (`gpu.minCudaVersion`). `RUNPOD_API=v1` still
/// selects v1 until the retirement (`config check` warns); one config line, or
/// `RUNPOD_API=v1` in the environment for a single command.
/// Both report themselves as provider `runpod`, so pod names, the proxy's provider tags
/// and `--provider runpod` don't change with the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunpodApi {
    V1,
    V2,
}

impl RunpodApi {
    /// Parse `RUNPOD_API`: unset/empty → v2 (the default, see [`RunpodApi`]); `v1`/`v2`
    /// (any case); anything else is a config error — never a silent fallback, since a
    /// typo would otherwise quietly pick an API nobody chose.
    ///
    /// The error quotes the bad value only when it's short and version-like (`v3`,
    /// `2.0`): `RUNPOD_API` is one suffix away from `RUNPOD_API_KEY`, so a pasted key is
    /// the likely long value — and this error reaches every fleet command's output, cron
    /// logs and `config check`, which must never show a secret.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        match cfg.get("RUNPOD_API").map(str::trim) {
            None | Some("") => Ok(Self::V2),
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
        "hetzner" => Ok(Box::new(hetzner::HetznerProvider::new(
            cfg.require("HETZNER_API_KEY")?,
            hetzner::HetznerOpts::from_config(cfg),
        ))),
        // Opt-in overflow only (`--provider shadeform`): never in the default plan chain.
        "shadeform" => Ok(Box::new(shadeform::ShadeformProvider::from_config(cfg)?)),
        other => Err(Error::Config(format!(
            "unknown provider `{other}` (known: runpod, vast, hetzner, shadeform)"
        ))),
    }
}

/// What a restart is known to do to a pod's container disk ([`Provider::restart_resets_container`]).
/// Three answers, not a bool, because "gate it as a wipe" and "say it was wiped" are two
/// different claims: a backend can be worth gating without being known to reset (RunPod v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerReset {
    /// It resets the container to its image (verified): only a volume survives.
    Yes,
    /// It keeps the disk (a Hetzner VM).
    No,
    /// Not known to reset every time: gated and set up again as if it did — the safe side —
    /// but nothing may say it happened.
    Unknown,
}

impl ContainerReset {
    /// Whether the work outside a volume may be gone afterwards: what the `--wipe-ok` gate
    /// and the re-setup go by.
    pub fn may_wipe(self) -> bool {
        self != Self::No
    }
}

/// How a stop ended ([`Provider::stop_pod_outcome`]), for the line that reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Stopped the way the backend stops a pod (a Hetzner VM: it shut itself down).
    Stopped,
    /// The VM didn't shut down within `waited`, so it was powered off (Hetzner): what it
    /// hadn't written to disk yet may be lost.
    ForcedOff { waited: std::time::Duration },
}

impl StopOutcome {
    /// What the operator must hear about this stop beyond "stopped", if anything.
    pub fn note(self) -> Option<String> {
        match self {
            StopOutcome::Stopped => None,
            StopOutcome::ForcedOff { waited } => Some(format!(
                "it didn't shut down within {}s, so it was powered off — writes not yet on its disk may be lost",
                waited.as_secs()
            )),
        }
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

/// Await a backend's list call, failing as that backend's error once `limit` passes — an
/// [`Error::Timeout`] (`vast list timed out after 60s`): never retried in place (it already
/// took the whole bound), but told apart from a refusal, so a retry round can wait out a
/// stalled backend as it does a throttled one ([`crate::placement::relist`]). Its
/// rate-limit waits get all but a margin of it ([`crate::ratelimit::bounded`]): a quota held
/// for longer fails it as rate-limited, saying how long, not as a timeout.
pub async fn bounded_list<F>(name: &str, list: F, limit: std::time::Duration) -> Result<Vec<Pod>>
where
    F: std::future::Future<Output = Result<Vec<Pod>>>,
{
    match crate::ratelimit::bounded(limit, list).await {
        Ok(res) => res,
        Err(_) => Err(Error::Timeout { what: format!("{name} list"), after: limit }),
    }
}

/// Every configured backend's pods — or, if **any** backend didn't answer, an error naming
/// it. For the callers that decide a machine name is free: create/up's name allocation
/// (`-n`/`-a` and explicit names), placement's retry rounds, rename.
///
/// The fleet provider's `list_pods` skips a backend that failed (one warning line) — right
/// for a dashboard, wrong here: "not in a listing Vast wasn't in" is not "free". Live, a bad
/// `VAST_API_KEY` had `create delta` plan a second `devtest-delta` on RunPod next to Vast's
/// running one (a duplicate name, billed twice), and `up -n 4` count a 4-pod fleet as 3. So
/// this fails closed on one failed backend. The error is retryable (for
/// [`crate::retry::retrying`]) only when every failure is — a 429/5xx passes; a rejected key
/// doesn't, and the message then points at that backend's key.
pub async fn list_complete(provider: &dyn Provider) -> Result<Vec<Pod>> {
    let mut pods = Vec::new();
    let mut failed: Vec<(String, Error)> = Vec::new();
    for (name, listed) in provider.list_by_provider().await {
        match listed {
            Ok(p) => pods.extend(p),
            Err(e) => failed.push((name, e)),
        }
    }
    if failed.is_empty() {
        return Ok(pods);
    }
    Err(partial_listing_error(&failed))
}

/// The refusal [`list_complete`] fails with: which backends didn't answer and why, what that
/// means (no name can be known free), and what to do. Pure.
pub(crate) fn partial_listing_error(failed: &[(String, Error)]) -> Error {
    let names: Vec<&str> = failed.iter().map(|(n, _)| n.as_str()).collect();
    let why: Vec<String> = failed.iter().map(|(n, e)| format!("{n}: {e}")).collect();
    let keys: Vec<&str> = failed
        .iter()
        .filter(|(_, e)| e.kind() == Some(ErrorKind::Auth))
        .filter_map(|(n, _)| crate::teardown::PROVIDER_KEYS.iter().find(|(p, _)| p == n).map(|(_, k)| *k))
        .collect();
    let fix = if keys.is_empty() {
        format!("retry once {} answers again (`arena pods list` warns while it doesn't)", names.join(" and "))
    } else {
        format!(
            "{} was rejected — fix it (or remove it from the config to leave that provider out of the fleet), then retry",
            keys.join(" / ")
        )
    };
    let retryable = failed.iter().all(|(_, e)| crate::retry::is_retryable(e));
    Error::Provider {
        kind: if retryable { ErrorKind::Transient } else { ErrorKind::Other },
        message: format!(
            "{} failed to list ({}) — refusing to pick machine names from a partial fleet listing: a pod \
             there may already hold the name, and creating it again would duplicate it (billed twice). {fix}",
            names.join(", "),
            why.join("; ")
        ),
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

    /// Mutating: [`Self::stop_pod`], saying how it ended — what `pods stop` and the TUI
    /// report. Default: the backend's stop, [`StopOutcome::Stopped`]. Hetzner overrides it:
    /// its stop is a graceful shutdown that falls back to a power-off, and the fallback must
    /// be said (it can lose writes).
    async fn stop_pod_outcome(&self, id: &str) -> Result<StopOutcome> {
        self.stop_pod(id).await.map(|()| StopOutcome::Stopped)
    }

    /// Mutating, and on most backends DESTRUCTIVE: restart in place (same id, same
    /// machine), for when a pod is wedged. Don't count on it keeping files: on RunPod REST
    /// v2 the container is reset to its image, so the container disk — everything outside a
    /// persistent/network volume: participants' work, `~/.name`, setup's git remote,
    /// distributed keys — is wiped (live-verified 2026-10-07 and 2026-10-09; RunPod
    /// documents the container disk as ephemeral). REST v1's restart was seen keeping it
    /// (twice, 2026-10-09), so it may or may not. Hetzner's hard reset keeps the VM disk.
    /// Callers ask [`Self::restart_resets_container`] and confirm, re-run setup and word
    /// their output accordingly.
    async fn restart_pod(&self, id: &str) -> Result<()>;

    /// Whether restarting `pod` — or stopping it and starting it again — MAY reset its
    /// container disk to the image, leaving only a persistent volume (if it has one).
    /// Drives the stop/start confirm text and the `--wipe-ok` gate; for a restart, what is
    /// known (and so may be said) is [`Self::restart_resets_container`].
    ///
    /// Default **true**: a backend that hasn't said otherwise is assumed to wipe, so a new
    /// provider can only err towards one confirmation too many, never towards silent data
    /// loss. Takes the pod (not just `self`) so the fleet provider can route the question
    /// to the backend that owns it, like every per-pod call.
    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        true
    }

    /// What a restart of `pod` is known to do to its container disk: drives the restart's
    /// `--wipe-ok` gate and re-setup ([`ContainerReset::may_wipe`]) and what the restart says
    /// afterwards — "reset to the image" only on [`ContainerReset::Yes`].
    ///
    /// Default: [`ContainerReset::Unknown`] where [`Self::restart_wipes_container_disk`]
    /// holds (gated, never claimed), [`ContainerReset::No`] where it doesn't — so the two
    /// can't disagree about the gate unless a backend overrides one of them.
    fn restart_resets_container(&self, pod: &Pod) -> ContainerReset {
        if self.restart_wipes_container_disk(pod) {
            ContainerReset::Unknown
        } else {
            ContainerReset::No
        }
    }

    /// Whether `pod`'s backend can stop it (and start it again) at all; `Err` says why not, in
    /// the operator's words. Pure: `pods stop`/`start` refuse a selection with such a pod up
    /// front, before any gate or prompt — a `--wipe-ok` gate would only mislead about a pod
    /// that can't be stopped. Default: it can (RunPod, Vast, Hetzner). Shadeform has no stop:
    /// an instance bills until it's deleted.
    fn stop_support(&self, _pod: &Pod) -> Result<()> {
        Ok(())
    }

    /// How long a pod this backend just created may take to come up — listed with an SSH
    /// endpoint — before `pods up` counts it as a failed placement: terminated (it bills, and
    /// nothing is on it yet) and the name tried again on the next option. `None` (the default):
    /// `up --timeout` bounds the wait, and a pod still without an endpoint then is left running
    /// for a look. Shadeform: `SHADEFORM_READY_MINS` — its instances can sit in a pending state
    /// for long, billed or not (unverified).
    fn ready_deadline(&self, _pod: &Pod) -> Option<std::time::Duration> {
        None
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

    /// Mutating and irreversible: [`Self::terminate_pod`] of the pod `id` that the backend
    /// named `provider` listed — routed by that NAME only, never guessed from the id. The
    /// auto-reaper's one way to terminate ([`crate::reap::ReapTicket`]): Vast and Hetzner ids
    /// are both numbers, and the fleet provider's id → backend cache could hand one backend's
    /// id to the other, terminating a different machine. Default: this backend's own pods
    /// only; any other name is refused without a request.
    async fn terminate_pod_on(&self, provider: &str, id: &str) -> Result<()> {
        if provider == self.name() {
            self.terminate_pod(id).await
        } else {
            Err(Error::provider(format!("pod {id} is on `{provider}`, not `{}` — not terminated", self.name())))
        }
    }

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
/// be locked only through REST v2 (the default); on v1 the RunPod backend says so itself.
pub(crate) fn lock_unsupported(provider: &str) -> Error {
    Error::NotImplemented(format!(
        "not supported on {provider} — only RunPod pods can be locked (on RunPod REST v2, the default)"
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
            min_cuda: None,
            max_price: None,
            api_extra: None,
            avoid_machines: Vec::new(),
            server_type: None,
        }
    }

    /// Locking is RunPod v2's alone (the default: an empty `RUNPOD_API` locks too); every other
    /// backend says why not, before any request — v1 by saying how to get back to v2.
    #[test]
    fn only_runpod_v2_can_lock() {
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        for (name, api, ok, why) in [
            ("runpod", "v2", true, ""),
            ("runpod", "", true, ""),
            ("runpod", "v1", false, RUNPOD_V1_REMEDY),
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

    /// Only a forced power-off has something to add to "stopped" — and says what it risked.
    #[test]
    fn only_a_forced_stop_has_a_note() {
        assert_eq!(StopOutcome::Stopped.note(), None);
        let n = StopOutcome::ForcedOff { waited: std::time::Duration::from_secs(90) }.note().unwrap();
        assert!(n.contains("didn't shut down within 90s") && n.contains("powered off") && n.contains("may be lost"), "{n}");
    }

    #[test]
    fn runpod_api_parses_known_values_and_rejects_the_rest() {
        let api = |text: &str| RunpodApi::from_config(&Config::parse(text));
        assert_eq!(api("").unwrap(), RunpodApi::V2); // unset → default
        assert_eq!(api("RUNPOD_API=\"\"").unwrap(), RunpodApi::V2); // empty = unset
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

    /// Restart safety (live findings 2026-10-07 and 2026-10-09): RunPod v2's restart resets
    /// the container to its image; v1's was seen keeping it, so it's gated as a wipe but
    /// never said to be one; Vast's stop+start is unverified and treated the same; only
    /// Hetzner's VM disk survives. The CLI/TUI gate restart/stop on these answers — and the
    /// two never disagree about the gate.
    #[test]
    fn only_hetzner_keeps_the_disk_across_a_restart() {
        use ContainerReset::{No, Unknown, Yes};
        let any = Pod::default();
        let keys = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\n";
        for (name, api, wipes, resets) in [
            ("runpod", "v1", true, Unknown),
            ("runpod", "v2", true, Yes),
            ("runpod", "", true, Yes), // the default is v2: a restart resets the container
            ("vast", "v1", true, Unknown),
            ("hetzner", "v1", false, No),
        ] {
            let p = build(name, &Config::parse(&format!("{keys}RUNPOD_API={api}"))).unwrap();
            assert_eq!(p.restart_wipes_container_disk(&any), wipes, "{name} {api}");
            assert_eq!(p.restart_resets_container(&any), resets, "{name} {api}");
            assert_eq!(resets.may_wipe(), wipes, "{name} {api}: the gate is the same either way");
        }
    }

    /// `build("runpod")` follows RUNPOD_API, v2 when it's unset. Both report as `runpod`;
    /// the v2 backend says so in its dry-run description, which is how an operator (and this
    /// test) tells them apart.
    #[test]
    fn build_selects_runpod_backend_by_runpod_api() {
        let default = build("runpod", &Config::parse("RUNPOD_API_KEY=k")).unwrap();
        assert_eq!(default.name(), "runpod");
        assert!(default.describe(&spec()).contains("RunPod API v2"), "{}", default.describe(&spec()));
        let v1 = build("runpod", &Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v1")).unwrap();
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

    /// REST v2 is the default (RunPod retires v1 on 2026-11-16, and v1 can't ask for a host on
    /// CUDA 13.1+): a config without `RUNPOD_API` — or with it empty or blank, as a
    /// commented-out template line leaves it — builds the v2 backend, its creates sending a
    /// real CUDA floor. An explicit `v1` (any case) still builds v1 until the retirement;
    /// `config check` warns about it (and `config`'s tests cover the environment's say).
    #[test]
    fn unset_runpod_api_means_v2_and_an_explicit_v1_still_works() {
        let mut floor = spec();
        floor.min_cuda = Some("13.1".into());
        for text in ["RUNPOD_API_KEY=k", "RUNPOD_API_KEY=k\nRUNPOD_API=", "RUNPOD_API_KEY=k\nRUNPOD_API=\"  \""] {
            let cfg = Config::parse(text);
            assert_eq!(RunpodApi::from_config(&cfg).unwrap(), RunpodApi::V2, "{text:?}");
            let p = build("runpod", &cfg).unwrap();
            assert!(p.describe(&spec()).contains("RunPod API v2"), "{text:?}: {}", p.describe(&spec()));
            let body = p.preview_create_body(&floor).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert_eq!(body["gpu"]["minCudaVersion"], "13.1", "{text:?}: a real floor");
            assert!(p.lock_support(&Pod { provider: "runpod".into(), ..Default::default() }).is_ok(), "{text:?}");
            assert!(build_fleet("runpod", &cfg, false).is_ok(), "{text:?}");
        }
        for text in ["RUNPOD_API_KEY=k\nRUNPOD_API=v1", "RUNPOD_API_KEY=k\nRUNPOD_API=\"V1\""] {
            let cfg = Config::parse(text);
            assert_eq!(RunpodApi::from_config(&cfg).unwrap(), RunpodApi::V1, "{text:?}");
            let p = build("runpod", &cfg).unwrap();
            assert!(!p.describe(&spec()).contains("API v2"), "{text:?}: {}", p.describe(&spec()));
            let e = p.preview_create_body(&floor).unwrap_err().to_string();
            assert!(e.contains("can't ask for it") && e.contains(RUNPOD_V1_REMEDY), "{text:?}: {e}");
        }
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
