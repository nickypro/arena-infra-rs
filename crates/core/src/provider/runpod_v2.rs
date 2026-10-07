//! RunPod backend on the REST **v2** API (`https://api.runpod.io/v2`), selected with
//! `RUNPOD_API=v2`. RunPod retires REST v1 ([`super::runpod`]) on 2026-11-15; this is its
//! replacement, opt-in until it has been live-tested on a sandbox pod.
//!
//! What differs from v1, and why the code looks the way it does:
//! - **Listing is cursor-paginated** (`{pods, pagination: {nextCursor, hasNextPage}}`).
//!   Every page is followed (a bounded number), and any unexpected shape is an error, never
//!   a short list: the proxy merge reads "listed OK without pod X" as "X was terminated".
//! - **Real lifecycle status** (`PROVISIONING`/`STARTING`/`RUNNING`/`EXITED`/`ERROR`/
//!   `TERMINATED`) instead of v1's `desiredStatus`, plus `gpu`, `cost` and `cloud` on every
//!   pod — so GPU type and cloud tier are now recoverable for `replace` (v1's were not).
//! - **SSH comes from `ssh.direct` only.** `ssh.proxy` (RunPod's SSH gateway) carries an
//!   interactive shell only — no scp/rsync/port-forwarding, which setup/backup/pull and the
//!   nginx stream proxy all need — so it is never reported as the pod's endpoint.
//! - **Create defaults to the SECURE cloud** when `cloud` is omitted (silently ~doubling the
//!   bill), so `cloud` is always sent and an unknown tier is refused before any request.
//! - **`startSsh` injects the account's registered SSH keys into `PUBLIC_KEY` only when the
//!   request doesn't set one** — and we always set it (the cohort keys). So the account's
//!   keys are fetched (`GET /v2/account/ssh-keys`) and merged in here, which keeps the
//!   admin key working exactly as on v1, where RunPod added it on its own.
//! - **Rename and maintenance stay on GraphQL** (shared with `runpod.rs`): v2 has no
//!   maintenance field, and its `PATCH name` isn't documented as restart-free (v1's PATCH
//!   restarted the container, wiping its disk).
//!
//! Requests and responses go through small pure functions (URL/body builders, [`judge`],
//! [`parse_page`], [`parse_pod`], [`parse_spec`]) so all of it is tested against
//! schema-shaped fixtures from RunPod's v2 OpenAPI document without a network call.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Mutex;

use async_trait::async_trait;
use reqwest::{Client, Method, RequestBuilder, StatusCode, Url};
use serde_json::{json, Map, Value};

use super::runpod::{self, loose_f64, loose_string, GpuType};
use super::Provider;
use crate::error::{Error, ProviderErrorKind, Result};
use crate::pod::{Pod, PodSpec};

const BASE: &str = "https://api.runpod.io/v2";

/// Page size for `GET /v2/pods` (the API's maximum, and its default). A cohort is a few
/// dozen pods, so this is one page in practice; the loop exists for correctness.
const PAGE_LIMIT: u32 = 1000;

/// Upper bound on pages followed by one listing (20 × 1000 pods). Hitting it is an error,
/// not a truncated list — see the module doc on why a partial list is dangerous.
const MAX_PAGES: usize = 20;

/// Where a persistent volume (`VOLUME_GB` > 0) is mounted: RunPod's conventional path, and
/// what v1 used when `volumeMountPath` was omitted (v2 makes the path mandatory).
const VOLUME_MOUNT_PATH: &str = "/workspace";

pub struct RunpodV2Provider {
    api_key: String,
    client: Client,
    /// The account's registered SSH public keys, fetched on first need (create/reimage)
    /// and kept for the process — a batch create of a whole cohort costs one extra
    /// request, not one per pod.
    account_keys: Mutex<Option<Vec<String>>>,
}

impl RunpodV2Provider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), client: Client::new(), account_keys: Mutex::new(None) }
    }

    /// The account's registered SSH keys (cached, see the field). A failed fetch fails the
    /// caller *before* it mutates anything: a pod created without the admin key would only
    /// be discovered later, when someone needs it.
    async fn account_keys(&self) -> Result<Vec<String>> {
        let cached = self.account_keys.lock().unwrap().clone();
        if let Some(keys) = cached {
            return Ok(keys);
        }
        let rb = api_request(&self.client, &self.api_key, Method::GET, base_url("account/ssh-keys"));
        let body = send(rb, "account ssh keys").await?;
        let keys = parse_account_keys(&body)?;
        *self.account_keys.lock().unwrap() = Some(keys.clone());
        Ok(keys)
    }

    /// The account keys when `env` sets `PUBLIC_KEY` (they must be merged in by hand),
    /// else none — without our own `PUBLIC_KEY`, `startSsh` injects them server-side, so
    /// the fetch would be a wasted request.
    async fn keys_to_merge(&self, env: &[(String, String)]) -> Result<Vec<String>> {
        if env.iter().any(|(k, _)| k == "PUBLIC_KEY") {
            self.account_keys().await
        } else {
            Ok(Vec::new())
        }
    }
}

/// `{BASE}/{path}` for a fixed, known-safe path.
fn base_url(path: &str) -> Url {
    Url::parse(&format!("{BASE}/{path}")).expect("BASE + a static path is a valid URL")
}

/// `{BASE}/pods/{id}[/{suffix}]`, with `id` pushed as one encoded path segment (so an odd
/// id can't walk to another endpoint: `/`, `?`, `#`, `%` are percent-encoded). The ids the
/// encoding can't contain are refused: an empty id (`pods/` is the list), and `.`/`..`,
/// which the URL parser resolves as dot-segments — `pods/..` is the list too, so a
/// terminate would DELETE the collection and `pod_spec` would read the list as one pod.
fn pod_url(id: &str, suffix: Option<&str>) -> Result<Url> {
    if id.trim().is_empty() {
        return Err(Error::provider("runpod v2: empty pod id"));
    }
    if matches!(id.trim(), "." | "..") {
        return Err(Error::provider(format!("runpod v2: invalid pod id `{}`", id.trim())));
    }
    let mut url = base_url("pods");
    {
        let mut segs = url.path_segments_mut().expect("an https URL has path segments");
        segs.push(id);
        if let Some(s) = suffix {
            segs.push(s);
        }
    }
    Ok(url)
}

/// An authenticated request. The key travels only in the `Authorization: Bearer` header —
/// never the URL, which reqwest errors print (see `runpod::graphql_request`).
fn api_request(client: &Client, api_key: &str, method: Method, url: Url) -> RequestBuilder {
    client.request(method, url).bearer_auth(api_key)
}

/// One `GET /v2/pods` page request (`cursor` = the previous page's `nextCursor`).
fn list_request(client: &Client, api_key: &str, cursor: Option<&str>) -> RequestBuilder {
    let mut query: Vec<(&str, String)> = vec![("limit", PAGE_LIMIT.to_string())];
    if let Some(c) = cursor {
        query.push(("cursor", c.to_string()));
    }
    api_request(client, api_key, Method::GET, base_url("pods")).query(&query)
}

/// The GPU catalog with prices and pod stock for one cloud tier.
fn catalog_request(client: &Client, api_key: &str, cloud: &str) -> RequestBuilder {
    api_request(client, api_key, Method::GET, base_url("catalog/gpus"))
        .query(&[("include", "AVAILABILITY"), ("product", "POD"), ("cloud", cloud)])
}

/// Send a request and [`judge`] its response.
async fn send(rb: RequestBuilder, ctx: &str) -> Result<Value> {
    let resp = rb.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    judge(status, &text, ctx)
}

/// Turn one response (status + raw body) into its JSON body or a classified error. Pure,
/// so the status handling is table-tested.
///
/// - 2xx with an empty body (`204 No Content`: terminate) → `Null`.
/// - 2xx with a body that isn't JSON → an `Other` error (not retried — a decode error
///   would just fail again).
/// - Anything else → [`Error::provider_http`], which classifies it: 401/403 auth, 429
///   rate-limited, capacity sniffed from the message (v2 reports "no capacity" as a 400
///   whose only signal is the human-readable `detail`), other 5xx transient. Error bodies
///   are problem+json (`{title, status, detail, errors}`); a proxy's HTML 502 isn't, so a
///   non-JSON body is kept as raw text (truncated by `provider_http`).
fn judge(status: StatusCode, text: &str, ctx: &str) -> Result<Value> {
    if status.is_success() {
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(text).map_err(|e| {
            let raw: String = text.chars().take(120).collect();
            Error::provider(format!("{ctx}: HTTP {status} with a body that isn't JSON ({e}): {raw}"))
        });
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Err(Error::provider_http(status, &v, ctx)),
        Err(_) => Err(Error::provider_http(status, &text, ctx)),
    }
}

/// [`judge`] for `POST /v2/pods`, whose error table gives 403 a meaning of its own: "Your
/// account cannot access the requested pool. | Skip this candidate, keep going." So a 403
/// here is [`ProviderErrorKind::Denied`] (placement skips that option and tries the next),
/// not `Auth` (which aborts the whole run on one restricted pool). 401 stays `Auth`, and a
/// 403 anywhere else (e.g. the ssh-keys read) stays `Auth` too. Pure.
fn judge_create(status: StatusCode, text: &str) -> Result<Value> {
    judge(status, text, "create pod").map_err(|e| match e {
        Error::Provider { message, .. } if status == StatusCode::FORBIDDEN => {
            Error::Provider { kind: ProviderErrorKind::Denied, message }
        }
        e => e,
    })
}

/// What a JSON body looks like — its top-level keys, or its type — for shape errors.
/// Never the values: a v2 pod object carries its whole `env`, which can hold tokens
/// (HF_TOKEN, …), and these messages get printed and land in cron logs.
fn shape_of(body: &Value) -> String {
    match body {
        Value::Object(o) => format!("object with keys [{}]", o.keys().map(String::as_str).collect::<Vec<_>>().join(", ")),
        Value::Array(a) => format!("array of {}", a.len()),
        Value::String(_) => "a string".into(),
        Value::Number(_) => "a number".into(),
        Value::Bool(_) => "a bool".into(),
        Value::Null => "null/empty".into(),
    }
}

/// One `GET /v2/pods` page: its pods and the next page's cursor (`None` = last page).
///
/// Fails closed on anything unexpected — no `pods` array, no `pagination` block, no
/// `hasNextPage`, or "more pages" without a cursor to fetch them with. Each of those
/// would otherwise read as a complete (short) list, and the proxy merge treats a pod
/// missing from a successful list as terminated, dropping its forward.
fn parse_page(body: &Value) -> Result<(Vec<Pod>, Option<String>)> {
    let shape = |what: &str| {
        Error::provider(format!("list pods: unexpected response shape ({what}): {}", shape_of(body)))
    };
    let pods = body.get("pods").and_then(Value::as_array).ok_or_else(|| shape("no pods array"))?;
    let pagination = body.get("pagination").filter(|p| p.is_object()).ok_or_else(|| shape("no pagination"))?;
    let more = pagination.get("hasNextPage").and_then(Value::as_bool).ok_or_else(|| shape("no hasNextPage"))?;
    let next = if more {
        let cursor = pagination
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| shape("hasNextPage without a nextCursor"))?;
        Some(cursor.to_string())
    } else {
        None
    };
    Ok((pods.iter().map(parse_pod).collect(), next))
}

/// Follow `GET /v2/pods` pages until the last one. `fetch` gets the cursor (`None` for the
/// first page) and returns that page's body; it's a parameter so the paging logic is tested
/// with fixture pages. Bounded by `max_pages`, and a cursor that doesn't advance is an error
/// (both mean "can't be sure the list is complete" — fail closed, see [`parse_page`]).
/// Pods are de-duplicated by id: the list is newest-first, so a pod created mid-walk can
/// shift an already-seen pod onto the next page.
async fn collect_pages<F, Fut>(mut fetch: F, max_pages: usize) -> Result<Vec<Pod>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut pods: Vec<Pod> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..max_pages {
        let body = fetch(cursor.clone()).await?;
        let (page, next) = parse_page(&body)?;
        for p in page {
            if p.id.is_empty() || seen.insert(p.id.clone()) {
                pods.push(p);
            }
        }
        match next {
            None => return Ok(pods),
            Some(n) if cursor.as_deref() == Some(n.as_str()) => {
                return Err(Error::provider(format!("list pods: pagination cursor didn't advance ({n})")));
            }
            Some(n) => cursor = Some(n),
        }
    }
    Err(Error::provider(format!("list pods: still more pages after {max_pages} — refusing a partial list")))
}

/// Parse one v2 `Pod` object. Defensive: a missing/odd field degrades to `None` rather than
/// failing the whole list (the list's *shape* is checked in [`parse_page`]).
fn parse_pod(v: &Value) -> Pod {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let gpu = v.get("gpu").filter(|g| g.is_object());
    // `ssh.direct` only (never `ssh.proxy`, see the module doc). It's null while the pod is
    // provisioning or stopped; only a complete host+port pair counts as an endpoint.
    let direct = v.pointer("/ssh/direct").filter(|d| d.is_object());
    let host = direct
        .and_then(|d| d.get("host"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(String::from);
    let port = direct
        .and_then(|d| d.get("port"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0);
    let (ssh_ip, ssh_port) = match (host, port) {
        (Some(h), Some(p)) => (Some(h), Some(p)),
        _ => (None, None),
    };
    Pod {
        id: s("id").unwrap_or_default(),
        name: s("name").unwrap_or_default(),
        provider: "runpod".into(),
        // v2's PodStatus enum is already the RUNNING/EXITED/… vocabulary `status.rs` speaks.
        status: s("status").unwrap_or_else(|| "UNKNOWN".into()),
        gpu_type: gpu.and_then(|g| g.get("id")).and_then(loose_string),
        // `count` defaults to 1 in the schema, so a GPU block without one is one GPU.
        gpu_count: gpu.map(|g| g.get("count").and_then(Value::as_u64).map_or(1, |n| n as u32)),
        // `cost` is the live billed $/h (0.0 while EXITED).
        cost_per_hr: v.get("cost").and_then(loose_f64).filter(|c| c.is_finite() && *c >= 0.0),
        ssh_ip,
        ssh_port,
        maintenance: None, // GraphQL-only, see `enrich`
    }
}

/// The cloud tier as v2 spells it. Anything but COMMUNITY/SECURE is refused rather than
/// omitted: an omitted `cloud` means SECURE on v2 (roughly double the community price).
pub fn normalize_cloud(tier: &str) -> Result<&'static str> {
    match tier.trim().to_ascii_uppercase().as_str() {
        "COMMUNITY" => Ok("COMMUNITY"),
        "SECURE" => Ok("SECURE"),
        other => Err(Error::Config(format!(
            "RunPod v2 needs CLOUD_TYPE COMMUNITY or SECURE (got `{other}`) — the tier is always \
             sent, because v2 creates on SECURE when it's omitted"
        ))),
    }
}

/// The account keys from `GET /v2/account/ssh-keys` (`{keys: [...]}`). Fails closed on any
/// other shape: creating pods without the admin key is worse than not creating them.
fn parse_account_keys(body: &Value) -> Result<Vec<String>> {
    let keys = body.get("keys").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("account ssh keys: unexpected response shape: {}", shape_of(body)))
    })?;
    Ok(keys.iter().filter_map(Value::as_str).map(str::trim).filter(|k| !k.is_empty()).map(String::from).collect())
}

/// `PUBLIC_KEY` (newline-separated authorized_keys lines) with the account's keys appended.
/// De-duplicated by key material (`<type> <base64>`), so the same key under a different
/// comment isn't authorized twice; ours keep their order and come first. Blank lines go.
fn merge_public_keys(ours: &str, account: &[String]) -> String {
    let material = |line: &str| line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&str> = Vec::new();
    for line in ours.lines().chain(account.iter().flat_map(|k| k.lines())).map(str::trim) {
        if !line.is_empty() && seen.insert(material(line)) {
            out.push(line);
        }
    }
    out.join("\n")
}

/// The request `env` object, with the account keys merged into `PUBLIC_KEY` when the spec
/// sets one (see the module doc). A spec without `PUBLIC_KEY` is left alone: `startSsh`
/// then injects the account keys server-side. Later duplicates of a key win, as on v1.
fn env_object(env: &[(String, String)], account_keys: &[String]) -> Map<String, Value> {
    env.iter()
        .map(|(k, v)| {
            let v = if k == "PUBLIC_KEY" { merge_public_keys(v, account_keys) } else { v.clone() };
            (k.clone(), Value::String(v))
        })
        .collect()
}

/// Build the `POST /v2/pods` body. Pure, so every field rule is unit-tested — the schema
/// rejects unknown keys (422), so names must match exactly:
/// - `cloud` is ALWAYS present (omitted = SECURE on v2) and must be COMMUNITY/SECURE.
/// - CUDA: `spec.allowed_cuda` → `gpu.allowedCudaVersions` (an exact set; it lives under
///   `gpu` in v2). `minCudaVersion` is never sent — the two together are a 400.
/// - `--bootstrap` (`docker_args`) → `cmd`: like v1's `dockerStartCmd` it replaces the
///   image's CMD and keeps its ENTRYPOINT. Omitted otherwise, so the image's own CMD runs
///   (the arena image starts sshd itself). `entrypoint`/`args` are never sent.
/// - `volume_gb` > 0 → `mounts.persistent {size, path}`; 0 → no mount at all.
/// - `startSsh: true`: with our `PUBLIC_KEY` set it injects nothing (the account keys are
///   merged into ours instead); without one it injects the account keys — v1's behaviour.
fn create_payload(spec: &PodSpec, account_keys: &[String]) -> Result<Value> {
    let cloud = normalize_cloud(&spec.cloud_type)?;
    let mut gpu = json!({ "id": spec.gpu_type, "count": spec.gpu_count });
    if !spec.allowed_cuda.is_empty() {
        gpu["allowedCudaVersions"] = json!(spec.allowed_cuda);
    }
    let ports: Vec<&str> = spec.ports.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let mut payload = json!({
        "name": spec.name,
        "image": spec.image,
        "cloud": cloud,
        "gpu": gpu,
        "disk": spec.disk_gb,
        "ports": ports,
        "env": env_object(&spec.env, account_keys),
        "startSsh": true,
    });
    if let Some(cmd) = &spec.docker_args {
        payload["cmd"] = json!(cmd);
    }
    if spec.volume_gb > 0 {
        payload["mounts"] = json!({ "persistent": { "size": spec.volume_gb, "path": VOLUME_MOUNT_PATH } });
    }
    Ok(payload)
}

/// The `POST /v2/pods/{id}/action` body.
fn action_body(action: &str) -> Value {
    json!({ "action": action })
}

/// The `PATCH /v2/pods/{id}` body for a reimage: new image + the env, which REPLACES the
/// pod's env wholesale (PATCH semantics on `env`, same contract as v1's reimage) — so the
/// caller passes everything to keep, and the account keys are merged into `PUBLIC_KEY`
/// exactly as on create (`startSsh` is create-only, so nothing would re-add them).
fn reimage_payload(image: &str, env: &[(String, String)], account_keys: &[String]) -> Value {
    json!({ "image": image, "env": env_object(env, account_keys) })
}

/// Recover a recreate-able `PodSpec` from `GET /v2/pods/{id}`. Pure, for fixture tests.
/// Unlike v1 (whose `machine` came back empty), v2 reports `gpu` and `cloud`, so the GPU
/// type, count and tier ARE recovered. Best-effort elsewhere: a field RunPod doesn't send
/// is left blank/default for the caller's config/CLI fallback.
/// - `name` is empty for the caller to set; `PUBLIC_KEY`/`MACHINE_NAME` are dropped
///   (`replace`/`reimage` re-seed them).
/// - `cmd` → `docker_args` (so a `--bootstrap` pod's replacement keeps its start script),
///   but only when no ENTRYPOINT override is set — `PodSpec` can't carry one, and a CMD
///   without its ENTRYPOINT would be a different command.
/// - A network-volume mount isn't recreatable from a spec: `volume_gb` stays 0 for it.
/// - The CUDA constraint isn't returned by v2 (only the host's `cudaVersion`), so
///   `allowed_cuda` is empty, as on v1.
fn parse_spec(v: &Value) -> PodSpec {
    let str_at = |ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
    let strings = |ptr: &str| -> Option<Vec<String>> {
        v.pointer(ptr)?.as_array()?.iter().map(|x| x.as_str().map(String::from)).collect()
    };
    let entrypoint_set = strings("/entrypoint").is_some_and(|e| !e.is_empty());
    let docker_args = strings("/cmd").filter(|c| !c.is_empty() && !entrypoint_set);
    let ports = strings("/ports")
        .map(|p| p.join(","))
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
    let num = |ptr: &str| v.pointer(ptr).and_then(Value::as_u64);
    PodSpec {
        name: String::new(),
        image: str_at("/image"),
        gpu_type: str_at("/gpu/id"),
        gpu_count: num("/gpu/count").filter(|n| *n >= 1).unwrap_or(1) as u32,
        // Unknown/absent → "" ("couldn't determine"): the caller falls back to config.
        cloud_type: normalize_cloud(&str_at("/cloud")).map(String::from).unwrap_or_default(),
        disk_gb: num("/disk").unwrap_or(0) as u32,
        volume_gb: num("/mounts/persistent/size").unwrap_or(0) as u32,
        ports,
        env,
        docker_args,
        allowed_cuda: Vec::new(),
    }
}

/// Parse `GET /v2/catalog/gpus` (`{gpus: [...]}`) into the same [`GpuType`] rows the
/// GraphQL catalog yields, so `arena gpus` renders either. Pure, for fixture tests. A
/// price counts only when that tier offers the GPU (`community`/`secure` flags) and is
/// positive; `availability` (NONE/LOW/MEDIUM/HIGH, for the requested cloud) becomes the
/// stock hint in GraphQL's spelling ("Low"…). No `gpus` array is an error, not "no GPUs".
fn parse_catalog(body: &Value) -> Result<Vec<GpuType>> {
    let gpus = body.get("gpus").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("gpu catalog: unexpected response shape: {}", shape_of(body)))
    })?;
    let price = |g: &Value, tier: &str| {
        let offered = g.get(tier).and_then(Value::as_bool).unwrap_or(true);
        g.pointer(&format!("/price/{tier}")).and_then(loose_f64).filter(|p| offered && *p > 0.0)
    };
    let title = |s: &str| {
        let mut c = s.chars();
        c.next().map(|f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect::<String>())
    };
    Ok(gpus
        .iter()
        .filter_map(|g| {
            Some(GpuType {
                id: g.get("id").and_then(loose_string)?,
                display_name: g.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                memory_gb: g.get("memory").and_then(loose_f64).map_or(0, |n| n.max(0.0) as u32),
                community_price: price(g, "community"),
                secure_price: price(g, "secure"),
                stock_status: g.get("availability").and_then(Value::as_str).and_then(title),
            })
        })
        .collect())
}

/// RunPod's GPU catalog from REST v2, with list prices for both tiers and pod stock for
/// `cloud` (COMMUNITY/SECURE). The v2 counterpart of [`runpod::fetch_gpu_types`]: `arena
/// gpus` uses it when `RUNPOD_API=v2` and falls back to GraphQL if it fails.
pub async fn fetch_gpu_types(api_key: &str, cloud: &str) -> Result<Vec<GpuType>> {
    let body = send(catalog_request(&Client::new(), api_key, normalize_cloud(cloud)?), "gpu catalog").await?;
    parse_catalog(&body)
}

#[async_trait]
impl Provider for RunpodV2Provider {
    fn name(&self) -> &'static str {
        "runpod"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        format!("{} [RunPod API v2]", runpod::describe_spec(spec))
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        collect_pages(
            |cursor| {
                let rb = list_request(&self.client, &self.api_key, cursor.as_deref());
                async move { send(rb, "list pods").await }
            },
            MAX_PAGES,
        )
        .await
    }

    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        // v2 already reports GPU type/count and $/h; GraphQL adds the maintenance window.
        runpod::enrich_via_graphql(&self.client, &self.api_key, pods).await
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        // Refuse a bad tier before any request (not even the read-only key fetch).
        normalize_cloud(&spec.cloud_type)?;
        let keys = self.keys_to_merge(&spec.env).await?;
        let payload = create_payload(spec, &keys)?;
        let resp = api_request(&self.client, &self.api_key, Method::POST, base_url("pods")).json(&payload).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        // A 2xx we can't read still means a pod (that bills) probably exists: say so, so
        // nobody "retries" into a duplicate. Such errors are `Other`, which isn't retried.
        let may_exist = |e: Error| {
            Error::provider(format!("{e} — the pod may have been created: check `arena pods list` before retrying"))
        };
        let body = judge_create(status, &text).map_err(|e| if status.is_success() { may_exist(e) } else { e })?;
        let pod = parse_pod(&body);
        if pod.id.is_empty() {
            return Err(may_exist(Error::provider(format!("create pod: HTTP {status} without a pod id: {}", shape_of(&body)))));
        }
        Ok(pod)
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        let rb = api_request(&self.client, &self.api_key, Method::POST, pod_url(id, Some("action"))?);
        send(rb.json(&action_body("stop")), "stop pod").await.map(drop)
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // In-place container restart of a RUNNING pod (keeps the pod and its machine),
        // unlike stop/start. A pod in any other state answers 409.
        let rb = api_request(&self.client, &self.api_key, Method::POST, pod_url(id, Some("action"))?);
        send(rb.json(&action_body("restart")), "restart pod").await.map(drop)
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        // `DELETE /v2/pods/{id}` (documented as equivalent to the `terminate` action):
        // 204 with no body, which `judge` reads as Null; a 200 with a body is fine too.
        let rb = api_request(&self.client, &self.api_key, Method::DELETE, pod_url(id, None)?);
        send(rb, "terminate pod").await.map(drop)
    }

    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        runpod::rename_via_graphql(&self.client, &self.api_key, id, new_name).await
    }

    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> Result<()> {
        let url = pod_url(id, None)?;
        let keys = self.keys_to_merge(env).await?;
        let rb = api_request(&self.client, &self.api_key, Method::PATCH, url).json(&reimage_payload(image, env, &keys));
        send(rb, "reimage pod").await.map(drop)
    }

    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        let body = send(api_request(&self.client, &self.api_key, Method::GET, pod_url(id, None)?), "get pod").await?;
        Ok(parse_spec(&body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RUNNING pod exactly as the v2 OpenAPI example has it (`listPods` 200), plus a
    /// second `22/tcp` mapping in `runtime.ports` to show it isn't what we read.
    fn running_pod() -> Value {
        json!({
            "id": "7h9k2m4n6p", "name": "devtest-apple",
            "image": "nickypro/arena-env:9.1", "args": "", "disk": 100,
            "ports": ["8888/http", "22/tcp"], "env": {"MODEL_NAME": "llama-3"}, "registry": null,
            "status": "RUNNING", "actions": ["stop", "restart", "terminate"],
            "mounts": {"persistent": {"size": 20, "path": "/workspace"}},
            "gpu": {"id": "NVIDIA RTX A4000", "count": 2, "vcpuCount": 16, "memory": 64},
            "cloud": "COMMUNITY", "dataCenterId": "US-KS-2", "cudaVersion": "13.0",
            "ssh": {
                "proxy": {"host": "ssh.runpod.io", "port": 22, "username": "7h9k2m4n6p-64411eb2",
                          "command": "ssh 7h9k2m4n6p-64411eb2@ssh.runpod.io"},
                "direct": {"host": "195.26.233.3", "port": 34446, "username": "root",
                           "command": "ssh root@195.26.233.3 -p 34446"}
            },
            "template": null, "cost": 0.34, "locked": false,
            "globalNetworking": {"enabled": false},
            "runtime": {"uptime": 3600, "ports": [{"private": 22, "public": 34446, "type": "tcp", "ip": "195.26.233.3"}]},
            "createdAt": "2026-06-01T12:00:00Z", "startedAt": "2026-06-01T12:02:00Z"
        })
    }

    /// Fresh from create (the `createPod` 201 example): no direct endpoint yet.
    fn provisioning_pod() -> Value {
        json!({
            "id": "p2", "name": "devtest-autumn", "image": "img", "args": "", "disk": 50,
            "ports": ["22/tcp"], "env": {}, "registry": null, "status": "PROVISIONING",
            "actions": ["stop", "terminate"], "mounts": {},
            "gpu": {"id": "NVIDIA RTX A4000", "count": 1, "vcpuCount": 8, "memory": 32},
            "cloud": "COMMUNITY", "dataCenterId": null, "cudaVersion": null,
            "ssh": {"proxy": null, "direct": null},
            "template": null, "cost": 0.17, "locked": false, "globalNetworking": {"enabled": false},
            "runtime": null, "createdAt": "2026-06-01T12:00:00Z", "startedAt": null
        })
    }

    /// Stopped: `cost` 0.0 and the direct endpoint gone, the GPU still reported.
    fn exited_pod() -> Value {
        json!({
            "id": "p3", "name": "devtest-bloom", "image": "img", "args": "", "disk": 50,
            "ports": ["22/tcp"], "env": {}, "registry": null, "status": "EXITED",
            "actions": ["start", "terminate"], "mounts": {},
            "gpu": {"id": "NVIDIA GeForce RTX 3090", "count": 1, "vcpuCount": 8, "memory": 32},
            "cloud": "SECURE", "dataCenterId": "EU-RO-1", "cudaVersion": "12.8",
            "ssh": {"proxy": {"host": "ssh.runpod.io", "port": 22, "username": "x", "command": "ssh x@ssh.runpod.io"},
                    "direct": null},
            "template": null, "cost": 0.0, "locked": false, "globalNetworking": {"enabled": false},
            "runtime": null, "createdAt": "2026-06-01T12:00:00Z", "startedAt": null
        })
    }

    fn page(pods: Vec<Value>, next: Option<&str>) -> Value {
        json!({ "pods": pods, "pagination": { "nextCursor": next, "hasNextPage": next.is_some() } })
    }

    #[test]
    fn parse_pod_running_uses_ssh_direct_never_the_proxy() {
        let p = parse_pod(&running_pod());
        assert_eq!(
            p,
            Pod {
                id: "7h9k2m4n6p".into(),
                name: "devtest-apple".into(),
                provider: "runpod".into(),
                status: "RUNNING".into(),
                gpu_type: Some("NVIDIA RTX A4000".into()),
                gpu_count: Some(2),
                cost_per_hr: Some(0.34),
                ssh_ip: Some("195.26.233.3".into()),
                ssh_port: Some(34446),
                maintenance: None,
            }
        );
    }

    #[test]
    fn parse_pod_without_direct_endpoint_has_none_even_with_a_proxy() {
        let p = parse_pod(&provisioning_pod());
        assert_eq!((p.status.as_str(), p.ssh_ip, p.ssh_port), ("PROVISIONING", None, None));
        assert_eq!(p.gpu_count, Some(1));
        // PROVISIONING with the whole `ssh` object null. The schema has `ssh` required (only
        // `direct`/`proxy` nullable), but parsing is defensive: still a pod, no endpoint yet.
        let mut bare = provisioning_pod();
        bare["ssh"] = Value::Null;
        let b = parse_pod(&bare);
        assert_eq!((b.id.as_str(), b.status.as_str(), b.ssh_ip, b.ssh_port), (p.id.as_str(), "PROVISIONING", None, None));

        // EXITED: the ssh proxy is still offered — it must NOT become the endpoint.
        let e = parse_pod(&exited_pod());
        assert_eq!((e.status.as_str(), e.ssh_ip, e.ssh_port), ("EXITED", None, None));
        assert_eq!(e.cost_per_hr, Some(0.0));
        assert_eq!(e.gpu_type.as_deref(), Some("NVIDIA GeForce RTX 3090"));
    }

    #[test]
    fn parse_pod_degrades_odd_fields_to_none() {
        // Half an endpoint is no endpoint; an out-of-range port is no port.
        let mut v = running_pod();
        v["ssh"]["direct"]["port"] = json!(70000);
        let p = parse_pod(&v);
        assert_eq!((p.ssh_ip, p.ssh_port), (None, None));
        v["ssh"]["direct"] = json!({"host": "", "port": 22});
        assert_eq!(parse_pod(&v).ssh_ip, None);
        // A CPU pod (no `gpu`), no cost, no status: Nones and UNKNOWN, no panic.
        let cpu = parse_pod(&json!({"id": "c1", "name": "n", "cpu": {"flavorId": "cpu3c"}}));
        assert_eq!((cpu.gpu_type, cpu.gpu_count, cpu.cost_per_hr), (None, None, None));
        assert_eq!(cpu.status, "UNKNOWN");
        // A GPU block without `count` is the schema default: one GPU.
        assert_eq!(parse_pod(&json!({"id": "g", "gpu": {"id": "NVIDIA L4"}})).gpu_count, Some(1));
    }

    #[test]
    fn parse_page_reads_pods_and_cursor() {
        let (pods, next) = parse_page(&page(vec![running_pod(), provisioning_pod()], Some("c2"))).unwrap();
        assert_eq!(pods.len(), 2);
        assert_eq!(next.as_deref(), Some("c2"));
        let (pods, next) = parse_page(&page(vec![], None)).unwrap();
        assert!(pods.is_empty() && next.is_none()); // a real, empty fleet is fine
    }

    #[test]
    fn parse_page_fails_closed_on_schema_drift() {
        // None of these may read as "zero pods" (or "all pods") — that would look like every
        // pod was terminated and drop their proxy forwards.
        let cases = [
            json!({}),
            json!([]),                                                        // a v1-style bare array
            json!({"pods": null, "pagination": {"hasNextPage": false}}),
            json!({"pods": []}),                                              // no pagination block
            json!({"pods": [], "pagination": null}),
            json!({"pods": [], "pagination": {"nextCursor": null}}),          // no hasNextPage
            json!({"pods": [], "pagination": {"hasNextPage": true, "nextCursor": null}}),
            json!({"pods": [], "pagination": {"hasNextPage": true, "nextCursor": ""}}),
            json!(null),
            json!("ok"),
        ];
        for body in cases {
            let err = parse_page(&body).unwrap_err().to_string();
            assert!(err.contains("unexpected response shape"), "{body}: {err}");
        }
    }

    /// A shape error names the keys it saw, never the values (pod `env` can hold tokens).
    #[test]
    fn shape_errors_never_echo_values() {
        let mut pod = running_pod();
        pod["env"] = json!({"HF_TOKEN": "hf_SECRET"});
        let e = parse_page(&json!({"pods": [pod], "pagination": null})).unwrap_err().to_string();
        assert!(e.contains("no pagination") && e.contains("[pagination, pods]"), "{e}");
        assert!(!e.contains("SECRET") && !e.contains("195.26"), "{e}");
        assert_eq!(shape_of(&json!([1, 2])), "array of 2");
        assert_eq!(shape_of(&Value::Null), "null/empty");
    }

    /// Drive [`collect_pages`] with canned pages, recording the cursors it asked for.
    async fn walk(pages: Vec<Result<Value>>, max: usize) -> (Result<Vec<Pod>>, Vec<Option<String>>) {
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let pages = std::sync::Arc::new(Mutex::new(pages.into_iter()));
        let (a, p) = (asked.clone(), pages.clone());
        let res = collect_pages(
            move |cursor| {
                a.lock().unwrap().push(cursor);
                let next = p.lock().unwrap().next().expect("asked for more pages than scripted");
                async move { next }
            },
            max,
        )
        .await;
        let asked = asked.lock().unwrap().clone();
        (res, asked)
    }

    #[tokio::test]
    async fn collect_pages_follows_the_cursor_across_two_pages() {
        let (res, asked) = walk(
            vec![Ok(page(vec![running_pod()], Some("cur-2"))), Ok(page(vec![provisioning_pod(), exited_pod()], None))],
            MAX_PAGES,
        )
        .await;
        let ids: Vec<String> = res.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["7h9k2m4n6p", "p2", "p3"]);
        assert_eq!(asked, vec![None, Some("cur-2".to_string())]);
    }

    #[tokio::test]
    async fn collect_pages_dedups_a_pod_shifted_onto_the_next_page() {
        let (res, _) = walk(
            vec![Ok(page(vec![running_pod()], Some("c2"))), Ok(page(vec![running_pod(), exited_pod()], None))],
            MAX_PAGES,
        )
        .await;
        assert_eq!(res.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn collect_pages_fails_closed() {
        // A bad second page fails the whole listing (no partial list).
        let (res, _) = walk(vec![Ok(page(vec![running_pod()], Some("c2"))), Ok(json!({"pods": []}))], MAX_PAGES).await;
        assert!(res.unwrap_err().to_string().contains("unexpected response shape"));
        // So does an HTTP error on any page.
        let (res, _) =
            walk(vec![Ok(page(vec![], Some("c2"))), Err(Error::provider("list pods HTTP 500"))], MAX_PAGES).await;
        assert!(res.is_err());
        // A cursor that doesn't advance would loop forever.
        let (res, asked) = walk(vec![Ok(page(vec![], Some("c2"))), Ok(page(vec![], Some("c2")))], MAX_PAGES).await;
        assert!(res.unwrap_err().to_string().contains("didn't advance"));
        assert_eq!(asked.len(), 2);
        // More pages than the bound: an error, not the first N pages.
        let (res, asked) =
            walk(vec![Ok(page(vec![running_pod()], Some("a"))), Ok(page(vec![exited_pod()], Some("b")))], 2).await;
        assert!(res.unwrap_err().to_string().contains("refusing a partial list"));
        assert_eq!(asked.len(), 2);
    }

    #[test]
    fn judge_classifies_responses() {
        use ProviderErrorKind as K;
        let pod = running_pod().to_string();
        let problem = |status: u16, detail: &str| {
            json!({"title": "x", "status": status, "detail": detail}).to_string()
        };
        // (case, status, body, Ok?, error kind)
        let cases: Vec<(&str, u16, String, bool, Option<K>)> = vec![
            ("201 created pod", 201, pod.clone(), true, None),
            ("200 action pod", 200, pod, true, None),
            ("204 terminate", 204, String::new(), true, None),
            ("200 non-JSON", 200, "<html>ok</html>".into(), false, Some(K::Other)),
            ("400 capacity (v1 wording)", 400, problem(400, "There are no instances currently available"), false, Some(K::Capacity)),
            ("400 capacity (placement)", 400, problem(400, "this GPU and data center combination could not be placed"), false, Some(K::Capacity)),
            ("400 rule violation", 400, problem(400, "allowedCudaVersions and minCudaVersion are mutually exclusive"), false, Some(K::Other)),
            ("402 balance is not capacity", 402, problem(402, "Insufficient balance"), false, Some(K::Other)),
            ("401", 401, problem(401, "missing bearer token"), false, Some(K::Auth)),
            ("403 (key lacks access)", 403, problem(403, "access denied"), false, Some(K::Auth)),
            ("409 bad action", 409, problem(409, "action not valid for current pod status"), false, Some(K::Other)),
            ("422", 422, problem(422, "Request validation failed."), false, Some(K::Other)),
            ("429", 429, problem(429, "rate limit exceeded for the minute window"), false, Some(K::RateLimited)),
            ("502 html", 502, "<html>Bad Gateway</html>".into(), false, Some(K::Transient)),
            ("500 capacity", 500, problem(500, "no instances available"), false, Some(K::Capacity)),
        ];
        for (case, code, body, ok, kind) in &cases {
            let r = judge(StatusCode::from_u16(*code).unwrap(), body, "get pod");
            assert_eq!(r.is_ok(), *ok, "{case}: {r:?}");
            if let Err(e) = r {
                assert_eq!(e.kind(), *kind, "{case}: {e}");
                assert!(!crate::retry::is_retryable(&e) || matches!(kind, Some(K::Transient | K::RateLimited)), "{case}");
            }
        }
        // `POST /v2/pods` reads every response the same, except its documented 403 ("Your
        // account cannot access the requested pool. Skip this candidate, keep going."):
        // Denied, so placement tries the next option instead of aborting the run.
        for (case, code, body, ok, kind) in &cases {
            let want = if *code == 403 { Some(K::Denied) } else { *kind };
            let r = judge_create(StatusCode::from_u16(*code).unwrap(), body);
            assert_eq!(r.is_ok(), *ok, "create {case}: {r:?}");
            if let Err(e) = r {
                assert_eq!(e.kind(), want, "create {case}: {e}");
                assert!(e.to_string().contains("create pod"), "create {case}: {e}");
                assert!(!crate::retry::is_retryable(&e) || matches!(want, Some(K::Transient | K::RateLimited)), "{case}");
            }
        }
        let pool = problem(403, "your account cannot access the requested pool");
        let e = judge_create(StatusCode::FORBIDDEN, &pool).unwrap_err();
        assert_eq!(e.kind(), Some(K::Denied));
        assert!(e.to_string().contains("cannot access the requested pool"), "{e}");
        assert_eq!(judge(StatusCode::NO_CONTENT, "", "terminate pod").unwrap(), Value::Null);
        // The problem's `detail` makes it into the message.
        let e = judge(StatusCode::NOT_FOUND, &problem(404, "pod not found"), "get pod").unwrap_err();
        assert!(e.to_string().contains("get pod HTTP 404") && e.to_string().contains("pod not found"), "{e}");
    }

    fn spec() -> PodSpec {
        PodSpec {
            name: "devtest-apple".into(),
            image: "nickypro/arena-env:9.1".into(),
            gpu_type: "NVIDIA RTX A4000".into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            disk_gb: 100,
            volume_gb: 0,
            ports: "8888/http, 22/tcp".into(),
            env: vec![
                ("PUBLIC_KEY".into(), "ssh-ed25519 AAAAcohort devtest\nssh-ed25519 AAAAdeploy deploy".into()),
                ("MACHINE_NAME".into(), "devtest-apple".into()),
            ],
            docker_args: None,
            allowed_cuda: Vec::new(),
        }
    }

    #[test]
    fn create_payload_always_sends_cloud_and_matches_the_schema() {
        let p = create_payload(&spec(), &[]).unwrap();
        assert_eq!(
            p,
            json!({
                "name": "devtest-apple",
                "image": "nickypro/arena-env:9.1",
                "cloud": "COMMUNITY",
                "gpu": {"id": "NVIDIA RTX A4000", "count": 1},
                "disk": 100,
                "ports": ["8888/http", "22/tcp"],
                "env": {"PUBLIC_KEY": "ssh-ed25519 AAAAcohort devtest\nssh-ed25519 AAAAdeploy deploy",
                        "MACHINE_NAME": "devtest-apple"},
                "startSsh": true,
            })
        );
        // Lower-case tiers are normalized; SECURE passes through.
        let mut s = spec();
        s.cloud_type = "secure".into();
        assert_eq!(create_payload(&s, &[]).unwrap()["cloud"], "SECURE");
        // Never v1 field names (the schema rejects unknown keys with a 422).
        for v1_key in ["imageName", "gpuTypeIds", "gpuCount", "cloudType", "containerDiskInGb", "volumeInGb", "dockerStartCmd"] {
            assert!(p.get(v1_key).is_none(), "{v1_key}");
        }
    }

    #[test]
    fn create_payload_refuses_an_unknown_cloud_instead_of_defaulting_to_secure() {
        for tier in ["", "ALL", "communty"] {
            let mut s = spec();
            s.cloud_type = tier.into();
            let e = create_payload(&s, &[]).unwrap_err();
            assert!(matches!(e, Error::Config(_)) && e.to_string().contains("CLOUD_TYPE"), "{tier}: {e}");
        }
    }

    #[test]
    fn create_payload_cuda_rules() {
        let mut s = spec();
        let p = create_payload(&s, &[]).unwrap();
        assert!(p["gpu"].get("allowedCudaVersions").is_none());
        s.allowed_cuda = vec!["13.0".into(), "12.9".into()];
        let p = create_payload(&s, &[]).unwrap();
        // An exact set, under `gpu` (not top-level as on v1) — and never a floor beside it.
        assert_eq!(p["gpu"]["allowedCudaVersions"], json!(["13.0", "12.9"]));
        assert!(p["gpu"].get("minCudaVersion").is_none());
        assert!(p.get("allowedCudaVersions").is_none() && p.get("minCudaVersion").is_none());
    }

    #[test]
    fn create_payload_volume_becomes_a_persistent_mount() {
        let mut s = spec();
        assert!(create_payload(&s, &[]).unwrap().get("mounts").is_none()); // 0 = no mount at all
        s.volume_gb = 50;
        assert_eq!(
            create_payload(&s, &[]).unwrap()["mounts"],
            json!({"persistent": {"size": 50, "path": "/workspace"}})
        );
    }

    #[test]
    fn create_payload_bootstrap_is_cmd_not_entrypoint() {
        let mut s = spec();
        s.docker_args = Some(vec!["bash".into(), "-c".into(), "apt-get install -y openssh-server && /usr/sbin/sshd -D".into()]);
        let p = create_payload(&s, &[]).unwrap();
        assert_eq!(p["cmd"], json!(["bash", "-c", "apt-get install -y openssh-server && /usr/sbin/sshd -D"]));
        // CMD only: the image's ENTRYPOINT is kept, as v1's dockerStartCmd did.
        assert!(p.get("entrypoint").is_none() && p.get("args").is_none());
        assert!(create_payload(&spec(), &[]).unwrap().get("cmd").is_none());
    }

    #[test]
    fn create_payload_merges_account_keys_into_public_key() {
        let account = vec![
            "ssh-ed25519 AAAAadmin arena_admin".to_string(),
            "ssh-ed25519 AAAAcohort the-same-key-another-comment".to_string(), // dup by material
        ];
        let p = create_payload(&spec(), &account).unwrap();
        assert_eq!(
            p["env"]["PUBLIC_KEY"],
            "ssh-ed25519 AAAAcohort devtest\nssh-ed25519 AAAAdeploy deploy\nssh-ed25519 AAAAadmin arena_admin"
        );
        assert_eq!(p["env"]["MACHINE_NAME"], "devtest-apple"); // other env untouched
        // No PUBLIC_KEY of ours: none is invented (startSsh injects the account keys).
        let mut s = spec();
        s.env.retain(|(k, _)| k != "PUBLIC_KEY");
        assert!(create_payload(&s, &account).unwrap()["env"].get("PUBLIC_KEY").is_none());
    }

    #[test]
    fn merge_public_keys_dedups_by_key_material() {
        assert_eq!(merge_public_keys("ssh-ed25519 A a\n\n  ssh-ed25519 A again \n", &[]), "ssh-ed25519 A a");
        assert_eq!(
            merge_public_keys("ssh-ed25519 A a", &["ssh-rsa B b\nssh-ed25519 C c".into(), "ssh-rsa B dup".into(), "  ".into()]),
            "ssh-ed25519 A a\nssh-rsa B b\nssh-ed25519 C c"
        );
        assert_eq!(merge_public_keys("", &["ssh-ed25519 Z z".into()]), "ssh-ed25519 Z z");
    }

    #[test]
    fn parse_account_keys_reads_keys_and_fails_closed() {
        let v = json!({"keys": ["ssh-ed25519 AAAA me@example.com", "", 7, "  ssh-rsa BBBB x  "]});
        assert_eq!(parse_account_keys(&v).unwrap(), vec!["ssh-ed25519 AAAA me@example.com", "ssh-rsa BBBB x"]);
        assert!(parse_account_keys(&json!({"keys": []})).unwrap().is_empty()); // none registered
        for bad in [json!({}), json!({"keys": null}), json!(["ssh-ed25519 AAAA"]), json!(null)] {
            assert!(parse_account_keys(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn action_and_reimage_bodies() {
        for a in ["stop", "restart", "terminate"] {
            assert_eq!(action_body(a), json!({"action": a}));
        }
        let env = vec![("MACHINE_NAME".into(), "devtest-apple".into()), ("PUBLIC_KEY".into(), "ssh-ed25519 K1 k1".into())];
        let v = reimage_payload("img:9", &env, &["ssh-ed25519 ADMIN admin".into()]);
        assert_eq!(
            v,
            json!({"image": "img:9", "env": {"MACHINE_NAME": "devtest-apple",
                                              "PUBLIC_KEY": "ssh-ed25519 K1 k1\nssh-ed25519 ADMIN admin"}})
        );
        assert!(v.get("imageName").is_none() && v.get("name").is_none()); // never a v1 key, never a rename
    }

    #[test]
    fn parse_spec_recovers_gpu_cloud_and_volume_and_drops_identity_env() {
        let mut v = running_pod();
        v["env"] = json!({"PUBLIC_KEY": "ssh-ed25519 AAAA", "MACHINE_NAME": "devtest-apple", "WANDB_KEY": "w"});
        let s = parse_spec(&v);
        assert_eq!(s.name, ""); // caller fills
        assert_eq!(s.image, "nickypro/arena-env:9.1");
        assert_eq!(s.gpu_type, "NVIDIA RTX A4000"); // recoverable on v2 (not on v1)
        assert_eq!(s.gpu_count, 2);
        assert_eq!(s.cloud_type, "COMMUNITY"); // likewise
        assert_eq!((s.disk_gb, s.volume_gb), (100, 20));
        assert_eq!(s.ports, "8888/http,22/tcp");
        assert_eq!(s.env, vec![("WANDB_KEY".to_string(), "w".to_string())]);
        assert!(s.docker_args.is_none()); // `args: ""` = no start-command override
        assert!(s.allowed_cuda.is_empty()); // the host's cudaVersion is not the constraint
    }

    #[test]
    fn parse_spec_recovers_bootstrap_cmd_only_without_an_entrypoint_override() {
        let mut v = running_pod();
        v["cmd"] = json!(["bash", "-c", "sshd -D"]);
        v["entrypoint"] = json!([]);
        assert_eq!(parse_spec(&v).docker_args, Some(vec!["bash".into(), "-c".into(), "sshd -D".into()]));
        v["entrypoint"] = json!(["/bin/sh"]);
        assert_eq!(parse_spec(&v).docker_args, None);
    }

    #[test]
    fn parse_spec_defaults_when_fields_absent() {
        let s = parse_spec(&json!({"id": "x", "mounts": {"network": [{"volumeId": "v", "path": "/workspace"}]}}));
        assert_eq!((s.image.as_str(), s.gpu_type.as_str(), s.cloud_type.as_str()), ("", "", ""));
        assert_eq!((s.gpu_count, s.disk_gb, s.volume_gb), (1, 0, 0)); // network volume: not recreatable
        assert_eq!(s.ports, "8888/http,22/tcp");
        assert!(s.env.is_empty() && s.docker_args.is_none());
        // An unexpected tier string is "unknown", so the caller falls back to config.
        assert_eq!(parse_spec(&json!({"cloud": "MOON"})).cloud_type, "");
    }

    /// A provider whose HTTP client can reach nothing (all traffic goes to a closed local
    /// port), so these tests can never touch the real API even if a guard regresses.
    fn offline() -> RunpodV2Provider {
        let proxy = reqwest::Proxy::all("http://127.0.0.1:9").unwrap();
        let client = Client::builder().no_proxy().proxy(proxy).build().unwrap();
        RunpodV2Provider { api_key: "rpa_TEST".into(), client, account_keys: Mutex::new(None) }
    }

    /// The guards fire before any request: a bad tier (which v2 would turn into SECURE if
    /// omitted) and an empty id (which would address the pod *list*).
    #[tokio::test]
    async fn guards_refuse_before_any_request() {
        let p = offline();
        let mut s = spec();
        s.cloud_type = "ALL".into();
        assert!(matches!(p.create_pod(&s).await, Err(Error::Config(_))));
        for r in [p.stop_pod("").await, p.restart_pod(" ").await, p.terminate_pod("").await, p.reimage_pod("", "img", &[]).await] {
            assert!(r.unwrap_err().to_string().contains("empty pod id"));
        }
        assert!(p.pod_spec("").await.unwrap_err().to_string().contains("empty pod id"));
    }

    /// The account keys are fetched only when we set `PUBLIC_KEY` (otherwise `startSsh`
    /// injects them server-side), a failed fetch fails the caller, and a fetched set is
    /// reused for the rest of the batch.
    #[tokio::test]
    async fn account_keys_fetched_only_when_needed_and_cached() {
        let p = offline();
        let ours = [("PUBLIC_KEY".to_string(), "ssh-ed25519 K k".to_string())];
        assert!(p.keys_to_merge(&[("MACHINE_NAME".into(), "x".into())]).await.unwrap().is_empty());
        assert!(p.keys_to_merge(&ours).await.is_err()); // uncached → fetch → fails (offline)
        *p.account_keys.lock().unwrap() = Some(vec!["ssh-ed25519 ADMIN admin".into()]);
        assert_eq!(p.keys_to_merge(&ours).await.unwrap(), vec!["ssh-ed25519 ADMIN admin"]);
    }

    #[test]
    fn requests_target_v2_with_the_key_only_in_the_header() {
        let c = Client::new();
        let check = |rb: RequestBuilder, method: Method, url: &str| {
            let req = rb.build().unwrap();
            assert_eq!(req.method(), method);
            assert_eq!(req.url().as_str(), url);
            assert!(!req.url().as_str().contains("SECRET"));
            assert_eq!(req.headers()["authorization"], "Bearer rpa_SECRET");
        };
        check(list_request(&c, "rpa_SECRET", None), Method::GET, "https://api.runpod.io/v2/pods?limit=1000");
        check(
            list_request(&c, "rpa_SECRET", Some("abc+/=")),
            Method::GET,
            "https://api.runpod.io/v2/pods?limit=1000&cursor=abc%2B%2F%3D",
        );
        let action = pod_url("7h9k2m4n6p", Some("action")).unwrap();
        check(api_request(&c, "rpa_SECRET", Method::POST, action), Method::POST, "https://api.runpod.io/v2/pods/7h9k2m4n6p/action");
        check(
            api_request(&c, "rpa_SECRET", Method::DELETE, pod_url("p1", None).unwrap()),
            Method::DELETE,
            "https://api.runpod.io/v2/pods/p1",
        );
        check(
            api_request(&c, "rpa_SECRET", Method::GET, base_url("account/ssh-keys")),
            Method::GET,
            "https://api.runpod.io/v2/account/ssh-keys",
        );
        check(
            catalog_request(&c, "rpa_SECRET", "COMMUNITY"),
            Method::GET,
            "https://api.runpod.io/v2/catalog/gpus?include=AVAILABILITY&product=POD&cloud=COMMUNITY",
        );
    }

    #[test]
    fn pod_url_keeps_an_odd_id_inside_its_own_segment() {
        assert_eq!(pod_url("../account/ssh-keys", None).unwrap().as_str(), "https://api.runpod.io/v2/pods/..%2Faccount%2Fssh-keys");
        assert_eq!(pod_url("a b", Some("action")).unwrap().as_str(), "https://api.runpod.io/v2/pods/a%20b/action");
        assert!(pod_url("", None).is_err());
        assert!(pod_url("  ", Some("action")).is_err());
        // Dot-segments would resolve back to the list endpoint (`pods/..` → `/v2/pods`).
        for dots in [".", "..", " .. ", ". "] {
            assert!(pod_url(dots, None).is_err(), "{dots:?}");
            assert!(pod_url(dots, Some("action")).is_err(), "{dots:?}");
        }
        // Percent-encoded dots and dot-containing ids stay inside their own segment.
        for odd in ["%2e%2e", "%2E.", ".%2e", "%2e", "...", "a.b"] {
            let u = pod_url(odd, Some("action")).unwrap();
            let segs: Vec<&str> = u.path_segments().unwrap().collect();
            assert_eq!((segs.len(), segs[0], segs[1], segs[3]), (4, "v2", "pods", "action"), "{odd:?} -> {u}");
        }
    }

    /// The `listGpuTypes` example shape, plus a GPU only on secure and one with no stock.
    #[test]
    fn parse_catalog_fixture() {
        let v = json!({"gpus": [
            {"id": "NVIDIA GeForce RTX 4090", "name": "RTX 4090", "pool": "ADA_24", "manufacturer": "NVIDIA",
             "memory": 24, "secure": true, "community": true,
             "price": {"secure": 0.44, "community": 0.31, "serverless": 1.1},
             "maxCount": {"secure": 8, "community": 4}, "availability": "HIGH",
             "dataCenters": [{"id": "US-KS-2", "name": "US Kansas 2", "availability": "HIGH"}]},
            {"id": "NVIDIA H100 80GB HBM3", "name": "H100 SXM", "pool": null, "manufacturer": "NVIDIA",
             "memory": 80, "secure": true, "community": false,
             "price": {"secure": 2.69, "community": 1.99}, "maxCount": {"secure": 8, "community": 0},
             "availability": "NONE"},
            {"id": "NVIDIA RTX A4000", "name": "RTX A4000", "memory": 16, "secure": true, "community": true,
             "price": {"secure": 0.25, "community": 0.0}},
            {"name": "no id — skipped"}
        ]});
        let t = parse_catalog(&v).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(
            t[0],
            GpuType {
                id: "NVIDIA GeForce RTX 4090".into(),
                display_name: "RTX 4090".into(),
                memory_gb: 24,
                community_price: Some(0.31),
                secure_price: Some(0.44),
                stock_status: Some("High".into()),
            }
        );
        // `community: false` hides a quoted community price; NONE stock is reported as such.
        assert_eq!((t[1].community_price, t[1].secure_price, t[1].stock_status.as_deref()), (None, Some(2.69), Some("None")));
        // A 0 price is "not offered", and no availability (not requested) is no hint.
        assert_eq!((t[2].community_price, t[2].stock_status.clone()), (None, None));
        assert!(parse_catalog(&json!({"data": []})).is_err());
        assert!(parse_catalog(&json!({"gpus": []})).unwrap().is_empty());
    }
}
