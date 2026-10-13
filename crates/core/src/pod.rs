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
    /// The provider's id for the physical machine the pod runs on, where it reports one
    /// (Vast's `machine_id`). What names the host for the same-host checks
    /// ([`crate::health::host_key`]) where the IP can't: several Vast machines can sit behind
    /// one public IP, each with its own port range. `None` = not reported. Left out of the
    /// JSON when unknown, so other providers' `pods list --json` is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Whether the provider has the pod **locked** (RunPod REST v2's `locked`; see
    /// [`crate::lock`]): stop, restart and terminate are refused for it by the provider
    /// itself — from any client, RunPod's own console included. `Some(false)` = reported
    /// unlocked; `None` = the backend doesn't report a lock (Vast, Hetzner), which is never
    /// read as "locked". Left out of the JSON when unknown, like `machine_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// The provider reports the pod up — RunPod v2: `RUNNING` with a live `runtime` — yet it
    /// has no direct SSH endpoint, only RunPod's SSH proxy (an interactive shell: no scp,
    /// rsync or port-forwards, so no setup, backups or proxy forward). Live (2026-10-08)
    /// community hosts sat like this for 15+ minutes: no public IP. `up` treats a pod still
    /// like this after a grace period as a host failure; `pods list` says so. Left out of
    /// the JSON unless set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proxy_only: bool,
    /// The pod's disk in GB — container plus volume — where the listing says (RunPod v1
    /// `containerDiskInGb + volumeInGb`, v2 `disk + mounts.persistent.size`, Vast
    /// `disk_space`). What a stopped pod keeps billing for ([`crate::balance`]), read from a
    /// fresh listing only — so it is never written to JSON: `pods list --json` stays byte for
    /// byte what it was before the balance knew disks (a scripting interface), and a JSON
    /// that carries it still reads.
    #[serde(default, skip_serializing)]
    pub disk_gb: Option<f64>,
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
    /// Optional container start command as an argv (RunPod REST v1 `dockerStartCmd` / v2
    /// `cmd`, which override the image's CMD but keep its ENTRYPOINT). `None` => use the
    /// image's own CMD (correct for the prebuilt arena image, which already starts sshd). `Some(..)`
    /// overrides it — e.g. the `--bootstrap` start script (`["bash","-c", …]`) that installs
    /// + launches sshd on a non-arena base image (NVIDIA NGC, etc.) so the pod is reachable.
    pub docker_args: Option<Vec<String>>,
    /// An EXACT set of host CUDA versions the pod may land on (RunPod `allowedCudaVersions`),
    /// from `EXACT_CUDA_VERSIONS` only — opt-in, because RunPod matches it exactly: a host on
    /// a newer CUDA than listed is "no capacity" (live: every B300 MIG host, on 13.2, was
    /// unreachable with "13.0"). Empty = no exact set ([`Self::min_cuda`] applies instead).
    pub allowed_cuda: Vec<String>,
    /// The lowest host CUDA version the pod may land on (`MIN_CUDA_VERSION`, or the lowest of
    /// `ALLOWED_CUDA_VERSIONS`, which is read as a floor): RunPod v2 `gpu.minCudaVersion`,
    /// v1 every version of its schema's list at or above it, Vast `cuda_max_good ≥`. The
    /// cu130 arena image needs a CUDA 13 driver (≥ 580) — "13.0 or newer", not "exactly
    /// 13.0". `None` with an empty `allowed_cuda` = any host. Never set together with
    /// `allowed_cuda` by [`PodSpec::from_config`] (v2 refuses both with a 400).
    pub min_cuda: Option<String>,
    /// `--max-price`: the most this pod may cost, $/h for the whole pod. Placement checks
    /// it against the plan's prices, but only a marketplace backend picks the machine — and
    /// so the price — *at* create: Vast rents the cheapest offer it finds then, which may
    /// cost more than the one the plan quoted, so it must honour the cap itself (RunPod's
    /// price is fixed per GPU type and tier, already checked). `None` = no cap.
    pub max_price: Option<f64>,
    /// `--api-json` / `CREATE_EXTRA_JSON`: extra fields deep-merged into the provider's
    /// create request body, for API options the tool has no flag for (`dataCenterIds`,
    /// `globalNetworking`, a network volume mount…). Each backend refuses the fields it sets
    /// itself before any request ([`crate::apiextra`]). `None` = nothing extra.
    pub api_extra: Option<serde_json::Map<String, serde_json::Value>>,
    /// Machines not to place on, by the provider's machine id (`31540`): the ones an `up
    /// --check` remembers as bad (`snapshot::BAD_HOST_TTL_SECS`). Only a marketplace backend
    /// can honour it — Vast skips their offers; RunPod's create has no such filter, so a pod
    /// RunPod places on one is rejected by `up` and replaced. Empty = any machine.
    pub avoid_machines: Vec<String>,
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
        // key, e.g. arena_admin, is added by RunPod itself on REST v1; the v2 backend
        // merges the account's registered keys in, as v2 skips them when PUBLIC_KEY is set).
        let mut env = Vec::new();
        let pubkeys = crate::ssh::authorized_pubkeys(cfg);
        if !pubkeys.is_empty() {
            env.push(("PUBLIC_KEY".to_string(), pubkeys.join("\n")));
        }
        let cuda = cuda_from_config(cfg);
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
            allowed_cuda: cuda.exact,
            min_cuda: cuda.floor,
            max_price: None,
            api_extra: None,
            avoid_machines: Vec::new(),
        }
    }

    /// The lowest host CUDA version this spec admits: the floor, else the lowest of the exact
    /// set. What a marketplace filters on and what the deep check's driver floor follows.
    pub fn cuda_floor(&self) -> Option<String> {
        self.min_cuda.clone().or_else(|| lowest_cuda(&self.allowed_cuda))
    }

    /// The first CUDA constraint value that isn't a version ([`cuda_key`]), if any. Every
    /// backend refuses to create with one (and the deep check to run): dropping it — as a
    /// marketplace filter reading it as "no floor" would — admits any host, and a cu130 image
    /// on a CUDA 12 host can't use its GPU. Pure.
    pub fn bad_cuda(&self) -> Option<&str> {
        bad_cuda_value(&self.allowed_cuda, self.min_cuda.as_ref())
    }
}

/// The first of `exact` / `floor` that isn't a CUDA version. Pure.
fn bad_cuda_value<'a>(exact: &'a [String], floor: Option<&'a String>) -> Option<&'a str> {
    exact.iter().chain(floor).find(|v| cuda_key(v).is_none()).map(String::as_str)
}

/// The refusal for a CUDA value that isn't a version ([`PodSpec::bad_cuda`]). Pure.
pub fn bad_cuda_error(bad: &str) -> crate::Error {
    crate::Error::Config(format!(
        "CUDA version `{bad}` isn't a version like 13.0 (MIN_CUDA_VERSION / ALLOWED_CUDA_VERSIONS / EXACT_CUDA_VERSIONS) \
         — refusing rather than admitting any host"
    ))
}

/// The CUDA host constraint config asks for (see [`cuda_from_config`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CudaConfig {
    /// `EXACT_CUDA_VERSIONS`: an exact set (opt-in). Empty = none.
    pub exact: Vec<String>,
    /// The floor: `MIN_CUDA_VERSION`, else the lowest of `ALLOWED_CUDA_VERSIONS`. `None`
    /// when an exact set is given (it wins) or nothing is configured.
    pub floor: Option<String>,
    /// Which key the constraint came from — for `config check` and messages.
    pub source: Option<&'static str>,
    /// Every key that was set, the winner first (`config check` flags shadowed ones).
    pub set: Vec<&'static str>,
}

impl CudaConfig {
    /// The first configured value that isn't a CUDA version, if any ([`PodSpec::bad_cuda`]).
    pub fn bad_value(&self) -> Option<&str> {
        bad_cuda_value(&self.exact, self.floor.as_ref())
    }
}

/// The config keys that constrain host CUDA, in precedence order.
pub const CUDA_KEYS: [&str; 4] = ["EXACT_CUDA_VERSIONS", "MIN_CUDA_VERSION", "ALLOWED_CUDA_VERSIONS", "RUNPOD_ALLOWED_CUDA_VERSIONS"];

/// Read the host CUDA constraint from config (finding #10). `ALLOWED_CUDA_VERSIONS` (and the
/// legacy `RUNPOD_ALLOWED_CUDA_VERSIONS`) used to be sent as RunPod's exact
/// `allowedCudaVersions`; it is now a FLOOR — its lowest version — which is what the image
/// needs and what Vast and the deep check already did with it. `MIN_CUDA_VERSION` names the
/// floor outright and wins over it; `EXACT_CUDA_VERSIONS` is the opt-in exact set and wins
/// over both. Pure.
pub fn cuda_from_config(cfg: &Config) -> CudaConfig {
    let list = |k: &str| -> Vec<String> {
        cfg.get(k)
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default()
    };
    let set: Vec<&'static str> = CUDA_KEYS.iter().copied().filter(|k| !list(k).is_empty()).collect();
    let mut out = CudaConfig { set: set.clone(), ..Default::default() };
    // Versions go out as `major.minor` (RunPod's create pattern; `13` → `13.0`). A value that
    // isn't a version is kept as written, so the create refuses it — never silently dropped,
    // which would create on any host (a cu130 image on a CUDA 12 host can't use its GPU).
    let norm = |v: &str| cuda_key(v).map_or_else(|| v.trim().to_string(), |(a, b)| format!("{a}.{b}"));
    let exact = list("EXACT_CUDA_VERSIONS");
    if !exact.is_empty() {
        out.exact = exact.iter().map(|v| norm(v)).collect();
        out.source = Some("EXACT_CUDA_VERSIONS");
        return out;
    }
    for key in ["MIN_CUDA_VERSION", "ALLOWED_CUDA_VERSIONS", "RUNPOD_ALLOWED_CUDA_VERSIONS"] {
        let values = list(key);
        if values.is_empty() {
            continue;
        }
        out.floor = match values.iter().find(|v| cuda_key(v).is_none()) {
            Some(bad) => Some(bad.trim().to_string()),
            None => lowest_cuda(&values).map(|v| norm(&v)),
        };
        out.source = Some(key);
        return out;
    }
    out
}

/// `13.0` → `(13, 0)`; `13` → `(13, 0)`. Compared numerically, so `12.10` is above `12.9`
/// (RunPod's own rule for `minCudaVersion`). `None` for anything else.
pub fn cuda_key(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = match parts.next() {
        Some(m) => m.trim().parse().ok()?,
        None => 0,
    };
    parts.next().is_none().then_some((major, minor))
}

/// The lowest of `versions` by [`cuda_key`] (unparseable entries ignored; none → `None`).
pub fn lowest_cuda(versions: &[String]) -> Option<String> {
    versions.iter().filter_map(|v| cuda_key(v).map(|k| (k, v.trim().to_string()))).min().map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pods list --json` is a scripting interface: what it prints must deserialize back
    /// into the same pods, including the Phase 0.D fields (gpu_count, maintenance).
    #[test]
    fn pod_json_round_trips_with_maintenance() {
        let pods = vec![
            Pod {
                id: "abc123".into(),
                name: "devtest-apple".into(),
                provider: "runpod".into(),
                status: "RUNNING".into(),
                gpu_type: Some("RTX A4000".into()),
                gpu_count: Some(2),
                cost_per_hr: Some(0.34),
                ssh_ip: Some("1.2.3.4".into()),
                ssh_port: Some(10022),
                maintenance: Some(Maintenance {
                    start: Some("2026-10-09T02:00:00Z".into()),
                    end: Some("2026-10-09T06:00:00Z".into()),
                    note: Some("host upgrade".into()),
                }),
                machine_id: None,
                locked: Some(true),
                proxy_only: false,
                disk_gb: None,
            },
            Pod { id: "1".into(), name: "devtest-flutter".into(), provider: "hetzner".into(), ..Default::default() },
        ];
        let json = serde_json::to_string_pretty(&pods).unwrap();
        let back: Vec<Pod> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pods);
        // The schema is the field names scripts key on — pin them.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["gpu_count"], 2);
        assert_eq!(v[0]["maintenance"]["start"], "2026-10-09T02:00:00Z");
        assert!(v[1]["maintenance"].is_null());
        // The lock state rides along where the backend reports it, and is absent (not
        // `false`) where it doesn't — "unknown" must never read as "unlocked" to a script.
        assert_eq!(v[0]["locked"], true);
        assert!(v[1].get("locked").is_none(), "{}", v[1]);
        // The balance's disk size never reaches the JSON (merge of wave 7 balance): the same
        // pods with their disks known print byte for byte the same, and a JSON carrying it
        // (hand-made, or from a later version) still reads.
        let mut disks = pods.clone();
        disks[0].disk_gb = Some(60.0);
        disks[1].disk_gb = Some(0.0);
        assert_eq!(serde_json::to_string_pretty(&disks).unwrap(), json);
        let read: Pod = serde_json::from_str(r#"{"id":"1","name":"n","provider":"vast","status":"EXITED","disk_gb":32.5}"#).unwrap();
        assert_eq!(read.disk_gb, Some(32.5));
    }

    /// Finding #10: `ALLOWED_CUDA_VERSIONS` is a floor (its lowest version), `MIN_CUDA_VERSION`
    /// names one outright, and only `EXACT_CUDA_VERSIONS` asks RunPod for an exact set — never
    /// together with a floor.
    #[test]
    fn cuda_config_is_a_floor_unless_an_exact_set_is_asked_for() {
        let spec = |text: &str| PodSpec::from_config(&Config::parse(text));
        // (config, exact set, floor)
        let cases: [(&str, &[&str], Option<&str>); 9] = [
            ("", &[], None),
            ("ALLOWED_CUDA_VERSIONS=\"13.0\"", &[], Some("13.0")),
            ("ALLOWED_CUDA_VERSIONS=\"13.0, 12.8\"", &[], Some("12.8")),
            ("RUNPOD_ALLOWED_CUDA_VERSIONS=13.0", &[], Some("13.0")),
            ("MIN_CUDA_VERSION=13.0\nALLOWED_CUDA_VERSIONS=12.8", &[], Some("13.0")),
            ("ALLOWED_CUDA_VERSIONS=12.9,12.10", &[], Some("12.9")),
            ("EXACT_CUDA_VERSIONS=\"13.0,13.1,13.2\"", &["13.0", "13.1", "13.2"], None),
            ("EXACT_CUDA_VERSIONS=13.2\nMIN_CUDA_VERSION=13.0", &["13.2"], None),
            ("MIN_CUDA_VERSION=", &[], None),
        ];
        // Sent as major.minor; a value that isn't a version is kept (the create refuses it)
        // rather than dropped, which would admit any host.
        assert_eq!(spec("MIN_CUDA_VERSION=13").min_cuda.as_deref(), Some("13.0"));
        assert_eq!(spec("EXACT_CUDA_VERSIONS=13,12.8").allowed_cuda, ["13.0", "12.8"]);
        assert_eq!(spec("MIN_CUDA_VERSION=newest").min_cuda.as_deref(), Some("newest"));
        assert_eq!(spec("ALLOWED_CUDA_VERSIONS=13.0,x").min_cuda.as_deref(), Some("x"));
        // …and named, for the backends and checks that must refuse it (never read as "no floor").
        assert_eq!(spec("ALLOWED_CUDA_VERSIONS=\"13.0, 13.x\"").bad_cuda(), Some("13.x"));
        assert_eq!(spec("EXACT_CUDA_VERSIONS=13.0,x").bad_cuda(), Some("x"));
        assert_eq!(spec("ALLOWED_CUDA_VERSIONS=13.0,12.8").bad_cuda(), None);
        assert_eq!(cuda_from_config(&Config::parse("MIN_CUDA_VERSION=newest")).bad_value(), Some("newest"));
        assert_eq!(cuda_from_config(&Config::parse("MIN_CUDA_VERSION=13")).bad_value(), None);
        for (text, exact, floor) in cases {
            let s = spec(text);
            assert_eq!(s.allowed_cuda, exact, "{text:?}");
            assert_eq!(s.min_cuda.as_deref(), floor, "{text:?}");
            assert!(s.allowed_cuda.is_empty() || s.min_cuda.is_none(), "never both: {text:?}");
        }
        assert_eq!(spec("EXACT_CUDA_VERSIONS=13.1,13.0").cuda_floor().as_deref(), Some("13.0"));
        let c = cuda_from_config(&Config::parse("EXACT_CUDA_VERSIONS=13.2\nALLOWED_CUDA_VERSIONS=13.0"));
        assert_eq!((c.source, c.set), (Some("EXACT_CUDA_VERSIONS"), vec!["EXACT_CUDA_VERSIONS", "ALLOWED_CUDA_VERSIONS"]));
        assert_eq!(cuda_key("12.10"), Some((12, 10)));
        assert!(cuda_key("12.10") > cuda_key("12.9"));
        assert_eq!(cuda_key("13"), Some((13, 0)));
        assert_eq!(cuda_key("13.0.1"), None);
        assert_eq!(cuda_key("new"), None);
    }

    /// JSON written before gpu_count/maintenance existed must still load (`#[serde(default)]`).
    #[test]
    fn pod_json_without_new_fields_still_deserializes() {
        let old = r#"{"id":"x","name":"n","provider":"runpod","status":"RUNNING",
                      "gpu_type":null,"cost_per_hr":0.2,"ssh_ip":null,"ssh_port":null}"#;
        let p: Pod = serde_json::from_str(old).unwrap();
        assert_eq!(p.gpu_count, None);
        assert_eq!(p.maintenance, None);
        assert_eq!(p.locked, None);
        assert_eq!(p.cost_per_hr, Some(0.2));
    }
}
