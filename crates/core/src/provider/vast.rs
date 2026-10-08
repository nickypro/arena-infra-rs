//! Vast.ai backend, talking to the v0 REST API (https://console.vast.ai/api/v0).
//!
//! Vast's model differs from RunPod's named-pod model: you don't create a machine by name,
//! you *rent an offer*. So [`create_pod`](VastProvider::create_pod) searches the marketplace
//! for the cheapest rentable offer that satisfies the [`PodSpec`] (GPU type/count, disk,
//! CUDA floor, `--max-price`) and rents it, carrying the machine name as the instance
//! `label` — the rest of the system stays provider-agnostic.
//!
//! What the API wants (docs.vast.ai and the official vast CLI, plus read-only live probes on
//! 2026-10-08):
//! - **Search** — `PUT /search/asks/` takes the filters wrapped as `{"q": {…}}`: the flat
//!   body it used to take is now a 400 ("rentable: Extra inputs are not permitted"). And
//!   `gpu_name` is matched exactly against Vast's display name, *with spaces*: `RTX 3090`
//!   finds offers, `RTX_3090` finds none ([`vast_gpu_name`]). The same search prices
//!   placement's options ([`VastProvider::quote`]), so `arena offers` and `--max-price` work.
//! - **Create** — `PUT /asks/{id}/` with runtype `ssh_direct`. Vast's SSH/Jupyter runtypes
//!   *replace* the image's ENTRYPOINT with Vast's own launcher (sshd, the container's 22
//!   mapped to a port on the host's public IP, then our `onstart`); only runtype `args`
//!   keeps the image's entrypoint — and then there's no SSH unless the image runs its own
//!   and maps the port itself. So nothing here leans on the image's start script: SSH comes
//!   from Vast, keys from the API plus `onstart`. `cancel_unavail` turns a host that can't
//!   start the instance now into an error (capacity, next option) instead of a stopped
//!   instance holding the name.
//! - **SSH keys** — attached to *each* instance (`POST /instances/{id}/ssh/`) right after
//!   the rent, and written by the instance's own `onstart` too, so one failing doesn't lock
//!   us out ([`Provider::authorize_ssh_keys`] repeats the attach when a pod refuses the key).
//!   Never registered on the account: the account's keys are the operator's own (Vast adds
//!   them to every new instance by itself), and a cohort key there would outlive the cohort.
//! - **SSH endpoint** — the *direct* one, the host's IP and mapped port: Vast's shared SSH
//!   proxy breaks scp/rsync/port-forwards, and only the host IP says which machine a pod is
//!   on (`up --check`'s bad-host rejection, `test --deep`'s "same host?"). See
//!   [`ssh_endpoint`].
//!
//! As with RunPod, fields are read defensively from `serde_json::Value` (a missing detail
//! degrades to `None`), but a 2xx whose *shape* is wrong — no `offers`/`instances` array, a
//! rent without an instance id — is an error, never read as "nothing there".

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{json, Value};

use super::Provider;
use crate::error::{looks_like_capacity, Error, Result};
use crate::fleet::fmt_money;
use crate::http::{send_json, send_ok, status_error};
use crate::placement::{milli, PriceBook};
use crate::pod::{Pod, PodSpec};
use crate::retry::{retrying, RetryPolicy};

const BASE: &str = "https://console.vast.ai/api/v0";

/// Offers per search, cheapest first: plenty to re-filter client-side and still find one.
const SEARCH_LIMIT: u32 = 64;

/// The runtype instances are created with: Vast's SSH launcher with a direct port (the
/// documented values are `ssh_direct`/`ssh_proxy`/`jupyter_direct`/`jupyter_proxy`/`args`).
const RUNTYPE: &str = "ssh_direct";

/// Vast's limit on `onstart` ("limited to 4048 characters").
const ONSTART_MAX: usize = 4048;

pub struct VastProvider {
    api_key: String,
    client: Client,
    base: String,
}

impl VastProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
        }
    }

    /// Override the API base URL (used by tests; also lets an operator point at a
    /// proxy). The default is the public Vast.ai endpoint.
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.api_key)
    }

    /// Search the marketplace and return the cheapest offer that fits `spec`, or `None` if
    /// nothing suitable is rentable right now. Read-only.
    pub async fn cheapest_offer(&self, spec: &PodSpec) -> Result<Option<Offer>> {
        // Status first, then decode (crate::http): an auth/HTML error stays classified.
        let body = send_json(
            self.auth(self.client.put(format!("{}/search/asks/", self.base)).json(&search_body(spec))),
            "vast search",
        )
        .await?;
        Ok(select_offer(offers_array(&body)?, spec))
    }

    /// Placement's live prices: for each GPU option, the cheapest offer that fits `base`
    /// with that GPU right now (`Ok(None)` = none). One search per Vast GPU name and count,
    /// cached for this call — two options that are the same card on Vast share it. Each
    /// search is bounded by `limit` (the HTTP client has no timeout of its own; a stalled
    /// search must not hang `offers` or a create). Read-only; a failure is that option's
    /// `Err` (shown, and the option goes unpriced), never the whole plan's.
    pub async fn quote(&self, gpus: &[String], base: &PodSpec, limit: Duration) -> Vec<(String, OfferQuote)> {
        let mut cache: HashMap<(String, u32), OfferQuote> = HashMap::new();
        let mut out = Vec::new();
        for gpu in gpus {
            let key = (gpu_key(&vast_gpu_name(gpu)), base.gpu_count);
            if !cache.contains_key(&key) {
                // The plan prices what an uncapped search finds; the cap is the plan's to apply.
                let spec = PodSpec { gpu_type: gpu.clone(), max_price: None, ..base.clone() };
                let got = match tokio::time::timeout(limit, self.cheapest_offer(&spec)).await {
                    Ok(Ok(offer)) => Ok(offer),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
                };
                cache.insert(key.clone(), got);
            }
            out.push((gpu.clone(), cache[&key].clone()));
        }
        out
    }

    /// Attach `keys` to instance `id`, one `POST /instances/{id}/ssh/` per key (Vast takes
    /// one key per call). Idempotent: a key Vast reports as already attached counts as done.
    /// Throttling/5xx are retried (attaching a key twice is harmless); any other failure
    /// stops at that key.
    async fn attach_keys(&self, id: &str, keys: &[String]) -> Result<()> {
        if id.trim().is_empty() {
            return Err(Error::provider("vast attach ssh key: no instance id"));
        }
        let policy = RetryPolicy::default();
        for key in keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()) {
            retrying(&policy, || self.attach_key(id, key)).await?;
        }
        Ok(())
    }

    async fn attach_key(&self, id: &str, key: &str) -> Result<()> {
        let resp = self
            .auth(self.client.post(format!("{}/instances/{}/ssh/", self.base, id)).json(&json!({ "ssh_key": key })))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        attach_outcome(status, &text)
    }
}

/// One GPU option's plan-time quote: its cheapest fitting offer (`None`: none right now), or
/// why the search failed.
pub type OfferQuote = std::result::Result<Option<Offer>, String>;

/// A rentable marketplace offer, distilled to what we need to choose, price and rent one.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    /// The ask id passed to `PUT /asks/{id}/`.
    pub id: u64,
    /// Vast's display name, e.g. `RTX 3090`.
    pub gpu_name: String,
    pub num_gpus: u32,
    pub disk_space: f64,
    /// $/h for the whole instance at the disk size searched with (`allocated_storage`):
    /// GPU plus storage — what the instance bills while running (bandwidth aside).
    pub dph_total: f64,
    pub cuda_max_good: Option<f64>,
}

/// Vast's display name for a GPU type — what its search's `gpu_name` matches, exactly.
///
/// The configured type is normally RunPod's id (`NVIDIA GeForce RTX 3090`, from `--gpu
/// 3090` or `GPU_TYPE`), so known ids map through a table of Vast's own spellings (as Vast
/// lists them and dstack's gpuhunt maps them: `RTX A4000`, `RTX 4000Ada`, `A100 SXM4`,
/// `H100 SXM`). Anything else falls back to the id minus its vendor words, with spaces
/// (`NVIDIA GeForce RTX 3060` → `RTX 3060`, `… Ada Generation` → `…Ada`, a bare `3060` →
/// `RTX 3060`); a name already in Vast's form passes through. Never underscores: `RTX_3090`
/// finds no offers (live, 2026-10-08) — the vast CLI turns `_` into spaces before sending.
pub fn vast_gpu_name(gpu: &str) -> String {
    const TABLE: &[(&str, &str)] = &[
        ("NVIDIA RTX A2000", "RTX A2000"),
        ("NVIDIA RTX A4000", "RTX A4000"),
        ("NVIDIA RTX A4500", "RTX A4500"),
        ("NVIDIA RTX A5000", "RTX A5000"),
        ("NVIDIA RTX A6000", "RTX A6000"),
        ("NVIDIA RTX 2000 Ada Generation", "RTX 2000Ada"),
        ("NVIDIA RTX 4000 Ada Generation", "RTX 4000Ada"),
        ("NVIDIA RTX 5000 Ada Generation", "RTX 5000Ada"),
        ("NVIDIA RTX 6000 Ada Generation", "RTX 6000Ada"),
        ("NVIDIA GeForce RTX 3090", "RTX 3090"),
        ("NVIDIA GeForce RTX 4090", "RTX 4090"),
        ("NVIDIA GeForce RTX 4080 SUPER", "RTX 4080S"),
        ("NVIDIA A100 80GB PCIe", "A100 PCIE"),
        ("NVIDIA A100-SXM4-80GB", "A100 SXM4"),
        ("NVIDIA H100 80GB HBM3", "H100 SXM"),
        ("NVIDIA H100 PCIe", "H100 PCIE"),
        ("NVIDIA H100 NVL", "H100 NVL"),
    ];
    let g = gpu.trim();
    if let Some((_, vast)) = TABLE.iter().find(|(id, _)| id.eq_ignore_ascii_case(g)) {
        return vast.to_string();
    }
    let mut words: Vec<String> =
        g.split(|c: char| c.is_whitespace() || c == '_').filter(|w| !w.is_empty()).map(String::from).collect();
    while words.first().is_some_and(|w| ["nvidia", "geforce", "amd"].contains(&w.to_ascii_lowercase().as_str())) {
        words.remove(0);
    }
    // `RTX 4000 Ada Generation` → `RTX 4000Ada` (Vast's spelling).
    let n = words.len();
    if n >= 3 && words[n - 1].eq_ignore_ascii_case("generation") && words[n - 2].eq_ignore_ascii_case("ada") {
        words.truncate(n - 2);
        if let Some(last) = words.last_mut() {
            last.push_str("Ada");
        }
    }
    // A bare model number (`3060`, `4080S`): Vast's consumer cards are `RTX <n>`.
    if let [only] = words.as_slice() {
        if only.chars().count() >= 4 && only.chars().take(4).all(|c| c.is_ascii_digit()) {
            return format!("RTX {only}");
        }
    }
    words.join(" ")
}

/// A GPU name reduced for tolerant comparison: lowercase alphanumerics only, so `RTX 3090`,
/// `rtx_3090` and `RTX-3090` agree — but `RTX 3090 Ti` doesn't (a different card).
fn gpu_key(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// The lowest CUDA version `ALLOWED_CUDA_VERSIONS` lists: a host must support at least it
/// (Vast's `cuda_max_good`). `None` = no floor.
fn cuda_floor(allowed: &[String]) -> Option<f64> {
    allowed.iter().filter_map(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).reduce(f64::min)
}

/// The `PUT /search/asks/` body for `spec`: Vast's filters, wrapped in `{"q": …}` (the flat
/// form is refused now). Server-side filtering narrows the list; [`select_offer`] re-checks
/// every condition, since renting the wrong machine costs money. Pure.
///
/// - `num_gpus` *equal* to the count: `gte` could rent (and bill) an 8-GPU box for a 1-GPU pod.
/// - `allocated_storage` = the disk we'll ask for, so `dph_total` includes its storage cost.
/// - `direct_port_count ≥ 1`: a host without open ports can't give the direct SSH we use.
/// - `cuda_max_good ≥` the CUDA floor; `dph_total ≤` the cap when there is one.
fn search_body(spec: &PodSpec) -> Value {
    let mut q = json!({
        "rentable": {"eq": true},
        "num_gpus": {"eq": spec.gpu_count},
        "gpu_name": {"eq": vast_gpu_name(&spec.gpu_type)},
        "disk_space": {"gte": spec.disk_gb},
        "direct_port_count": {"gte": 1},
        "allocated_storage": spec.disk_gb,
        "type": "on-demand",
        "order": [["dph_total", "asc"]],
        "limit": SEARCH_LIMIT,
    });
    if let Some(floor) = cuda_floor(&spec.allowed_cuda) {
        q["cuda_max_good"] = json!({ "gte": floor });
    }
    if let Some(cap) = spec.max_price {
        q["dph_total"] = json!({ "lte": cap });
    }
    json!({ "q": q })
}

/// The offers in a 2xx search body. Anything but an `offers` array — a `{"success": false,
/// …}`, a renamed key — is schema drift and an error: read as "no offers" it would become
/// capacity, and placement would quietly skip (or wait on) a search that is broken.
fn offers_array(body: &Value) -> Result<&Vec<Value>> {
    body.get("offers").and_then(Value::as_array).ok_or_else(|| {
        let got = super::body_excerpt(body);
        Error::provider(format!("vast search: unexpected response shape (no offers array): {got}"))
    })
}

/// One offer, if we could rent it for `spec` — every condition re-checked client-side: the
/// GPU (tolerant name match, exact card), the exact GPU count, enough disk, rentable and not
/// rented, open ports for direct SSH, the CUDA floor (an offer that doesn't report its CUDA
/// fails a configured floor — unknown isn't enough), and the cap. An offer without an id or
/// a price can't be rented knowingly: skipped. Pure.
fn fitting_offer(v: &Value, spec: &PodSpec) -> Option<Offer> {
    let offer = Offer {
        id: v.get("id").and_then(Value::as_u64)?,
        gpu_name: v.get("gpu_name").and_then(Value::as_str)?.to_string(),
        num_gpus: v.get("num_gpus").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())?,
        disk_space: v.get("disk_space").and_then(Value::as_f64)?,
        dph_total: v.get("dph_total").and_then(Value::as_f64).filter(|p| p.is_finite() && *p > 0.0)?,
        cuda_max_good: v.get("cuda_max_good").and_then(Value::as_f64),
    };
    let flag = |k: &str| v.get(k).and_then(Value::as_bool);
    let fits = gpu_key(&offer.gpu_name) == gpu_key(&vast_gpu_name(&spec.gpu_type))
        && offer.num_gpus == spec.gpu_count
        && offer.disk_space + 0.5 >= f64::from(spec.disk_gb)
        && flag("rentable") != Some(false)
        && flag("rented") != Some(true)
        && v.get("direct_port_count").and_then(Value::as_u64) != Some(0)
        && cuda_floor(&spec.allowed_cuda).is_none_or(|floor| offer.cuda_max_good.is_some_and(|c| c + 1e-9 >= floor))
        && spec.max_price.is_none_or(|cap| milli(offer.dph_total) <= milli(cap));
    fits.then_some(offer)
}

/// The cheapest offer in `offers` that fits `spec` (the first, on a tie — the server's
/// order). Pure, so selection is tested against recorded search responses.
fn select_offer(offers: &[Value], spec: &PodSpec) -> Option<Offer> {
    offers.iter().filter_map(|v| fitting_offer(v, spec)).min_by(|a, b| a.dph_total.total_cmp(&b.dph_total))
}

/// Placement's view of [`VastProvider::quote`]: a price book holding each option's
/// cheapest offer, and a note for every option left unpriced — no offer right now, or the
/// search failed (said, never silently unpriced). Pure.
pub fn price_book(quotes: &[(String, OfferQuote)], gpu_count: u32) -> (PriceBook, Vec<String>) {
    let mut prices = Vec::new();
    let mut notes = vec![
        "vast prices are the cheapest matching offer right now; each create searches again and rents \
         the cheapest offer then — never above --max-price"
            .to_string(),
    ];
    for (gpu, quote) in quotes {
        let what = format!("{gpu_count}×{}", vast_gpu_name(gpu));
        match quote {
            Ok(Some(offer)) => prices.push((gpu.clone(), offer.dph_total)),
            Ok(None) => notes.push(format!("vast: no rentable {what} offer right now — unpriced")),
            Err(e) => notes.push(format!("vast: searching for {what} failed ({e}) — unpriced")),
        }
    }
    (PriceBook::offers(prices, gpu_count), notes)
}

/// The cohort's public keys from the spec: `PUBLIC_KEY` holds them newline-joined (see
/// `PodSpec::from_config`). Trimmed, deduped, in order.
fn spec_pubkeys(spec: &PodSpec) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (_, v) in spec.env.iter().filter(|(k, _)| k == "PUBLIC_KEY") {
        for k in v.lines().map(str::trim).filter(|k| !k.is_empty()) {
            if !keys.iter().any(|have| have == k) {
                keys.push(k.to_string());
            }
        }
    }
    keys
}

/// Whether `key` can be single-quoted into the `onstart` line as is: one line of printable
/// ASCII without a `'`, starting with its type word. Every OpenSSH public key is; anything
/// else is not something to write into `authorized_keys` unseen.
fn quotable_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_alphabetic())
        && key.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        && !key.contains('\'')
}

/// The instance's `onstart`, which Vast's launcher runs at every container start:
/// - `touch ~/.no_auto_tmux` — Vast's launcher otherwise drops every interactive login into
///   tmux; participants connect with VS Code / plain ssh, as on RunPod.
/// - append each cohort key to `~/.ssh/authorized_keys` unless it's there (idempotent) —
///   the same keys the API attaches, written by the container itself, so a failed or slow
///   attach call can't lock setup out.
///
/// A key that isn't [`quotable_key`] is left to the API attach alone, and so are all of them
/// if the script would pass Vast's 4048-character limit. Pure.
fn onstart_script(keys: &[String]) -> String {
    let mut script = r#"touch "${HOME:-/root}/.no_auto_tmux""#.to_string();
    let quoted: Vec<String> =
        keys.iter().map(|k| k.trim()).filter(|k| quotable_key(k)).map(|k| format!("'{k}'")).collect();
    if quoted.is_empty() {
        return script;
    }
    let authorize = format!(
        "; d=\"${{HOME:-/root}}/.ssh\"; mkdir -p \"$d\" && chmod 700 \"$d\" && touch \"$d/authorized_keys\" && \
         chmod 600 \"$d/authorized_keys\" && for k in {}; do grep -qxF -e \"$k\" \"$d/authorized_keys\" || \
         printf '%s\\n' \"$k\" >> \"$d/authorized_keys\"; done",
        quoted.join(" ")
    );
    if script.len() + authorize.len() <= ONSTART_MAX {
        script.push_str(&authorize);
    }
    script
}

/// The `PUT /asks/{id}/` body, every field one docs.vast.ai documents for create (no
/// `client_id`: the search endpoint now refuses fields it doesn't know, so create may too).
/// `env` is a JSON object (Vast's create guide: a docker-flag *string* fails), minus
/// `PUBLIC_KEY`: the keys travel by the attach API and `onstart`, and a multi-line value
/// is a needless risk in a field Vast turns into `docker -e` flags. Pure.
fn create_body(spec: &PodSpec, keys: &[String]) -> Value {
    let env: serde_json::Map<String, Value> = spec
        .env
        .iter()
        .filter(|(k, _)| k != "PUBLIC_KEY")
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    json!({
        "image": spec.image,
        "disk": spec.disk_gb,
        "label": spec.name,
        "runtype": RUNTYPE,
        "env": env,
        "onstart": onstart_script(keys),
        "cancel_unavail": true,
    })
}

/// A short, bounded rendering of a response body for an error message (JSON compacted).
fn excerpt(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => super::body_excerpt(&v),
        Err(_) => {
            let t = text.trim();
            if t.chars().count() > 120 {
                format!("{}…", t.chars().take(120).collect::<String>())
            } else {
                t.to_string()
            }
        }
    }
}

/// Whether a rent refusal says *this offer* is gone — someone else rented it, it was
/// withdrawn (`no_such_ask`: "Instance type by id … is not available"), or the host
/// couldn't schedule it under `cancel_unavail`. Capacity: the next option (or the next
/// round's search) can still succeed. Billing words ("insufficient credit") are not here.
fn offer_gone(text: &str) -> bool {
    let m = text.to_lowercase();
    ["no_such_ask", "no longer available", "already rented", "not rentable", "is rented", "schedul"]
        .iter()
        .any(|needle| m.contains(needle))
        || looks_like_capacity(text)
}

/// What a `PUT /asks/{id}/` answer means. Pure, table-tested.
/// - 2xx `{"success": true, "new_contract": <id>}` → the new instance's id.
/// - 401/429 (and a 403 that isn't about the offer) → Auth/RateLimited by status, whatever
///   the body says — as everywhere ([`crate::error::ProviderErrorKind::classify`]).
/// - The offer is gone ([`offer_gone`], `410 Gone`, a `403` "… is not your own" offer) →
///   Capacity, so placement moves on to the next option.
/// - `{"success": false, …}` otherwise, another 4xx → refused (`Other`: the run stops).
/// - A 2xx without an id, a 2xx body that isn't JSON, a 5xx → an error saying the offer *may
///   have been rented*: not Transient, so nothing retries it — a retry (or the next option)
///   could rent a second machine for the same name. The run stops for the operator to look.
fn rent_outcome(status: StatusCode, text: &str, ask: u64, label: &str) -> Result<u64> {
    let ctx = format!("vast create (offer {ask})");
    let unsure = |why: String| {
        Error::provider(format!(
            "{ctx}: {why} — the offer may have been rented anyway: check `arena pods list --provider vast` \
             for {label} before creating it again"
        ))
    };
    if !status.is_success() {
        let not_ours = status == StatusCode::FORBIDDEN && text.to_lowercase().contains("not your own");
        if matches!(status.as_u16(), 401 | 429) || (status == StatusCode::FORBIDDEN && !not_ours) {
            return Err(status_error(status, text, &ctx));
        }
        if status == StatusCode::GONE || not_ours || offer_gone(text) {
            return Err(Error::capacity(format!("{ctx}: offer taken (HTTP {status}): {}", excerpt(text))));
        }
        if status.is_server_error() {
            return Err(unsure(format!("HTTP {status}: {}", excerpt(text))));
        }
        return Err(status_error(status, text, &ctx));
    }
    let Ok(body) = serde_json::from_str::<Value>(text) else {
        return Err(unsure(format!("HTTP {status} with a body that isn't JSON")));
    };
    if body.get("success").and_then(Value::as_bool) == Some(false) {
        let why = format!("{ctx} refused: {}", excerpt(text));
        return Err(if offer_gone(text) { Error::capacity(why) } else { Error::provider(why) });
    }
    body.get("new_contract")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|id| *id > 0)
        .ok_or_else(|| unsure(format!("HTTP {status} without an instance id: {}", excerpt(text))))
}

/// What a `POST /instances/{id}/ssh/` answer means. A key Vast says is already there is
/// attached — the call is a repair path and must be safe to repeat. Pure.
fn attach_outcome(status: StatusCode, text: &str) -> Result<()> {
    let already = text.to_lowercase().contains("already");
    if !status.is_success() {
        let benign = already && status.is_client_error() && !matches!(status.as_u16(), 401 | 403 | 429);
        return if benign { Ok(()) } else { Err(status_error(status, text, "vast attach ssh key")) };
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.get("success").and_then(Value::as_bool) == Some(false) && !already => {
            Err(Error::provider(format!("vast attach ssh key refused: {}", excerpt(text))))
        }
        // A 2xx is success; a body we can't read doesn't undo it.
        _ => Ok(()),
    }
}

/// A port from a JSON number or numeric string (`HostPort` is a string in Docker's map).
fn port_of(v: &Value) -> Option<u16> {
    let n = match v {
        Value::String(s) => s.trim().parse::<u64>().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }?;
    u16::try_from(n).ok().filter(|p| *p != 0)
}

/// The instance's SSH endpoint, preferring the direct one:
/// 1. **Direct** — the host's `public_ipaddr` and the host port Docker mapped to the
///    container's 22 (`ports["22/tcp"][0].HostPort`; Vast fills `ports` once the container
///    runs). The machine's own address: scp/rsync/port-forwards work, and two pods on it
///    share the IP — which `up --check` and `test --deep` rely on (hosts behind one NAT IP
///    share it too, so that grouping is a strong hint, not proof).
/// 2. **Proxy**, the fallback — `ssh_host:ssh_port` on Vast's shared SSH proxy (`sshN.vast.ai`;
///    +1 on a jupyter runtype, as the vast CLI computes it), for an instance with no direct
///    mapping: a host without open ports, or one rented by hand with proxy SSH. Only once
///    the instance is running, so a booting instance never hands `up` a proxy endpoint that
///    turns into the direct one moments later. A hostname, so it never groups as a machine.
///
/// The same choice the vast CLI's `ssh-url` makes. Pure.
fn ssh_endpoint(v: &Value) -> Option<(String, u16)> {
    let direct = || {
        let ip = v.get("public_ipaddr").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())?;
        let mapped = v.get("ports")?.get("22/tcp")?.as_array()?;
        let port = mapped.iter().find_map(|m| m.get("HostPort").and_then(port_of))?;
        Some((ip.to_string(), port))
    };
    let proxy = || {
        let running = v.get("actual_status").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("running"));
        if !running {
            return None;
        }
        let host = v.get("ssh_host").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())?;
        let port = v.get("ssh_port").and_then(port_of)?;
        let jupyter = v.get("image_runtype").and_then(Value::as_str).is_some_and(|r| r.contains("jupyter"));
        let port = if jupyter { port.checked_add(1)? } else { port };
        Some((host.to_string(), port))
    };
    direct().or_else(proxy)
}

fn parse_instance(v: &Value) -> Pod {
    // Vast reports liveness in `actual_status` ("running"/"exited"/...) and the
    // requested state in `cur_state`/`intended_status`; prefer the observed one.
    let status = v
        .get("actual_status")
        .or_else(|| v.get("cur_state"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_uppercase();
    let (ssh_ip, ssh_port) = ssh_endpoint(v).map_or((None, None), |(h, p)| (Some(h), Some(p)));

    Pod {
        id: v
            .get("id")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default(),
        // The machine name we set at rent time lives in `label`; fall back to the
        // numeric id so a hand-rented instance still shows something.
        name: v
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| v.get("id").and_then(Value::as_u64).map(|n| format!("vast-{n}")))
            .unwrap_or_default(),
        provider: "vast".into(),
        status,
        gpu_type: v.get("gpu_name").and_then(Value::as_str).map(String::from),
        gpu_count: v.get("num_gpus").and_then(Value::as_u64).map(|n| n as u32),
        cost_per_hr: v.get("dph_total").and_then(Value::as_f64),
        ssh_ip,
        ssh_port,
        maintenance: None,
    }
}

/// The instance objects in a 2xx `GET /instances/` body: an `instances` array (or a bare
/// array). Any other shape is schema drift and must be an error, not an empty list — the
/// proxy merge reads "listed OK without instance X" as "X was terminated", so an
/// empty-by-accident list would drop every Vast forward. Fail closed.
fn instances_array(body: &Value) -> Result<&Vec<Value>> {
    body.get("instances")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())
        .ok_or_else(|| {
            Error::provider(format!(
                "vast list: unexpected response shape (no instances array): {}",
                super::body_excerpt(body)
            ))
        })
}

#[async_trait]
impl Provider for VastProvider {
    fn name(&self) -> &'static str {
        "vast"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        let cuda = cuda_floor(&spec.allowed_cuda).map(|v| format!(", CUDA ≥ {v:.1}")).unwrap_or_default();
        let cap = spec.max_price.map(|c| format!(", ≤ {}/h", fmt_money("$", c))).unwrap_or_default();
        // `--bootstrap` brings up sshd on an image without one; Vast's launcher does that.
        let bootstrap = if spec.docker_args.is_some() { " (--bootstrap not needed: ignored)" } else { "" };
        format!(
            "cheapest rentable {}×{} offer (disk ≥ {}GB{cuda}{cap}), image {}; SSH by Vast's launcher (direct port; \
             it replaces the image's entrypoint{bootstrap}), cohort keys attached to the instance",
            spec.gpu_count,
            vast_gpu_name(&spec.gpu_type),
            spec.disk_gb,
            spec.image
        )
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        let body = send_json(self.auth(self.client.get(format!("{}/instances/", self.base))), "vast list").await?;
        Ok(instances_array(&body)?.iter().map(parse_instance).collect())
    }

    /// Search, rent the cheapest fitting offer, then attach the cohort keys to it. Once the
    /// rent succeeded this never returns `Err`: the instance exists and bills, and an error
    /// would send placement to the next option — a second instance for the same name. So the
    /// key attach is best effort here; `onstart` writes the same keys, and setup re-attaches
    /// them ([`Provider::authorize_ssh_keys`]) if the pod still refuses ours.
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let offer = self.cheapest_offer(spec).await?.ok_or_else(|| {
            // No fitting offer == the marketplace has no capacity for this spec (right now).
            let cap = spec.max_price.map(|c| format!(", ≤ {}/h", fmt_money("$", c))).unwrap_or_default();
            Error::capacity(format!(
                "vast: no rentable offer matching {}×{} (disk ≥ {}GB{cap})",
                spec.gpu_count,
                vast_gpu_name(&spec.gpu_type),
                spec.disk_gb
            ))
        })?;
        let keys = spec_pubkeys(spec);
        let resp = self
            .auth(self.client.put(format!("{}/asks/{}/", self.base, offer.id)).json(&create_body(spec, &keys)))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let id = rent_outcome(status, &text, offer.id, &spec.name)?.to_string();
        let _ = self.attach_keys(&id, &keys).await;
        // Vast doesn't echo the instance, so the Pod is synthesized from the offer.
        Ok(Pod {
            id,
            name: spec.name.clone(),
            provider: "vast".into(),
            status: "CREATING".into(),
            gpu_type: Some(offer.gpu_name),
            gpu_count: Some(offer.num_gpus),
            cost_per_hr: Some(offer.dph_total),
            ssh_ip: None,
            ssh_port: None,
            maintenance: None,
        })
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        // Vast stops an instance by setting its desired state.
        send_ok(
            self.auth(self.client.put(format!("{}/instances/{}/", self.base, id)).json(&json!({"state": "stopped"}))),
            "vast stop",
        )
        .await
    }

    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        // Unverified either way, so assume the worst (see restart_pod).
        true
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // Vast has no single reboot endpoint; a restart is stop then start. Vast keeps the
        // stopped instance (same id), but whether its container disk survives the cycle
        // hasn't been verified here — so callers treat it as wiping, like RunPod's.
        for state in ["stopped", "running"] {
            send_ok(
                self.auth(self.client.put(format!("{}/instances/{}/", self.base, id)).json(&json!({ "state": state }))),
                "vast restart",
            )
            .await?;
        }
        Ok(())
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/instances/{}/", self.base, id))), "vast terminate").await
    }

    /// The per-instance attach, as at create (idempotent): the repair path for a pod that
    /// refuses the cohort key.
    async fn authorize_ssh_keys(&self, pod: &Pod, keys: &[String]) -> Result<()> {
        self.attach_keys(&pod.id, keys).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind as K;
    use crate::http::test_server::{canned, client, serve};

    /// A real `PUT /search/asks/` response (q-wrapped body, `gpu_name` "RTX 3090"), recorded
    /// 2026-10-08: three RTX 3090 offers, cuda_max_good 13.0/13.0/13.1, cheapest $0.1058/h.
    const SEARCH_3090: &str = include_str!("fixtures/search_asks_q_rtx3090_2026-10-08.json");

    /// A `GET /instances/` body shaped after docs.vast.ai's show-instances schema (and the
    /// vast CLI's ssh-url logic) — NOT a recorded response: no instance existed to record.
    const INSTANCES: &str = include_str!("fixtures/instances_docs_shape.json");

    const R3090: &str = "NVIDIA GeForce RTX 3090";
    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbc devtest";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIXyz deploy@arena";

    fn spec(gpu: &str, count: u32, disk: u32) -> PodSpec {
        PodSpec {
            name: "arena8-apple".into(),
            image: "img:1".into(),
            gpu_type: gpu.into(),
            gpu_count: count,
            cloud_type: "COMMUNITY".into(),
            disk_gb: disk,
            volume_gb: 0,
            ports: "8888/http,22/tcp".into(),
            env: vec![],
            docker_args: None,
            allowed_cuda: Vec::new(),
            max_price: None,
        }
    }

    fn fixture_offers() -> Vec<Value> {
        let body: Value = serde_json::from_str(SEARCH_3090).unwrap();
        offers_array(&body).unwrap().clone()
    }

    fn provider(base: &str) -> VastProvider {
        VastProvider { api_key: "BOGUS".into(), client: client(), base: base.to_string() }
    }

    /// Status first, then decode: an error response whose body isn't JSON (a bad key's
    /// 401, a proxy's HTML) is classified by its status — never "error decoding response
    /// body". Against a loopback server, through the real request path.
    #[tokio::test]
    async fn error_statuses_are_classified_before_decoding() {
        let srv = serve(vec![
            canned(401, "text/html", "<html>401 Unauthorized</html>"),
            canned(429, "text/plain", "Too Many Requests"),
            canned(403, "text/plain", "Forbidden"),
        ]);
        let p = provider(&srv.base);
        let e = p.list_pods().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("vast list HTTP 401"), "{e}");
        let e = p.cheapest_offer(&spec("NVIDIA RTX A4000", 1, 50)).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::RateLimited), "the offer search: {e}");
        let e = p.terminate_pod("7").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
    }

    #[test]
    fn instances_array_accepts_both_list_shapes() {
        assert_eq!(instances_array(&json!({"instances": [{"id": 1}]})).unwrap().len(), 1);
        assert!(instances_array(&json!({"instances": []})).unwrap().is_empty());
        assert_eq!(instances_array(&json!([{"id": 1}, {"id": 2}])).unwrap().len(), 2);
    }

    #[test]
    fn instances_array_fails_closed_on_schema_drift() {
        // e.g. a 200 carrying `{"success": false, "error": …}` must not read as "no instances".
        for body in [json!({}), json!({"instances": null}), json!({"success": false, "error": "x"}), json!(null)] {
            let err = instances_array(&body).unwrap_err().to_string();
            assert!(err.contains("unexpected response shape"), "{body}: {err}");
        }
    }

    /// RunPod ids (what `--gpu`/`GPU_TYPE` resolve to) → the names Vast's search matches.
    #[test]
    fn vast_gpu_names_have_spaces_and_vasts_spelling() {
        for (gpu, vast) in [
            (R3090, "RTX 3090"), // the live-verified one: "RTX_3090" finds no offers
            ("NVIDIA RTX A4000", "RTX A4000"),
            ("NVIDIA RTX 4000 Ada Generation", "RTX 4000Ada"),
            ("NVIDIA RTX 6000 Ada Generation", "RTX 6000Ada"),
            ("NVIDIA A100-SXM4-80GB", "A100 SXM4"),
            ("NVIDIA A100 80GB PCIe", "A100 PCIE"),
            ("NVIDIA H100 80GB HBM3", "H100 SXM"),
            ("NVIDIA GeForce RTX 4080 SUPER", "RTX 4080S"),
            // Fallback: vendor words dropped, spaces kept, Ada joined.
            ("NVIDIA GeForce RTX 3070", "RTX 3070"),
            ("NVIDIA GeForce RTX 3090 Ti", "RTX 3090 Ti"),
            ("NVIDIA RTX 5880 Ada Generation", "RTX 5880Ada"),
            ("NVIDIA L40S", "L40S"),
            ("NVIDIA A40", "A40"),
            ("nvidia rtx a4000", "RTX A4000"), // case-insensitive table hit
            ("RTX_3060", "RTX 3060"),          // underscores never survive
            ("3060", "RTX 3060"),              // a bare model number
            ("4080S", "RTX 4080S"),
            // Already Vast's form: unchanged.
            ("RTX 3090", "RTX 3090"),
            ("RTX 4000Ada", "RTX 4000Ada"),
            ("Tesla V100", "Tesla V100"),
        ] {
            assert_eq!(vast_gpu_name(gpu), vast, "{gpu}");
            assert!(!vast_gpu_name(gpu).contains('_'), "{gpu}");
        }
        // Tolerant comparison: formatting differences agree, different cards don't.
        assert_eq!(gpu_key("RTX 3090"), gpu_key("rtx_3090"));
        assert_ne!(gpu_key("RTX 3090"), gpu_key("RTX 3090 Ti"));
    }

    /// The search payload: q-wrapped (the flat body is a 400 now), Vast's spaced GPU name,
    /// an exact GPU count, the CUDA floor, and the cap only when there is one.
    #[test]
    fn search_body_is_q_wrapped_with_the_vast_name() {
        let mut s = spec(R3090, 1, 100);
        s.allowed_cuda = vec!["13.0".into(), "12.8".into()];
        let body = search_body(&s);
        assert_eq!(body.as_object().unwrap().keys().collect::<Vec<_>>(), ["q"], "only `q` at the top: {body}");
        let q = &body["q"];
        assert_eq!(q["gpu_name"], json!({"eq": "RTX 3090"}));
        assert_eq!(q["num_gpus"], json!({"eq": 1}));
        assert_eq!(q["rentable"], json!({"eq": true}));
        assert_eq!(q["disk_space"], json!({"gte": 100}));
        assert_eq!(q["allocated_storage"], json!(100));
        assert_eq!(q["direct_port_count"], json!({"gte": 1}));
        assert_eq!(q["cuda_max_good"], json!({"gte": 12.8}), "the lowest allowed version");
        assert_eq!(q["type"], "on-demand");
        assert_eq!(q["order"], json!([["dph_total", "asc"]]));
        assert_eq!(q["limit"], json!(SEARCH_LIMIT));
        assert!(q.get("dph_total").is_none(), "no cap, no price filter");
        s.max_price = Some(0.15);
        s.allowed_cuda.clear();
        let q = &search_body(&s)["q"];
        assert_eq!(q["dph_total"], json!({"lte": 0.15}));
        assert!(q.get("cuda_max_good").is_none());
    }

    /// Contract test on the recorded response: it parses, the cheapest fitting offer wins,
    /// and every client-side condition (disk, count, CUDA, cap, card) is applied to it.
    #[test]
    fn recorded_search_selects_the_cheapest_fitting_offer() {
        let offers = fixture_offers();
        assert_eq!(offers.len(), 3);
        let mut s = spec(R3090, 1, 100);
        s.allowed_cuda = vec!["13.0".into()];
        let best = select_offer(&offers, &s).unwrap();
        assert_eq!((best.id, best.gpu_name.as_str(), best.num_gpus), (54036773, "RTX 3090", 1));
        assert!((best.dph_total - 0.105_777_777).abs() < 1e-6, "{}", best.dph_total);
        assert_eq!(best.cuda_max_good, Some(13.0));
        // More disk than the cheapest host has: the 541 GB one ($0.1104).
        assert_eq!(select_offer(&offers, &spec(R3090, 1, 300)).map(|o| o.id), Some(45866034));
        // A CUDA 13.1 floor: only the 13.1 host.
        s.allowed_cuda = vec!["13.1".into()];
        assert_eq!(select_offer(&offers, &s).map(|o| o.id), Some(19025024));
        s.allowed_cuda = vec!["13.0".into()];
        // The cap, compared like the plan's (tenths of a cent): $0.106 admits $0.10578…
        s.max_price = Some(0.106);
        assert_eq!(select_offer(&offers, &s).map(|o| o.id), Some(54036773));
        // …$0.105 admits nothing.
        s.max_price = Some(0.105);
        assert_eq!(select_offer(&offers, &s), None);
        // A different count or card finds nothing in a 1×3090 response.
        assert_eq!(select_offer(&offers, &spec(R3090, 2, 50)), None);
        assert_eq!(select_offer(&offers, &spec("NVIDIA GeForce RTX 3090 Ti", 1, 50)), None);
        assert_eq!(select_offer(&offers, &spec("RTX_3090", 1, 50)).map(|o| o.id), Some(54036773));
    }

    #[test]
    fn offers_failing_a_check_are_skipped_never_rented() {
        let ok = json!({"id": 1, "gpu_name": "RTX A4000", "num_gpus": 1, "disk_space": 200.0, "dph_total": 0.30,
                        "cuda_max_good": 13.0, "rentable": true, "rented": false, "direct_port_count": 10});
        let with = |k: &str, v: Value| {
            let mut o = ok.clone();
            o["dph_total"] = json!(0.10); // each bad one is cheaper than `ok`
            o[k] = v;
            o
        };
        let mut s = spec("NVIDIA RTX A4000", 1, 100);
        s.allowed_cuda = vec!["13.0".into()];
        for (why, bad) in [
            ("rented", with("rented", json!(true))),
            ("not rentable", with("rentable", json!(false))),
            ("no open ports", with("direct_port_count", json!(0))),
            ("CUDA too old", with("cuda_max_good", json!(12.4))),
            ("CUDA unknown", with("cuda_max_good", Value::Null)),
            ("too little disk", with("disk_space", json!(50.0))),
            ("more GPUs than asked", with("num_gpus", json!(2))),
            ("another card", with("gpu_name", json!("RTX A4000 Ada"))),
            ("no price", with("dph_total", Value::Null)),
            ("no id", with("id", Value::Null)),
        ] {
            assert_eq!(select_offer(&[bad, ok.clone()], &s).map(|o| o.id), Some(1), "{why}");
        }
        // With no CUDA floor configured, an offer that doesn't report one is fine.
        let unknown = with("cuda_max_good", Value::Null);
        assert_eq!(select_offer(&[unknown], &spec("NVIDIA RTX A4000", 1, 100)).map(|o| o.dph_total), Some(0.10));
    }

    #[test]
    fn offers_array_fails_closed_on_an_unexpected_2xx() {
        assert!(offers_array(&json!({"offers": []})).unwrap().is_empty());
        for body in [json!({"success": false, "error": "invalid_args", "msg": "bad q"}), json!({"offers": null}), json!([])] {
            let e = offers_array(&body).unwrap_err();
            assert_eq!(e.kind(), Some(K::Other), "{body}: never capacity");
            assert!(e.to_string().contains("no offers array"), "{e}");
        }
    }

    /// The create payload: only documented fields, the ssh launcher with a direct port, env
    /// as an object without PUBLIC_KEY, the keys in onstart, and no stopped-instance fallback.
    #[test]
    fn create_body_payload() {
        let mut s = spec(R3090, 1, 80);
        s.env = vec![("PUBLIC_KEY".into(), format!("{KEY_A}\n{KEY_B}\n")), ("MACHINE_NAME".into(), "arena8-apple".into())];
        let keys = spec_pubkeys(&s);
        assert_eq!(keys, [KEY_A, KEY_B]);
        let body = create_body(&s, &keys);
        let mut fields: Vec<&String> = body.as_object().unwrap().keys().collect();
        fields.sort();
        assert_eq!(fields, ["cancel_unavail", "disk", "env", "image", "label", "onstart", "runtype"]);
        assert_eq!(body["runtype"], "ssh_direct");
        assert_eq!(body["cancel_unavail"], true);
        assert_eq!((body["image"].as_str(), body["disk"].as_u64(), body["label"].as_str()), (Some("img:1"), Some(80), Some("arena8-apple")));
        assert_eq!(body["env"], json!({"MACHINE_NAME": "arena8-apple"}), "no PUBLIC_KEY, an object not a flag string");
        let onstart = body["onstart"].as_str().unwrap();
        assert!(onstart.starts_with(r#"touch "${HOME:-/root}/.no_auto_tmux""#), "{onstart}");
        assert!(onstart.contains(&format!("'{KEY_A}' '{KEY_B}'")), "{onstart}");
        assert!(onstart.contains(r#"grep -qxF -e "$k""#), "idempotent: {onstart}");
    }

    #[test]
    fn onstart_quotes_only_real_keys_and_stays_under_the_limit() {
        // Nothing usable: just the tmux opt-out.
        let none = onstart_script(&["".into(), "evil' ; rm -rf / #".into(), "ssh-ed25519 AAAA\nx".into()]);
        assert_eq!(none, r#"touch "${HOME:-/root}/.no_auto_tmux""#);
        let mixed = onstart_script(&[KEY_A.into(), "$(reboot)".into()]);
        assert!(mixed.contains(&format!("'{KEY_A}'")) && !mixed.contains("reboot"), "{mixed}");
        // Too long for Vast's 4048 chars: the keys are left to the API attach.
        let huge = format!("ssh-rsa {}", "A".repeat(4100));
        let s = onstart_script(&[huge]);
        assert!(s.len() <= ONSTART_MAX && !s.contains("authorized_keys"), "{}", s.len());
    }

    /// What each rent answer means for placement: an id, capacity (next option), or a stop —
    /// and an answer that might hide a rented machine is never retried.
    #[test]
    fn rent_outcome_classifies_every_answer() {
        let gone_404 = r#"{"success": false, "error": "invalid_args", "msg": "error 404/3603: no_such_ask Instance type by id 54036773 is not available.", "ask_id": 54036773}"#;
        // (case, status, body, Ok id / Err kind)
        let cases: Vec<(&str, u16, &str, std::result::Result<u64, Option<K>>)> = vec![
            ("rented", 200, r#"{"success": true, "new_contract": 7835610}"#, Ok(7835610)),
            ("id as string", 200, r#"{"success": true, "new_contract": "7835610"}"#, Ok(7835610)),
            ("offer gone (docs' 404)", 404, gone_404, Err(Some(K::Capacity))),
            ("offer gone in a 2xx", 200, gone_404, Err(Some(K::Capacity))),
            ("410 Gone", 410, r#"{"msg": "Offer no longer available"}"#, Err(Some(K::Capacity))),
            ("already rented", 400, r#"{"success": false, "msg": "ask already rented"}"#, Err(Some(K::Capacity))),
            ("not schedulable", 400, r#"{"success": false, "msg": "scheduling failed"}"#, Err(Some(K::Capacity))),
            ("someone's private offer", 403, r#"{"msg": "Offer 1234567 is not your own"}"#, Err(Some(K::Capacity))),
            ("bad key", 401, "Unauthorized", Err(Some(K::Auth))),
            ("forbidden", 403, "Forbidden", Err(Some(K::Auth))),
            ("throttled", 429, "slow down", Err(Some(K::RateLimited))),
            ("throttled, whatever the body", 429, r#"{"msg": "no_such_ask"}"#, Err(Some(K::RateLimited))),
            ("bad request", 400, r#"{"success": false, "error": "invalid_args", "msg": "bad image"}"#, Err(Some(K::Other))),
            ("refused in a 2xx", 200, r#"{"success": false, "msg": "insufficient credit"}"#, Err(Some(K::Other))),
            ("no id", 200, r#"{"success": true}"#, Err(Some(K::Other))),
            ("not json", 200, "<html>ok</html>", Err(Some(K::Other))),
            ("5xx", 502, "<html>Bad Gateway</html>", Err(Some(K::Other))),
        ];
        for (case, code, body, want) in cases {
            let got = rent_outcome(StatusCode::from_u16(code).unwrap(), body, 54036773, "arena8-apple");
            match (&got, want) {
                (Ok(id), Ok(w)) => assert_eq!(*id, w, "{case}"),
                (Err(e), Err(kind)) => assert_eq!(e.kind(), kind, "{case}: {e}"),
                _ => panic!("{case}: {got:?}"),
            }
        }
        // Where a machine may have been rented, the message says where to look.
        for (code, body) in [(200, r#"{"success": true}"#), (502, "Bad Gateway"), (200, "not json")] {
            let e = rent_outcome(StatusCode::from_u16(code).unwrap(), body, 1, "arena8-apple").unwrap_err().to_string();
            assert!(e.contains("may have been rented") && e.contains("arena8-apple"), "{e}");
        }
    }

    #[test]
    fn attach_outcome_is_idempotent() {
        let ok = |code: u16, body: &str| attach_outcome(StatusCode::from_u16(code).unwrap(), body);
        assert!(ok(200, r#"{"success": true, "msg": "SSH key attached successfully"}"#).is_ok());
        assert!(ok(200, "").is_ok(), "a 2xx is success");
        assert!(ok(400, r#"{"success": false, "msg": "key already attached"}"#).is_ok(), "already there = done");
        assert!(ok(200, r#"{"success": false, "msg": "Key already exists"}"#).is_ok());
        assert_eq!(ok(200, r#"{"success": false, "msg": "invalid key"}"#).unwrap_err().kind(), Some(K::Other));
        assert_eq!(ok(400, r#"{"msg": "invalid key"}"#).unwrap_err().kind(), Some(K::Other));
        assert_eq!(ok(401, "already? no: unauthorized").unwrap_err().kind(), Some(K::Auth));
        assert_eq!(ok(503, "").unwrap_err().kind(), Some(K::Transient), "retried by attach_keys");
    }

    /// The instance list: the direct endpoint (host IP + mapped 22) when there is one, the
    /// proxy only for a running instance without it, nothing for a booting one.
    #[test]
    fn instance_list_prefers_the_direct_endpoint() {
        let body: Value = serde_json::from_str(INSTANCES).unwrap();
        let pods: Vec<Pod> = instances_array(&body).unwrap().iter().map(parse_instance).collect();
        let ep = |i: usize| (pods[i].ssh_ip.as_deref(), pods[i].ssh_port);
        // Direct: public IP (trimmed) + the HostPort mapped to 22, not ssh5.vast.ai:21000.
        assert_eq!((pods[0].name.as_str(), pods[0].status.as_str()), ("devtest-apple", "RUNNING"));
        assert_eq!(ep(0), (Some("124.123.111.26"), Some(40112)));
        assert_eq!((pods[0].gpu_type.as_deref(), pods[0].gpu_count), (Some("RTX 3090"), Some(1)));
        // Running, no direct mapping (proxy runtype): the proxy.
        assert_eq!(ep(1), (Some("ssh2.vast.ai"), Some(31002)));
        // Still loading: no endpoint at all, though a proxy host is already listed.
        assert_eq!((pods[2].status.as_str(), ep(2)), ("LOADING", (None, None)));
        // A hand-rented jupyter instance: its SSH is the proxy port + 1; no label → id name.
        assert_eq!((pods[3].name.as_str(), ep(3)), ("vast-26001004", (Some("ssh4.vast.ai"), Some(31005))));
        // HostPort as a number works too; a stopped instance keeps its last mapping.
        assert_eq!((pods[4].status.as_str(), ep(4)), ("EXITED", (Some("124.123.111.26"), Some(40200))));
        // The direct IP is the machine's: health groups the two pods on that host.
        assert_eq!(crate::health::host_ip(&pods[0]).as_deref(), Some("124.123.111.26"));
        assert_eq!(crate::health::host_ip(&pods[1]), None, "a proxy hostname never names a machine");
    }

    #[test]
    fn parses_instance_without_label_falls_back_to_id() {
        let v = json!({"id": 999, "actual_status": "exited"});
        let p = parse_instance(&v);
        assert_eq!(p.name, "vast-999");
        assert_eq!(p.status, "EXITED");
        assert_eq!((p.ssh_ip, p.ssh_port), (None, None));
    }

    #[test]
    fn price_book_quotes_the_cheapest_offer_and_notes_the_rest() {
        let best = select_offer(&fixture_offers(), &spec(R3090, 1, 50)).unwrap();
        let quotes = vec![
            (R3090.to_string(), Ok(Some(best))),
            ("NVIDIA RTX A4000".to_string(), Ok(None)),
            ("NVIDIA RTX 4000 Ada Generation".to_string(), Err("vast search HTTP 500".to_string())),
        ];
        let (book, notes) = price_book(&quotes, 1);
        let q = book.quote(R3090, None);
        assert!((q.price_per_gpu.unwrap() - 0.105_777_777).abs() < 1e-6, "{q:?}");
        assert_eq!(q.source, Some(crate::placement::PriceSource::Live));
        assert_eq!(book.quote("NVIDIA RTX A4000", None).price_per_gpu, None);
        assert!(notes[0].contains("cheapest matching offer right now"), "{notes:?}");
        assert_eq!(notes[1], "vast: no rentable 1×RTX A4000 offer right now — unpriced");
        assert_eq!(notes[2], "vast: searching for 1×RTX 4000Ada failed (vast search HTTP 500) — unpriced");
    }

    /// One search per Vast GPU name and count: two options that are the same card on Vast
    /// share it; a failed search is that option's error, not the plan's.
    #[tokio::test]
    async fn quote_searches_once_per_vast_name() {
        let srv = serve(vec![
            canned(200, "application/json", SEARCH_3090),
            canned(500, "text/plain", "boom"),
        ]);
        let p = provider(&srv.base);
        let gpus = vec![R3090.to_string(), "RTX 3090".to_string(), "NVIDIA RTX A4000".to_string()];
        let got = p.quote(&gpus, &spec("", 1, 50), Duration::from_secs(30)).await;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1.as_ref().unwrap().as_ref().map(|o| o.id), Some(54036773));
        assert_eq!(got[1].1, got[0].1, "the same card: served from the first search");
        assert!(got[2].1.as_ref().unwrap_err().contains("vast search HTTP 500"), "{:?}", got[2].1);
        assert_eq!(srv.requests.lock().unwrap().iter().filter(|r| r.starts_with("PUT /search/asks/")).count(), 2);
    }

    /// End to end over a loopback server: search → rent the cheapest fitting offer → attach
    /// each cohort key to the new instance. A refused attach doesn't fail the create (the
    /// instance exists and bills).
    #[tokio::test]
    async fn create_rents_the_cheapest_offer_then_attaches_each_key() {
        let srv = serve(vec![
            canned(200, "application/json", SEARCH_3090),
            canned(200, "application/json", r#"{"success": true, "new_contract": 26001001}"#),
            canned(200, "application/json", r#"{"success": true, "msg": "SSH key attached successfully"}"#),
            canned(400, "application/json", r#"{"success": false, "msg": "invalid ssh key"}"#),
        ]);
        let p = provider(&srv.base);
        let mut s = spec(R3090, 1, 50);
        s.env = vec![("PUBLIC_KEY".into(), format!("{KEY_A}\n{KEY_B}"))];
        let pod = p.create_pod(&s).await.unwrap();
        assert_eq!((pod.id.as_str(), pod.name.as_str(), pod.provider.as_str()), ("26001001", "arena8-apple", "vast"));
        assert_eq!((pod.gpu_type.as_deref(), pod.gpu_count), (Some("RTX 3090"), Some(1)));
        assert!((pod.cost_per_hr.unwrap() - 0.105_777_777).abs() < 1e-6);
        assert_eq!(
            *srv.requests.lock().unwrap(),
            [
                "PUT /search/asks/ HTTP/1.1",
                "PUT /asks/54036773/ HTTP/1.1",
                "POST /instances/26001001/ssh/ HTTP/1.1",
                "POST /instances/26001001/ssh/ HTTP/1.1"
            ]
        );
    }

    #[tokio::test]
    async fn create_reports_a_taken_offer_or_an_empty_market_as_capacity() {
        let gone = r#"{"success": false, "error": "invalid_args", "msg": "error 404/3603: no_such_ask Instance type by id 54036773 is not available."}"#;
        let srv = serve(vec![
            canned(200, "application/json", SEARCH_3090),
            canned(404, "application/json", gone),
            canned(200, "application/json", r#"{"offers": []}"#),
        ]);
        let p = provider(&srv.base);
        let e = p.create_pod(&spec(R3090, 1, 50)).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Capacity), "{e}");
        let e = p.create_pod(&spec(R3090, 1, 50)).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Capacity), "{e}");
        assert!(e.to_string().contains("no rentable offer matching 1×RTX 3090"), "{e}");
        // Nothing was attached: no instance came of it.
        assert!(srv.requests.lock().unwrap().iter().all(|r| !r.contains("/ssh/")));
    }

    /// The repair path: the same idempotent attach, for an existing pod.
    #[tokio::test]
    async fn authorize_ssh_keys_attaches_each_key_to_the_pod() {
        let srv = serve(vec![
            canned(200, "application/json", r#"{"success": true}"#),
            canned(400, "application/json", r#"{"success": false, "msg": "key already attached"}"#),
        ]);
        let p = provider(&srv.base);
        let pod = Pod { id: "26001001".into(), provider: "vast".into(), ..Default::default() };
        p.authorize_ssh_keys(&pod, &[KEY_A.into(), KEY_B.into()]).await.unwrap();
        assert_eq!(*srv.requests.lock().unwrap(), ["POST /instances/26001001/ssh/ HTTP/1.1", "POST /instances/26001001/ssh/ HTTP/1.1"]);
        let e = p.authorize_ssh_keys(&Pod::default(), &[KEY_A.into()]).await.unwrap_err();
        assert!(e.to_string().contains("no instance id"), "{e}");
    }

    #[test]
    fn describe_says_what_create_will_do() {
        let mut s = spec(R3090, 2, 80);
        s.allowed_cuda = vec!["13.0".into()];
        s.max_price = Some(0.3);
        let d = VastProvider::new("k").describe(&s);
        assert!(d.starts_with("cheapest rentable 2×RTX 3090 offer (disk ≥ 80GB, CUDA ≥ 13.0, ≤ $0.30/h), image img:1"), "{d}");
        assert!(d.contains("replaces the image's entrypoint"), "{d}");
    }
}
