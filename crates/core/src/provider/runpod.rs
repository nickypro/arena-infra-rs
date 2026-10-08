//! RunPod backend, talking to the REST API (https://rest.runpod.io/v1), plus GraphQL for
//! the few things REST can't do (restart-free rename, GPU catalog/prices, host details).
//!
//! RunPod retires REST v1 on 2026-11-15; its replacement is [`super::runpod_v2`], picked
//! with `RUNPOD_API=v2`. Of the GraphQL helpers here only the pod details (maintenance
//! windows) are shared with it, plus the network-volume listing as its fallback — v2 renames
//! and reads the GPU catalog over REST (see "GraphQL dependency inventory" in `runpod_v2`).
//!
//! Responses are parsed defensively from `serde_json::Value` so a schema tweak on
//! RunPod's side degrades a field to `None` rather than crashing the tool.

use std::collections::HashMap;

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::Provider;
use crate::error::{Error, ProviderErrorKind, Result};
use crate::http::{send_json, send_ok};
use crate::pod::{Maintenance, Pod, PodSpec};

const BASE: &str = "https://rest.runpod.io/v1";

/// RunPod's GraphQL endpoint. The key goes in the `Authorization: Bearer` header — NEVER
/// the `?api_key=` query param RunPod also accepts: reqwest's error `Display` includes the
/// request URL, so with the key in the URL any network error (timeout, DNS, TLS, a bad
/// JSON body) would print the secret verbatim to the terminal/cron logs.
const GRAPHQL: &str = "https://api.runpod.io/graphql";

/// Build a GraphQL POST (bearer auth, key never in the URL). Separate from sending so a
/// test can assert on the built request without a network call.
fn graphql_request(client: &Client, api_key: &str, body: &Value) -> RequestBuilder {
    client.post(GRAPHQL).bearer_auth(api_key).json(body)
}

/// POST one GraphQL request and return the decoded body. Checks only the HTTP status
/// (first, before decoding — see [`crate::http`]): a GraphQL error still comes back as HTTP
/// 200, and whether `errors` is fatal depends on the caller (a mutation must fail on any; a
/// read query can use partial `data`) — see [`graphql_errors`].
async fn graphql(client: &Client, api_key: &str, body: &Value, ctx: &str) -> Result<Value> {
    send_json(graphql_request(client, api_key, body), ctx).await
}

/// The GraphQL-level errors in a response, as one message (`None` when there are none —
/// absent, `null`, or an empty array). Messages are joined; anything unexpected is shown
/// raw, clipped, so a long error body can't flood the terminal.
fn graphql_errors(v: &Value) -> Option<String> {
    let errors = v.get("errors").filter(|e| !e.is_null())?;
    if errors.as_array().is_some_and(|a| a.is_empty()) {
        return None;
    }
    let msgs: Vec<&str> = errors
        .as_array()
        .map(|a| a.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    let msg = if msgs.is_empty() { errors.to_string() } else { msgs.join("; ") };
    Some(if msg.chars().count() > 300 { format!("{}…", msg.chars().take(300).collect::<String>()) } else { msg })
}

pub struct RunpodProvider {
    api_key: String,
    client: Client,
    /// The REST base URL: [`BASE`], or a loopback test server (see `with_base`).
    base: String,
}

impl RunpodProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
        }
    }

    /// Point the REST calls at `base` with `client` — tests only, so the real
    /// request/response path runs against a loopback server.
    #[cfg(test)]
    fn with_base(api_key: &str, base: &str, client: Client) -> Self {
        Self { api_key: api_key.into(), client, base: base.to_string() }
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.api_key)
    }
}

/// Build the REST v1 `POST /pods` body from a spec. Pure (no I/O) so it's unit-testable —
/// the field names must match the REST schema exactly (a stray key is a 400, e.g. the old
/// GraphQL `dockerArgs` which REST rejects in favor of the `dockerStartCmd` argv).
fn create_payload(spec: &PodSpec) -> Value {
    let env: serde_json::Map<String, Value> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let ports: Vec<String> = spec.ports.split(',').map(|s| s.trim().to_string()).collect();
    let mut payload = json!({
        "name": spec.name,
        "imageName": spec.image,
        "gpuTypeIds": [spec.gpu_type],
        "gpuCount": spec.gpu_count,
        "cloudType": spec.cloud_type,
        "containerDiskInGb": spec.disk_gb,
        "volumeInGb": spec.volume_gb,
        "ports": ports,
        "env": env,
    });
    // Only send a container start command when one was requested (`--bootstrap`). REST v1
    // takes `dockerStartCmd` as an argv array (overrides the image's CMD, keeps its
    // ENTRYPOINT). Omitting it lets the image's own CMD run — which is what the prebuilt
    // arena image needs (it starts sshd itself).
    if let Some(args) = &spec.docker_args {
        payload["dockerStartCmd"] = Value::Array(args.iter().map(|s| Value::String(s.clone())).collect());
    }
    if !spec.allowed_cuda.is_empty() {
        payload["allowedCudaVersions"] = json!(spec.allowed_cuda);
    }
    payload
}

fn parse_pod(v: &Value) -> Pod {
    let str_at = |keys: &[&[&str]]| -> Option<String> {
        for path in keys {
            let mut cur = v;
            let mut ok = true;
            for k in *path {
                match cur.get(k) {
                    Some(next) => cur = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some(s) = cur.as_str() {
                    return Some(s.to_string());
                }
            }
        }
        None
    };

    // SSH endpoint: IP is top-level `publicIp`; `portMappings` is an object that
    // maps container port -> public port, e.g. {"22": 1118}.
    let ssh_ip = v.get("publicIp").and_then(Value::as_str).map(String::from);
    let ssh_port = v
        .get("portMappings")
        .and_then(|m| m.get("22"))
        .and_then(Value::as_i64)
        .map(|n| n as u16);

    Pod {
        id: str_at(&[&["id"]]).unwrap_or_default(),
        name: str_at(&[&["name"]]).unwrap_or_default(),
        provider: "runpod".into(),
        status: str_at(&[&["desiredStatus"], &["status"]]).unwrap_or_else(|| "UNKNOWN".into()),
        // GPU type id is not returned in the list view (`machine` is empty there);
        // these paths populate it on create / detailed responses.
        gpu_type: str_at(&[&["machine", "gpuDisplayName"], &["machine", "gpuType"], &["gpuTypeId"]]),
        gpu_count: v.get("gpuCount").and_then(Value::as_u64).map(|n| n as u32),
        cost_per_hr: v.get("costPerHr").and_then(Value::as_f64),
        ssh_ip,
        ssh_port,
        maintenance: None,
        machine_id: None,
    }
}

/// The pod objects in a 2xx `GET /pods` body: a top-level array (REST v1) or a `pods`
/// array. Any other shape is schema drift and must be an error — NOT an empty list: the
/// proxy merge reads "listed OK without pod X" as "X was terminated", so an
/// empty-by-accident list would drop every RunPod forward at once. Fail closed.
fn pods_array(body: &Value) -> Result<&Vec<Value>> {
    body.as_array()
        .or_else(|| body.get("pods").and_then(Value::as_array))
        .ok_or_else(|| {
            Error::provider(format!(
                "list pods: unexpected response shape (no pod array): {}",
                super::body_excerpt(body)
            ))
        })
}

#[async_trait]
impl Provider for RunpodProvider {
    fn name(&self) -> &'static str {
        "runpod"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        describe_spec(spec)
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        // Status first, then decode (crate::http): a bad key's 401 must read as Auth, not
        // as "error decoding response body".
        let body = send_json(self.auth(self.client.get(format!("{}/pods", self.base))), "list pods").await?;
        Ok(pods_array(&body)?.iter().map(parse_pod).collect())
    }

    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        enrich_via_graphql(&self.client, &self.api_key, pods).await
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let payload = create_payload(spec);
        let rb = self.auth(self.client.post(format!("{}/pods", self.base)).json(&payload));
        let body = send_json(rb, "create pod").await?;
        Ok(parse_pod(&body))
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        send_ok(self.auth(self.client.post(format!("{}/pods/{id}/stop", self.base))), "stop pod").await
    }

    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        true // the container is reset to its image; only a volume survives (see restart_pod)
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // Dedicated restart endpoint: restarts the container on the same pod/machine
        // (same id and GPU), unlike stop/start which deallocates. It does NOT keep the
        // container disk: the container is reset to its image, so everything outside a
        // volume is wiped (RunPod documents the container disk as ephemeral; the same
        // action on v2 was live-verified to drop ~/.name and setup's git remote).
        send_ok(self.auth(self.client.post(format!("{}/pods/{id}/restart", self.base))), "restart pod").await
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/pods/{id}", self.base))), "terminate pod").await
    }

    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        rename_via_graphql(&self.client, &self.api_key, id, new_name).await
    }

    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> Result<()> {
        // REST `PATCH /pods/{id}` resets the container (see `rename_pod`): exactly what a
        // reimage wants. `env` replaces the pod's env wholesale, so pass everything to keep.
        let body = reimage_payload(image, env);
        send_ok(self.auth(self.client.patch(format!("{}/pods/{id}", self.base))).json(&body), "reimage pod").await
    }

    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        let body = send_json(self.auth(self.client.get(format!("{}/pods/{id}", self.base))), "get pod").await?;
        Ok(parse_spec(&body))
    }
}

/// The dry-run description of a RunPod create (both API generations take the same spec).
pub(super) fn describe_spec(spec: &PodSpec) -> String {
    let volume = if spec.volume_gb > 0 {
        format!(", volume {}GB", spec.volume_gb)
    } else {
        String::new()
    };
    let bootstrap = if spec.docker_args.is_some() { ", +bootstrap start script" } else { "" };
    let cuda = if spec.allowed_cuda.is_empty() {
        String::new()
    } else {
        format!(", CUDA {}", spec.allowed_cuda.join("/"))
    };
    format!(
        "{} x{}, {}, disk {}GB{volume}, image {}{bootstrap}{cuda}",
        spec.gpu_type, spec.gpu_count, spec.cloud_type, spec.disk_gb, spec.image
    )
}

/// Fill GPU type/count, $/h and the host's maintenance window from one GraphQL query (see
/// [`Provider::enrich`]). Shared with the v2 backend: REST v2 reports GPU and cost itself
/// but has no maintenance field, so GraphQL stays the only source of the window.
pub(super) async fn enrich_via_graphql(client: &Client, api_key: &str, pods: &mut [Pod]) -> Result<()> {
    if pods.is_empty() {
        return Ok(()); // nothing to fill: don't spend an API call
    }
    let v = graphql(client, api_key, &json!({ "query": POD_DETAILS_QUERY }), "pod details").await?;
    // Merge whatever `data` came back even alongside errors (GraphQL partial results),
    // then still report the errors so `pods list` can warn that details are incomplete.
    merge_pod_details(pods, &parse_pod_details(&v));
    match graphql_errors(&v) {
        Some(e) => Err(Error::provider(format!("pod details: {e}"))),
        None => Ok(()),
    }
}

/// Rename a pod in place with the GraphQL `podEditName` mutation — the one the RunPod
/// *dashboard* uses for a rename — NOT the REST `PATCH /pods/{id}`. The v1 REST update is
/// documented as "Update a Pod, potentially triggering a reset", and empirically it RESTARTS
/// the container, which wipes the container disk (everything not on a network volume) —
/// i.e. silent data loss. `podEditName` is a pure metadata rename: verified (via `/proc/1`
/// start time before/after) to leave the running container completely untouched. v1 only:
/// v2 renames with a name-only REST PATCH, live-verified restart-free (`runpod_v2`).
async fn rename_via_graphql(client: &Client, api_key: &str, id: &str, new_name: &str) -> Result<()> {
    let body = json!({
        "query": "mutation editPodName($input: PodEditNameInput!) { \
                  podEditName(input: $input) { id name } }",
        "variables": { "input": { "podId": id, "name": new_name } },
    });
    let v = graphql(client, api_key, &body, "rename pod").await?;
    // GraphQL returns HTTP 200 even on logical errors — a mutation must surface any.
    if let Some(errors) = graphql_errors(&v) {
        return Err(Error::provider(format!("rename pod (podEditName): {errors}")));
    }
    Ok(())
}

/// Build the REST `PATCH /pods/{id}` body for a reimage. Pure, for testing.
fn reimage_payload(image: &str, env: &[(String, String)]) -> Value {
    let env: serde_json::Map<String, Value> =
        env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    json!({ "imageName": image, "env": env })
}

/// Recover a recreate-able `PodSpec` from a RunPod `GET /pods/{id}` body. Pure (no I/O) so
/// it's unit-testable. Defensive: any field RunPod doesn't return is left blank/default for
/// the caller's config/CLI overrides to fill. In practice the REST API reliably returns
/// `imageName`, `containerDiskInGb`, `volumeInGb`, `gpuCount`, `ports` and `env` — but the
/// `machine` object comes back **empty**, so the **GPU type and cloud tier are NOT
/// recoverable** (both come back blank → caller falls back to the configured `GPU_TYPE` /
/// `CLOUD_TYPE`, which is correct since the fleet is provisioned uniformly from config).
/// `name` is returned empty for the caller to set, and the per-machine/identity env vars
/// (`PUBLIC_KEY`, `MACHINE_NAME`) are dropped — `replace` re-seeds those for the new pod.
fn parse_spec(v: &Value) -> PodSpec {
    let machine = v.get("machine");
    let m = |k: &str| machine.and_then(|mc| mc.get(k));

    let gpu_type = m("gpuTypeId")
        .or_else(|| m("gpuType"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let gpu_count = v
        .get("gpuCount")
        .and_then(Value::as_u64)
        .or_else(|| m("minPodGpuCount").and_then(Value::as_u64))
        .unwrap_or(1) as u32;
    // RunPod splits its catalog into a secure cloud and a community cloud; `secureCloud`
    // on the machine *would* tell us which tier this pod is on — but the REST API returns
    // an empty `machine` object (no `secureCloud`, and notably no GPU type) for both list
    // and get. So this is usually absent: return "" ("couldn't determine") and let the
    // caller fall back to the configured tier, exactly as it does for the GPU type.
    let cloud_type = match m("secureCloud").and_then(Value::as_bool) {
        Some(true) => "SECURE",
        Some(false) => "COMMUNITY",
        None => "",
    }
    .to_string();

    let ports = v
        .get("ports")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8888/http,22/tcp".to_string());

    let env = v
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter(|(k, _)| k.as_str() != "PUBLIC_KEY" && k.as_str() != "MACHINE_NAME")
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    PodSpec {
        name: String::new(),
        image: v.get("imageName").and_then(Value::as_str).unwrap_or_default().to_string(),
        gpu_type,
        gpu_count,
        cloud_type,
        disk_gb: v.get("containerDiskInGb").and_then(Value::as_u64).unwrap_or(0) as u32,
        volume_gb: v.get("volumeInGb").and_then(Value::as_u64).unwrap_or(0) as u32,
        ports,
        env,
        docker_args: None,
        allowed_cuda: Vec::new(),
        max_price: None,
    }
}

/// The per-pod details the REST list leaves out (its `machine` object comes back empty).
/// One query for the whole account — not one per pod — so `pods list` costs exactly one
/// extra request. Field names validated against the live API (2026-10-07).
const POD_DETAILS_QUERY: &str = "{ myself { pods { id gpuCount costPerHr \
     machine { gpuDisplayName maintenanceStart maintenanceEnd maintenanceNote } } } }";

/// What GraphQL `myself.pods` adds for one pod (see [`merge_pod_details`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct PodDetail {
    gpu_type: Option<String>,
    gpu_count: Option<u32>,
    cost_per_hr: Option<f64>,
    maintenance: Option<Maintenance>,
}

/// A non-empty string from a JSON string or number (`null`/other → `None`).
pub(super) fn loose_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A number from a JSON number or numeric string. RunPod doesn't document these GraphQL
/// scalar types (some APIs send decimals as strings), so accept either rather than lose
/// a price to a representation change.
pub(super) fn loose_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Unix epoch seconds → `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian, UTC). Hand-rolled
/// (days-from-civil inverse) to avoid a date-time dependency for one conversion.
fn epoch_to_iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// A maintenance timestamp. The live API's type for `maintenanceStart/End` is unknown, so
/// accept an ISO string as-is, or an epoch number (or numeric string) in seconds or
/// milliseconds (> 1e11 can only be ms: 1e11 s is the year 5138), normalized to ISO UTC.
fn loose_time(v: &Value) -> Option<String> {
    let epoch = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => match s.trim().parse::<f64>() {
            Ok(n) => Some(n),
            Err(_) => return Some(s.trim().to_string()),
        },
        _ => None,
    }?;
    if !epoch.is_finite() || epoch <= 0.0 {
        return None; // 0 / negative = "unset" placeholders, not a 1970 window
    }
    let secs = if epoch > 1e11 { epoch / 1000.0 } else { epoch };
    Some(epoch_to_iso(secs as i64))
}

/// Parse GraphQL `{ data: { myself: { pods: [...] } } }` into id → details. Pure, so it's
/// tested against a recorded-shape fixture. Entries without an id are skipped; a missing
/// or `null` `machine` just leaves its fields `None`.
fn parse_pod_details(v: &Value) -> HashMap<String, PodDetail> {
    let pods = v.pointer("/data/myself/pods").and_then(Value::as_array).cloned().unwrap_or_default();
    pods.iter()
        .filter_map(|p| {
            let id = p.get("id").and_then(loose_string)?;
            let machine = |k: &str| p.get("machine").and_then(|m| m.get(k));
            let maintenance = Maintenance {
                start: machine("maintenanceStart").and_then(loose_time),
                end: machine("maintenanceEnd").and_then(loose_time),
                note: machine("maintenanceNote").and_then(loose_string),
            };
            let has_window = maintenance.start.is_some() || maintenance.end.is_some() || maintenance.note.is_some();
            let detail = PodDetail {
                gpu_type: machine("gpuDisplayName").and_then(loose_string),
                gpu_count: p.get("gpuCount").and_then(loose_f64).filter(|n| *n >= 0.0).map(|n| n as u32),
                cost_per_hr: p.get("costPerHr").and_then(loose_f64).filter(|c| *c >= 0.0),
                maintenance: has_window.then_some(maintenance),
            };
            Some((id, detail))
        })
        .collect()
}

/// Fold GraphQL details into REST-listed pods, matched by id. REST stays authoritative
/// for what it does return (GPU type, $/h); GraphQL fills the gaps, owns `gpu_count` when
/// it has one, and is the only source of `maintenance` (set only when a window exists).
/// Pods GraphQL doesn't mention are untouched.
fn merge_pod_details(pods: &mut [Pod], details: &HashMap<String, PodDetail>) {
    for p in pods.iter_mut() {
        let Some(d) = details.get(&p.id) else { continue };
        if p.gpu_type.is_none() {
            p.gpu_type = d.gpu_type.clone();
        }
        p.gpu_count = d.gpu_count.or(p.gpu_count);
        if p.cost_per_hr.is_none() {
            p.cost_per_hr = d.cost_per_hr;
        }
        if d.maintenance.is_some() {
            p.maintenance = d.maintenance.clone();
        }
    }
}

/// One entry from RunPod's GPU catalog (its GraphQL `gpuTypes`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuType {
    /// The exact name to pass to `--gpu` (e.g. "NVIDIA A100 80GB PCIe").
    pub id: String,
    /// Short display name (e.g. "A100 PCIe").
    pub display_name: String,
    pub memory_gb: u32,
    /// Live on-demand $/h per GPU on the community / secure cloud. `None` when RunPod
    /// reports no price (null/0 — that tier doesn't offer the GPU) or the field is absent.
    pub community_price: Option<f64>,
    pub secure_price: Option<f64>,
    /// RunPod's stock indicator for a 1-GPU pod (`lowestPrice.stockStatus`, e.g. "Low",
    /// "Medium", "High"); `None` when unreported. A hint only — creating is the real test.
    pub stock_status: Option<String>,
}

/// The catalog with live prices + stock (fields validated against the live API 2026-10-07,
/// e.g. A4000 → communityPrice 0.17, securePrice 0.25, stockStatus "Low").
const GPU_TYPES_QUERY: &str = "{ gpuTypes { id displayName memoryInGb securePrice communityPrice \
     lowestPrice(input: {gpuCount: 1}) { stockStatus } } }";
/// The plain catalog, kept as a fallback: if RunPod ever rejects the pricing sub-fields,
/// `arena gpus` still lists live names (prices then come from the local presets).
const GPU_TYPES_BASIC_QUERY: &str = "{ gpuTypes { id displayName memoryInGb } }";

/// Parse a `gpuTypes` response. Pure, for fixture tests. Entries without an id are
/// skipped; non-positive prices are `None` (RunPod sends 0/null for an unoffered tier).
fn parse_gpu_types(v: &Value) -> Vec<GpuType> {
    let arr = v.pointer("/data/gpuTypes").and_then(Value::as_array).cloned().unwrap_or_default();
    let price = |x: &Value, k: &str| x.get(k).and_then(loose_f64).filter(|p| *p > 0.0);
    arr.iter()
        .filter_map(|x| {
            Some(GpuType {
                id: x.get("id")?.as_str()?.to_string(),
                display_name: x.get("displayName").and_then(Value::as_str).unwrap_or("").to_string(),
                memory_gb: x.get("memoryInGb").and_then(loose_f64).map(|n| n.max(0.0) as u32).unwrap_or(0),
                community_price: price(x, "communityPrice"),
                secure_price: price(x, "securePrice"),
                stock_status: x.pointer("/lowestPrice/stockStatus").and_then(loose_string),
            })
        })
        .collect()
}

/// What the priced catalog query's result means for [`fetch_gpu_types`].
#[derive(Debug)]
enum PricedCatalog {
    /// Use this result as-is (the catalog, or an error the plain query couldn't fix).
    Final(Result<Vec<GpuType>>),
    /// The server rejected the priced query: try the plain one (carrying why, for the
    /// message if that fails too).
    Fallback(String),
}

/// Judge the priced query's result. Pure, so the fallback policy is table-tested.
///
/// Falls back when RunPod *answered* but rejected the query — the case the plain query
/// exists for (pricing sub-fields renamed/removed). That arrives either as HTTP 200 with
/// `errors` and no data, or as a non-2xx: GraphQL servers commonly answer a schema
/// validation error with HTTP 400, and a crashing price resolver with a 5xx. Not on
/// auth (401/403) or rate-limit (429) — the plain query would fail the same way, or add
/// to the throttling — nor on a transport error (no answer at all). Partial data
/// alongside errors (e.g. one GPU's price failed) is still useful: kept.
fn judge_priced_catalog(result: Result<Value>) -> PricedCatalog {
    match result {
        Ok(v) => {
            let types = parse_gpu_types(&v);
            match graphql_errors(&v).filter(|_| types.is_empty()) {
                Some(err) => PricedCatalog::Fallback(err),
                None => PricedCatalog::Final(Ok(types)),
            }
        }
        Err(e @ Error::Provider { kind: ProviderErrorKind::Auth | ProviderErrorKind::RateLimited, .. }) => {
            PricedCatalog::Final(Err(e))
        }
        Err(e @ Error::Provider { .. }) => PricedCatalog::Fallback(e.to_string()),
        Err(e) => PricedCatalog::Final(Err(e)),
    }
}

/// Judge the plain (fallback) catalog query's result: its catalog, or an error that
/// names both failures (`priced_err` is why the priced query was abandoned).
fn judge_plain_catalog(result: Result<Value>, priced_err: &str) -> Result<Vec<GpuType>> {
    let plain_err = match result {
        Ok(v) => {
            let types = parse_gpu_types(&v);
            if !types.is_empty() {
                return Ok(types);
            }
            graphql_errors(&v).unwrap_or_else(|| "no gpuTypes in the response".into())
        }
        Err(e) => e.to_string(),
    };
    Err(Error::provider(format!("fetch gpu types: {priced_err} (plain catalog query also failed: {plain_err})")))
}

/// Fetch RunPod's full GPU catalog via GraphQL (the REST v1 API has no gpu-types route).
/// This is the authoritative, live list of `--gpu` names — including ones the local
/// preset table doesn't alias — with live community/secure prices and stock status. If
/// RunPod rejects the priced query, the plain catalog query is tried before giving up
/// (see [`judge_priced_catalog`]), so the live `--gpu` names survive a pricing-schema change.
pub async fn fetch_gpu_types(api_key: &str) -> Result<Vec<GpuType>> {
    let client = Client::new();
    let priced = graphql(&client, api_key, &json!({ "query": GPU_TYPES_QUERY }), "fetch gpu types").await;
    let priced_err = match judge_priced_catalog(priced) {
        PricedCatalog::Final(result) => return result,
        PricedCatalog::Fallback(why) => why,
    };
    let plain = graphql(&client, api_key, &json!({ "query": GPU_TYPES_BASIC_QUERY }), "fetch gpu types").await;
    judge_plain_catalog(plain, &priced_err)
}

/// Pull the GPU type ids the REST `create` endpoint actually accepts, from its OpenAPI
/// schema (`gpuTypeIds` enum). RunPod's create-validation enum can LAG the live `gpuTypes`
/// catalog — a GPU can be listed/in-stock yet rejected at create with a 400 ("value must be
/// one of …"). `arena gpus` intersects with this so it only advertises creatable types.
pub async fn fetch_creatable_gpu_ids(api_key: &str) -> Result<Vec<String>> {
    let client = Client::new();
    let spec = send_json(client.get(format!("{BASE}/openapi.json")).bearer_auth(api_key), "fetch openapi").await?;
    Ok(extract_gpu_enum(&spec))
}

/// Find the `gpuTypeIds` items-enum anywhere in an OpenAPI document (pure, so it's tested
/// without a network call). Returns the first such enum found, or empty if absent.
fn extract_gpu_enum(spec: &Value) -> Vec<String> {
    fn search(v: &Value) -> Option<Vec<String>> {
        match v {
            Value::Object(m) => {
                if let Some(en) = m
                    .get("gpuTypeIds")
                    .and_then(|g| g.get("items"))
                    .and_then(|i| i.get("enum"))
                    .and_then(Value::as_array)
                {
                    let ids: Vec<String> = en.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    if !ids.is_empty() {
                        return Some(ids);
                    }
                }
                m.values().find_map(search)
            }
            Value::Array(a) => a.iter().find_map(search),
            _ => None,
        }
    }
    search(spec).unwrap_or_default()
}

/// One RunPod network volume: persistent storage billed per GB per month for as long as it
/// exists, whether or not a pod uses it — the bill that outlives a program (ops playbook §5).
/// Read by `teardown --check`, from GraphQL here or from REST v2 ([`super::runpod_v2`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkVolume {
    pub id: String,
    pub name: String,
    /// Allocated GB (what's billed). `None` if the API didn't say — then no estimate.
    pub size_gb: Option<u32>,
    pub data_center: Option<String>,
    /// v2's storage tier (`STANDARD` / `HIGH_PERFORMANCE`); the GraphQL query doesn't ask.
    pub tier: Option<String>,
}

/// The account's network volumes over GraphQL — the only listing on `RUNPOD_API=v1` (REST v1
/// has no volumes route). Not live-verified (no live calls while a cohort runs); the field
/// names come from existing clients of this API: dstack's `getMyVolumes` query reads
/// `myself { networkVolumes { id name size dataCenter { id name } } }`, and SkyPilot reads
/// `networkVolumes[].dataCenterId` from runpod-python's `get_user()` (`myself`) result. If a
/// field is ever rejected, the query fails as a whole and the check says it couldn't look.
const NETWORK_VOLUMES_QUERY: &str = "{ myself { networkVolumes { id name size dataCenterId } } }";

/// One volume object (GraphQL or REST v2 — same `id`/`name`/`size`; the data center under
/// `dc_key`, v2's tier under `type`). An entry without an id fails the listing: it's a volume
/// we'd otherwise not report, and a teardown check must not under-count.
pub(super) fn parse_volume(v: &Value, dc_key: &str) -> Result<NetworkVolume> {
    let id = v
        .get("id")
        .and_then(loose_string)
        .ok_or_else(|| Error::provider("list network volumes: a volume without an id — not a complete listing"))?;
    Ok(NetworkVolume {
        id,
        name: v.get("name").and_then(loose_string).unwrap_or_default(),
        size_gb: v.get("size").and_then(loose_f64).filter(|s| s.is_finite() && *s >= 0.0).map(|s| s.round() as u32),
        data_center: v.get(dc_key).and_then(loose_string),
        tier: v.get("type").and_then(loose_string),
    })
}

/// Parse the GraphQL answer. Fails closed: any GraphQL error, or no `networkVolumes` array
/// (absent *or* `null` — GraphQL's null is "no answer", not "none"), is an error rather than
/// an empty list, because "no volumes" is exactly what a teardown check must not guess.
fn parse_network_volumes(v: &Value) -> Result<Vec<NetworkVolume>> {
    if let Some(err) = graphql_errors(v) {
        return Err(Error::provider(format!("list network volumes: {err}")));
    }
    v.pointer("/data/myself/networkVolumes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::provider("list network volumes: no data.myself.networkVolumes array in the response"))?
        .iter()
        .map(|x| parse_volume(x, "dataCenterId"))
        .collect()
}

/// Read-only: every network volume on the account (GraphQL; see [`NETWORK_VOLUMES_QUERY`]).
pub async fn fetch_network_volumes(api_key: &str) -> Result<Vec<NetworkVolume>> {
    let body = graphql(&Client::new(), api_key, &json!({ "query": NETWORK_VOLUMES_QUERY }), "list network volumes").await?;
    parse_network_volumes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GraphQL volume listing: sizes and data centers read, and every way of not
    /// answering — errors, a missing or null list, an entry without an id — is an error,
    /// never "no volumes".
    #[test]
    fn network_volumes_parse_and_fail_closed() {
        let ok = json!({ "data": { "myself": { "networkVolumes": [
            { "id": "vol1", "name": "group-a", "size": 100, "dataCenterId": "EU-RO-1" },
            { "id": "vol2", "name": null, "size": "2048", "dataCenterId": null }
        ] } } });
        let vols = parse_network_volumes(&ok).unwrap();
        assert_eq!(
            vols[0],
            NetworkVolume {
                id: "vol1".into(),
                name: "group-a".into(),
                size_gb: Some(100),
                data_center: Some("EU-RO-1".into()),
                tier: None
            }
        );
        assert_eq!((vols[1].name.as_str(), vols[1].size_gb, vols[1].data_center.as_deref()), ("", Some(2048), None));
        assert!(parse_network_volumes(&json!({ "data": { "myself": { "networkVolumes": [] } } })).unwrap().is_empty());
        for (bad, why) in [
            (json!({ "errors": [{ "message": "Cannot query field \"dataCenterId\"" }] }), "dataCenterId"),
            (json!({ "data": { "myself": { "networkVolumes": null } } }), "no data.myself.networkVolumes"),
            (json!({ "data": { "myself": {} } }), "no data.myself.networkVolumes"),
            (json!(null), "no data.myself.networkVolumes"),
            (json!({ "data": { "myself": { "networkVolumes": [{ "name": "x", "size": 10 }] } } }), "without an id"),
        ] {
            let e = parse_network_volumes(&bad).unwrap_err().to_string();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(NETWORK_VOLUMES_QUERY.contains("networkVolumes { id name size dataCenterId }"));
    }

    #[test]
    fn pods_array_accepts_both_list_shapes() {
        let top = json!([{"id": "a", "name": "arena8-apple"}]);
        assert_eq!(pods_array(&top).unwrap().len(), 1);
        let wrapped = json!({"pods": []});
        assert!(pods_array(&wrapped).unwrap().is_empty()); // a real, empty fleet is fine
    }

    /// Live repro (2026-10-07): a bogus key made `list_pods` fail with "http error: error
    /// decoding response body" — the body was decoded before the status was looked at, so
    /// the 401 was lost and the error wasn't `Auth`. Every v1 REST call, against a loopback
    /// server: an error status is classified by status whatever the body is.
    #[tokio::test]
    async fn v1_rest_calls_classify_error_statuses_before_decoding() {
        use crate::http::test_server::{canned, client, serve};
        use ProviderErrorKind as K;
        let srv = serve(vec![
            canned(401, "text/plain", "Unauthorized"),
            canned(403, "text/html", "<html>Forbidden</html>"),
            canned(500, "application/json", r#"{"error":"create pod: There are no instances currently available"}"#),
            canned(502, "text/html", "<html>Bad Gateway</html>"),
            canned(404, "text/plain", "pod not found"),
            canned(200, "application/json", r#"[{"id":"p1","name":"devtest-apple","desiredStatus":"RUNNING"}]"#),
            canned(200, "text/plain", ""), // a stop that worked, with an empty/odd body
        ]);
        let p = RunpodProvider::with_base("rpa_BOGUS", &srv.base, client());
        let e = p.list_pods().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("list pods HTTP 401") && !e.to_string().contains("decoding"), "{e}");
        let e = p.pod_spec("p1").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        let e = p.create_pod(&spec()).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Capacity), "{e}");
        let e = p.stop_pod("p1").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Transient), "{e}");
        let e = p.terminate_pod("p1").await.unwrap_err();
        assert!(e.to_string().contains("terminate pod HTTP 404") && e.to_string().contains("pod not found"), "{e}");
        let pods = p.list_pods().await.unwrap();
        assert_eq!((pods[0].id.as_str(), pods[0].status.as_str()), ("p1", "RUNNING"));
        p.stop_pod("p1").await.unwrap();
        let seen = srv.requests.lock().unwrap().clone();
        assert_eq!(
            seen,
            [
                "GET /pods HTTP/1.1",
                "GET /pods/p1 HTTP/1.1",
                "POST /pods HTTP/1.1",
                "POST /pods/p1/stop HTTP/1.1",
                "DELETE /pods/p1 HTTP/1.1",
                "GET /pods HTTP/1.1",
                "POST /pods/p1/stop HTTP/1.1",
            ]
        );
    }

    #[test]
    fn pods_array_fails_closed_on_schema_drift() {
        // None of these may read as "zero pods" — that would look like every pod was
        // terminated and drop their proxy forwards.
        for body in [json!({}), json!({"pods": null}), json!({"data": []}), json!(null), json!("ok")] {
            let err = pods_array(&body).unwrap_err().to_string();
            assert!(err.contains("unexpected response shape"), "{body}: {err}");
        }
    }

    fn spec() -> PodSpec {
        PodSpec {
            name: "arena8-entropy".into(),
            image: "nvcr.io/nvidia/clara/bionemo-framework:nightly".into(),
            gpu_type: "NVIDIA GeForce RTX 4090".into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            disk_gb: 200,
            volume_gb: 0,
            ports: "8888/http,22/tcp".into(),
            env: vec![("PUBLIC_KEY".into(), "ssh-ed25519 AAAA test".into())],
            docker_args: None,
            allowed_cuda: Vec::new(),
            max_price: None,
        }
    }

    #[test]
    fn payload_omits_start_cmd_without_bootstrap() {
        let p = create_payload(&spec());
        // No bootstrap => no start-command key at all (image's own CMD runs).
        assert!(p.get("dockerStartCmd").is_none());
        // And never the old GraphQL field name (that's what caused the 400).
        assert!(p.get("dockerArgs").is_none());
        assert_eq!(p["ports"], json!(["8888/http", "22/tcp"]));
        assert_eq!(p["gpuTypeIds"], json!(["NVIDIA GeForce RTX 4090"]));
    }

    #[test]
    fn payload_sends_start_cmd_as_argv_array() {
        let mut s = spec();
        s.docker_args = Some(vec!["bash".into(), "-c".into(), "/usr/sbin/sshd -D".into()]);
        let p = create_payload(&s);
        // REST v1 wants an array of strings under `dockerStartCmd`, not a `dockerArgs` string.
        assert_eq!(p["dockerStartCmd"], json!(["bash", "-c", "/usr/sbin/sshd -D"]));
        assert!(p.get("dockerArgs").is_none());
    }

    #[test]
    fn parse_spec_recovers_recreate_fields_and_drops_identity_env() {
        let body = json!({
            "id": "abc", "name": "arena8-apple",
            "imageName": "arena/base:latest",
            "containerDiskInGb": 200, "volumeInGb": 50,
            "ports": ["8888/http", "22/tcp"],
            "env": {"PUBLIC_KEY": "ssh-ed25519 AAAA", "MACHINE_NAME": "arena8-apple", "WANDB_KEY": "w"},
            "machine": {"gpuTypeId": "NVIDIA A40", "secureCloud": true, "minPodGpuCount": 2}
        });
        let s = parse_spec(&body);
        assert_eq!(s.name, ""); // caller fills
        assert_eq!(s.image, "arena/base:latest");
        assert_eq!(s.gpu_type, "NVIDIA A40");
        assert_eq!(s.gpu_count, 2); // from minPodGpuCount when gpuCount absent
        assert_eq!(s.cloud_type, "SECURE");
        assert_eq!(s.disk_gb, 200);
        assert_eq!(s.volume_gb, 50);
        assert_eq!(s.ports, "8888/http,22/tcp");
        // PUBLIC_KEY + MACHINE_NAME dropped (replace re-seeds them); other env kept.
        assert_eq!(s.env, vec![("WANDB_KEY".to_string(), "w".to_string())]);
        assert!(s.docker_args.is_none());
    }

    #[test]
    fn payload_sends_allowed_cuda_only_when_set() {
        let mut s = spec();
        assert!(create_payload(&s).get("allowedCudaVersions").is_none());
        s.allowed_cuda = vec!["13.0".into()];
        assert_eq!(create_payload(&s)["allowedCudaVersions"], json!(["13.0"]));
    }

    #[test]
    fn reimage_payload_sets_image_and_full_env() {
        let v = reimage_payload("img:9", &[("MACHINE_NAME".into(), "a-b".into()), ("PUBLIC_KEY".into(), "k1\nk2".into())]);
        assert_eq!(v["imageName"], "img:9");
        assert_eq!(v["env"]["MACHINE_NAME"], "a-b");
        assert_eq!(v["env"]["PUBLIC_KEY"], "k1\nk2");
    }

    #[test]
    fn parse_spec_defaults_when_fields_absent() {
        // A sparse body (image omitted as the docs schema does) must not panic and must
        // leave image blank so the caller falls back to the configured image.
        let s = parse_spec(&json!({"name": "x", "machine": {}}));
        assert_eq!(s.image, "");
        assert_eq!(s.gpu_type, ""); // machine is empty in the real API → unknown
        assert_eq!(s.gpu_count, 1);
        assert_eq!(s.cloud_type, ""); // unknown → caller falls back to config
        assert_eq!(s.ports, "8888/http,22/tcp");
        assert!(s.env.is_empty());
    }

    #[test]
    fn extract_gpu_enum_finds_nested_create_enum() {
        // Shape mirrors the real spec: components.schemas.PodCreateInput.properties…
        let spec = json!({
            "components": {"schemas": {"PodCreateInput": {"properties": {
                "gpuTypeIds": {"type": "array", "items": {"type": "string", "enum": [
                    "NVIDIA A40", "NVIDIA GeForce RTX 4090"
                ]}}
            }}}}
        });
        assert_eq!(extract_gpu_enum(&spec), vec!["NVIDIA A40", "NVIDIA GeForce RTX 4090"]);
        // Absent => empty (caller treats empty as "couldn't determine", shows all).
        assert!(extract_gpu_enum(&json!({"paths": {}})).is_empty());
    }

    /// Regression guard for the key-in-URL leak: every GraphQL call is built here, and the
    /// key must travel only in the Authorization header (reqwest errors print the URL).
    #[test]
    fn graphql_request_keeps_key_out_of_url() {
        let req = graphql_request(&Client::new(), "rpa_SECRET", &json!({ "query": "{ x }" }))
            .build()
            .unwrap();
        assert_eq!(req.url().as_str(), "https://api.runpod.io/graphql");
        assert!(!req.url().as_str().contains("SECRET"));
        assert!(req.url().query().is_none());
        assert_eq!(req.headers()["authorization"], "Bearer rpa_SECRET");
        assert_eq!(req.method(), reqwest::Method::POST);
    }

    #[test]
    fn graphql_errors_extracts_messages_and_ignores_empty() {
        assert_eq!(graphql_errors(&json!({ "data": {} })), None);
        assert_eq!(graphql_errors(&json!({ "errors": null })), None);
        assert_eq!(graphql_errors(&json!({ "errors": [] })), None);
        assert_eq!(
            graphql_errors(&json!({ "errors": [{ "message": "a" }, { "message": "b", "path": ["x"] }] })).as_deref(),
            Some("a; b")
        );
        // Unexpected shape: shown raw rather than swallowed.
        assert_eq!(graphql_errors(&json!({ "errors": "boom" })).as_deref(), Some("\"boom\""));
    }

    #[test]
    fn epoch_to_iso_matches_known_dates() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_iso(1_760_000_000), "2025-10-09T08:53:20Z");
        assert_eq!(epoch_to_iso(1_791_936_000), "2026-10-14T00:00:00Z");
        assert_eq!(epoch_to_iso(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(epoch_to_iso(-86_400), "1969-12-31T00:00:00Z");
    }

    /// Recorded-shape fixture of `{ myself { pods { … machine { … } } } }`. The maintenance
    /// fields' live types are unknown, so cover ISO strings, epoch s/ms numbers, numeric
    /// strings, empty strings and null.
    fn pod_details_fixture() -> Value {
        json!({ "data": { "myself": { "pods": [
            { "id": "pa", "gpuCount": 1, "costPerHr": 0.17,
              "machine": { "gpuDisplayName": "RTX A4000",
                           "maintenanceStart": "2026-10-09T02:00:00.000Z",
                           "maintenanceEnd": "2026-10-09T06:00:00.000Z",
                           "maintenanceNote": "host upgrade" } },
            { "id": "pb", "gpuCount": 2, "costPerHr": "0.34",
              "machine": { "gpuDisplayName": "RTX 3090",
                           "maintenanceStart": 1_791_936_000_000u64,
                           "maintenanceEnd": 1_791_950_400,
                           "maintenanceNote": "" } },
            { "id": "pc", "gpuCount": 1, "costPerHr": 0.2,
              "machine": { "gpuDisplayName": "RTX A4000",
                           "maintenanceStart": null, "maintenanceEnd": "", "maintenanceNote": null } },
            { "id": "pd", "gpuCount": 0, "costPerHr": 0, "machine": null },
            { "id": "pe", "machine": { "maintenanceStart": "1791936000" } },
            { "gpuCount": 1, "machine": { "gpuDisplayName": "no id — skipped" } }
        ] } } })
    }

    #[test]
    fn parse_pod_details_fixture() {
        let d = parse_pod_details(&pod_details_fixture());
        assert_eq!(d.len(), 5);
        assert_eq!(
            d["pa"],
            PodDetail {
                gpu_type: Some("RTX A4000".into()),
                gpu_count: Some(1),
                cost_per_hr: Some(0.17),
                maintenance: Some(Maintenance {
                    start: Some("2026-10-09T02:00:00.000Z".into()),
                    end: Some("2026-10-09T06:00:00.000Z".into()),
                    note: Some("host upgrade".into()),
                }),
            }
        );
        // Epoch ms / s numbers normalize to ISO UTC; an empty note is no note.
        let pb = d["pb"].maintenance.clone().unwrap();
        assert_eq!(pb.start.as_deref(), Some("2026-10-14T00:00:00Z"));
        assert_eq!(pb.end.as_deref(), Some("2026-10-14T04:00:00Z"));
        assert_eq!(pb.note, None);
        assert_eq!(d["pb"].cost_per_hr, Some(0.34)); // numeric string accepted
        // All-empty/null window => no maintenance at all (not an empty struct).
        assert_eq!(d["pc"].maintenance, None);
        // null machine => machine fields None, top-level ones still read.
        assert_eq!(d["pd"].gpu_type, None);
        assert_eq!(d["pd"].gpu_count, Some(0));
        assert_eq!(d["pe"].maintenance.as_ref().unwrap().start.as_deref(), Some("2026-10-14T00:00:00Z"));
        // Anything unexpected degrades to empty, never panics.
        assert!(parse_pod_details(&json!({ "errors": [{ "message": "x" }] })).is_empty());
        assert!(parse_pod_details(&json!({ "data": { "myself": null } })).is_empty());
    }

    #[test]
    fn merge_pod_details_fills_gaps_without_clobbering_rest() {
        let details = parse_pod_details(&pod_details_fixture());
        let mut pods = vec![
            // REST list shape: no machine info at all.
            Pod { id: "pa".into(), provider: "runpod".into(), ..Default::default() },
            // REST already had type + price: kept; GraphQL count wins.
            Pod {
                id: "pb".into(),
                gpu_type: Some("NVIDIA GeForce RTX 3090".into()),
                gpu_count: Some(1),
                cost_per_hr: Some(0.30),
                ..Default::default()
            },
            // No window reported: an existing value is not wiped.
            Pod {
                id: "pc".into(),
                maintenance: Some(Maintenance { note: Some("keep".into()), ..Default::default() }),
                ..Default::default()
            },
            // Unknown to GraphQL: untouched.
            Pod { id: "zz".into(), ..Default::default() },
        ];
        merge_pod_details(&mut pods, &details);
        assert_eq!(pods[0].gpu_type.as_deref(), Some("RTX A4000"));
        assert_eq!(pods[0].gpu_count, Some(1));
        assert_eq!(pods[0].cost_per_hr, Some(0.17));
        assert_eq!(pods[0].maintenance.as_ref().unwrap().note.as_deref(), Some("host upgrade"));
        assert_eq!(pods[1].gpu_type.as_deref(), Some("NVIDIA GeForce RTX 3090"));
        assert_eq!(pods[1].gpu_count, Some(2));
        assert_eq!(pods[1].cost_per_hr, Some(0.30));
        assert_eq!(pods[2].maintenance.as_ref().unwrap().note.as_deref(), Some("keep"));
        assert_eq!(pods[3], Pod { id: "zz".into(), ..Default::default() });
    }

    /// Recorded-shape fixture of the `gpuTypes` query with prices + stock.
    #[test]
    fn parse_gpu_types_fixture() {
        let v = json!({ "data": { "gpuTypes": [
            { "id": "NVIDIA RTX A4000", "displayName": "RTX A4000", "memoryInGb": 16,
              "securePrice": 0.25, "communityPrice": 0.17, "lowestPrice": { "stockStatus": "Low" } },
            { "id": "NVIDIA H100 80GB HBM3", "displayName": "H100 SXM", "memoryInGb": 80,
              "securePrice": 2.69, "communityPrice": 0, "lowestPrice": { "stockStatus": null } },
            { "id": "NVIDIA L4", "displayName": "L4", "memoryInGb": 24,
              "securePrice": null, "communityPrice": null, "lowestPrice": null },
            { "id": "unknown", "displayName": "unknown", "memoryInGb": 0 },
            { "displayName": "no id — skipped" }
        ] } });
        let t = parse_gpu_types(&v);
        assert_eq!(t.len(), 4);
        assert_eq!(
            t[0],
            GpuType {
                id: "NVIDIA RTX A4000".into(),
                display_name: "RTX A4000".into(),
                memory_gb: 16,
                community_price: Some(0.17),
                secure_price: Some(0.25),
                stock_status: Some("Low".into()),
            }
        );
        // communityPrice 0 = not offered on community => None, not "$0.00".
        assert_eq!((t[1].community_price, t[1].secure_price, t[1].stock_status.clone()), (None, Some(2.69), None));
        // Basic-query shape (no price fields at all) parses with prices None.
        assert_eq!((t[2].community_price, t[2].secure_price, t[2].stock_status.clone()), (None, None, None));
        assert_eq!(t[3].id, "unknown"); // filtering placeholders is the caller's policy
    }

    /// Review finding: a non-2xx rejection of the priced query (GraphQL validation errors
    /// are commonly HTTP 400) must still fall back to the plain catalog — before, `?`
    /// skipped straight to the local presets.
    #[test]
    fn priced_catalog_fallback_policy() {
        use reqwest::StatusCode;
        let catalog = json!({ "data": { "gpuTypes": [ { "id": "NVIDIA RTX A4000", "displayName": "RTX A4000" } ] } });
        let rejected = json!({ "errors": [ { "message": "Cannot query field \"securePrice\" on type \"GpuType\"." } ] });
        let http = |code: u16| Err(Error::provider_http(StatusCode::from_u16(code).unwrap(), &rejected, "fetch gpu types"));
        let transport = || Err(Error::Http(Client::new().get("not a url").build().unwrap_err()));

        // (case, priced query result, falls back?)
        let cases: Vec<(&str, Result<Value>, bool)> = vec![
            ("200 + data", Ok(catalog.clone()), false),
            ("200 + errors, no data", Ok(rejected.clone()), true),
            ("200 + errors + partial data", Ok(json!({ "data": catalog["data"], "errors": rejected["errors"] })), false),
            ("400 validation error", http(400), true),
            ("422", http(422), true),
            ("500 resolver crash", http(500), true),
            ("401 bad key", http(401), false),
            ("403", http(403), false),
            ("429 throttled", http(429), false),
            ("transport error", transport(), false),
        ];
        for (case, result, want_fallback) in cases {
            match judge_priced_catalog(result) {
                PricedCatalog::Fallback(why) => {
                    assert!(want_fallback, "{case}: fell back ({why})");
                    assert!(why.contains("Cannot query field"), "{case}: the reason is kept: {why}");
                }
                PricedCatalog::Final(r) => {
                    assert!(!want_fallback, "{case}: did not fall back ({r:?})");
                    if case.starts_with("200") {
                        assert_eq!(r.unwrap()[0].id, "NVIDIA RTX A4000", "{case}");
                    } else {
                        assert!(r.is_err(), "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn plain_catalog_result_names_both_failures() {
        let catalog = json!({ "data": { "gpuTypes": [ { "id": "NVIDIA L4", "displayName": "L4", "memoryInGb": 24 } ] } });
        assert_eq!(judge_plain_catalog(Ok(catalog), "priced: 400").unwrap()[0].id, "NVIDIA L4");
        let e = judge_plain_catalog(Ok(json!({ "errors": [ { "message": "boom" } ] })), "priced: 400").unwrap_err();
        assert_eq!(e.to_string(), "provider error: fetch gpu types: priced: 400 (plain catalog query also failed: boom)");
        let e = judge_plain_catalog(Ok(json!({ "data": { "gpuTypes": [] } })), "priced: 400").unwrap_err();
        assert!(e.to_string().contains("no gpuTypes"), "{e}");
        let e = judge_plain_catalog(Err(Error::provider("network down")), "priced: 400").unwrap_err();
        assert!(e.to_string().contains("priced: 400") && e.to_string().contains("network down"), "{e}");
    }
}
