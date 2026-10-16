//! Hetzner Cloud backend (https://api.hetzner.cloud/v1) — CPU-only VMs.
//!
//! Unlike RunPod/Vast this isn't a GPU container host: it provisions an ordinary
//! cloud VM, so the GPU-centric fields of a [`PodSpec`] don't apply. Hetzner instead
//! takes its sizing (`server_type`), OS (`image`), `location`, and `ssh_keys` from
//! its own `HETZNER_*` config, read once at construction; from a `PodSpec` it uses
//! only the `name` and `server_type` (`--server-type`, over `HETZNER_SERVER_TYPE`). VMs come
//! up on a real public IP with SSH on `:22`, so they slot straight into the same
//! proxy/`pods up` flow as the GPU providers.
//!
//! A refused create is explained from the project itself ([`HetznerProvider::explain_create_error`]):
//! a missing SSH key names the project's keys and what to set, a server type the location
//! doesn't offer lists the types it does — two read-only GETs, only after a create failed.
//!
//! `--api-json` merges into the create body ([`create_payload`]) — `labels`, `user_data`
//! (cloud-init), `firewalls`, `networks`… — minus the fields this module sets ([`MANAGED`]).
//!
//! Responses are parsed defensively from `serde_json::Value`, as with the others.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::{Provider, StopOutcome};
use crate::apiextra::{self, managed, Extra, Managed};
use crate::error::{Error, Result};
use crate::http::{send_json, send_ok};
use crate::pod::{Pod, PodSpec};

pub(crate) const BASE: &str = "https://api.hetzner.cloud/v1";

/// Hetzner-specific creation options, sourced from `HETZNER_*` config keys.
#[derive(Debug, Clone)]
pub struct HetznerOpts {
    pub server_type: String,
    pub image: String,
    pub location: Option<String>,
    /// Names (or ids) of SSH keys already uploaded to the Hetzner project.
    pub ssh_keys: Vec<String>,
    /// The config key that named `ssh_keys`: `HETZNER_SSH_KEY`, or `MACHINE_NAME_PREFIX` when
    /// that's unset (the default key is named after the prefix) — what the missing-key error
    /// tells the operator to change.
    pub ssh_keys_from: &'static str,
    /// The cohort key's public half on this box (`SHARED_SSH_KEY_PATH` + `.pub`): the file to
    /// upload under a missing key's name. `None` = no shared key configured.
    pub pubkey_file: Option<String>,
}

impl Default for HetznerOpts {
    fn default() -> Self {
        Self {
            // cx23: newest Intel **x86** shared vCPU (2 vCPU / 4 GB). The old cx/cax lines
            // had poor availability; cx23 is the current gen. Override via
            // HETZNER_SERVER_TYPE. Default location nbg1 (EU) — x86 shared types are
            // EU-only, so a pinned location avoids the US auto-placement failure.
            server_type: "cx23".into(),
            image: "ubuntu-24.04".into(),
            location: Some("nbg1".into()),
            ssh_keys: Vec::new(),
            ssh_keys_from: "MACHINE_NAME_PREFIX",
            pubkey_file: None,
        }
    }
}

impl HetznerOpts {
    /// The options from config (`HETZNER_*`), each key's default where it's unset — what
    /// [`super::build`] creates with and `config check` shows. Pure.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let set = |k: &str| cfg.get(k).filter(|s| !s.is_empty());
        let d = Self::default();
        // The universal default key: the cohort SSH key, named after MACHINE_NAME_PREFIX (e.g.
        // "arena8"), so every hetzner pod is reachable with the same arena key as the GPU fleet —
        // no per-pod config. Upload it once to the Hetzner project under that name (console);
        // HETZNER_SSH_KEY names another.
        let (ssh_keys, ssh_keys_from) = match set("HETZNER_SSH_KEY") {
            Some(k) => (vec![k.to_string()], "HETZNER_SSH_KEY"),
            None => (vec![cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string()], "MACHINE_NAME_PREFIX"),
        };
        Self {
            // cx23 in nbg1 (see `Default`): the location is always pinned — the x86 shared types
            // live only in the EU DCs, so no-location auto-placement can land on a US DC that
            // lacks them and fail with "error during placement".
            server_type: cfg.get("HETZNER_SERVER_TYPE").map_or(d.server_type, String::from),
            image: cfg.get("HETZNER_IMAGE").map_or(d.image, String::from),
            location: Some(cfg.get("HETZNER_LOCATION").unwrap_or("nbg1").to_string()),
            ssh_keys,
            ssh_keys_from,
            pubkey_file: set("SHARED_SSH_KEY_PATH").map(|p| format!("{p}.pub")),
        }
    }
}

/// How long `stop` waits for a VM to shut itself down before powering it off: `polls`
/// status reads, `every` apart.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownWait {
    pub polls: u32,
    pub every: std::time::Duration,
}

/// 90 s, read every 5 s: an Ubuntu VM shuts down in seconds; one that hasn't by then is hung
/// (the case `reset` is for in `restart_pod`), and waiting longer only holds up the command.
pub const SHUTDOWN_WAIT: ShutdownWait = ShutdownWait { polls: 18, every: std::time::Duration::from_secs(5) };

impl ShutdownWait {
    /// The whole wait, as the fallback's note says it.
    fn total(self) -> std::time::Duration {
        self.every * self.polls
    }
}

pub struct HetznerProvider {
    api_key: String,
    client: Client,
    base: String,
    opts: HetznerOpts,
    shutdown: ShutdownWait,
}

impl HetznerProvider {
    pub fn new(api_key: impl Into<String>, opts: HetznerOpts) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
            opts,
            shutdown: SHUTDOWN_WAIT,
        }
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.api_key)
    }

    /// Current Hetzner status string for a server (e.g. "running", "off", "starting").
    async fn server_status(&self, id: &str) -> Result<String> {
        let body = send_json(self.auth(self.client.get(format!("{}/servers/{}", self.base, id))), "hetzner get server").await?;
        Ok(body
            .get("server")
            .and_then(|s| s.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    /// POST a power action (`reset`/`poweron`/`reboot`/`shutdown`/`poweroff`) to a server.
    async fn power_action(&self, id: &str, action: &str) -> Result<()> {
        send_ok(
            self.auth(self.client.post(format!("{}/servers/{}/actions/{}", self.base, id, action))),
            &format!("hetzner {action}"),
        )
        .await
    }

    /// Every item of a read-only, paginated listing (`ssh_keys`, `server_types`): the `key`
    /// array of each page, following `meta.pagination.next_page` as [`Provider::list_pods`]
    /// does — bounded by [`MAX_PAGES`], and a page without the array is an error, never "none".
    async fn get_all(&self, key: &str) -> Result<Vec<Value>> {
        let ctx = format!("hetzner {}", key.replace('_', " "));
        let mut out = Vec::new();
        let mut page: u64 = 1;
        for _ in 0..MAX_PAGES {
            let url = format!("{}/{key}?page={page}&per_page={PER_PAGE}", self.base);
            let body = send_json(self.auth(self.client.get(url)), &ctx).await?;
            let (items, next) = items_page(&body, key, &ctx)?;
            out.extend(items.iter().cloned());
            match next {
                None => return Ok(out),
                Some(n) if n > page => page = n,
                Some(n) => return Err(Error::provider(format!("{ctx}: pagination did not advance (page {page} -> next_page {n})"))),
            }
        }
        Err(Error::provider(format!("{ctx}: more than {MAX_PAGES} pages")))
    }

    /// A refused create, explained from the project itself when that's what refused it. Live
    /// (an evals node, 2026-10-15): `404 SSH key not found` — the default key is named after
    /// the prefix (`arena9`) and the project only had `arena8` — and `cpx41` refused in nbg1,
    /// where the newer `cpx42` is sold; neither error said what to do.
    ///
    /// Only for a refusal the request itself can cause (`Other`, `Capacity`, `Rejected`) — not
    /// an auth failure, a throttle, a 5xx or a transport error — and with two read-only GETs:
    /// - `GET /ssh_keys`: a key the create named that the project lacks → which, the keys it
    ///   has, and what to set or upload ([`missing_key_advice`]);
    /// - `GET /server_types`: a type that can't be ordered at the location → the types that can
    ///   ([`type_advice`]), tagged `Rejected`: placement skips the option instead of waiting for
    ///   capacity that won't come (a "not available in" wording reads as capacity otherwise).
    ///
    /// The API's own answer stays first, and a lookup that fails or finds nothing wrong adds
    /// nothing: this only ever explains the error, never replaces it.
    async fn explain_create_error(&self, e: Error, server_type: &str) -> Error {
        use crate::error::ProviderErrorKind as K;
        let (mut kind, message) = match &e {
            Error::Provider { kind, message } if matches!(kind, K::Other | K::Capacity | K::Rejected) => (*kind, message.clone()),
            _ => return e,
        };
        let mut notes = Vec::new();
        if !self.opts.ssh_keys.is_empty() {
            if let Some(note) = self.get_all("ssh_keys").await.ok().and_then(|keys| missing_key_advice(&self.opts, &keys)) {
                notes.push(note);
            }
        }
        let now = crate::snapshot::unix_now();
        if let Some(note) =
            self.get_all("server_types").await.ok().and_then(|types| type_advice(&types, server_type, self.opts.location.as_deref(), now))
        {
            notes.push(note);
            kind = K::Rejected;
        }
        if notes.is_empty() {
            return e;
        }
        Error::Provider { kind, message: format!("{message} — {}", notes.join(" — ")) }
    }
}

/// What to do about the SSH keys a create named that the project doesn't have, or `None` when
/// it has them all. `keys`: the project's `GET /ssh_keys` items (`{"id": 2323, "name":
/// "arena8", "fingerprint": …, "public_key": …}`), matched by name — or by id, which
/// `HETZNER_SSH_KEY` may be. Names only: a key's material is never quoted. Pure.
fn missing_key_advice(opts: &HetznerOpts, keys: &[Value]) -> Option<String> {
    let names: Vec<&str> = keys.iter().filter_map(|k| k.get("name").and_then(Value::as_str)).collect();
    let ids: Vec<String> = keys.iter().filter_map(|k| k.get("id").and_then(Value::as_u64)).map(|i| i.to_string()).collect();
    let missing: Vec<&String> = opts.ssh_keys.iter().filter(|k| !names.contains(&k.as_str()) && !ids.contains(k)).collect();
    let first = missing.first()?;
    let quoted = missing.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ");
    let upload = match &opts.pubkey_file {
        Some(f) => format!("upload {f} to it as `{first}` (console: Security → SSH keys)"),
        None => format!("upload the cohort key's public half (SHARED_SSH_KEY_PATH + .pub) to it as `{first}` (console: Security → SSH keys)"),
    };
    let unset = if opts.ssh_keys_from == "HETZNER_SSH_KEY" { "" } else { " (unset, the key is the one named after MACHINE_NAME_PREFIX)" };
    Some(if names.is_empty() {
        format!("the Hetzner project has no SSH keys at all, so not {quoted}: {upload}")
    } else {
        format!(
            "the Hetzner project has no SSH key {quoted} (it has: {}): set HETZNER_SSH_KEY in the config to one of those{unset}, \
             or {upload}",
            names.join(", ")
        )
    })
}

/// When an item's `deprecation` (`{"announced": …, "unavailable_after": …}`, or null) has taken
/// the type off sale: its `unavailable_after` has passed. Until then it can still be ordered.
fn off_sale(deprecation: Option<&Value>, now: u64) -> bool {
    deprecation
        .and_then(|d| d.get("unavailable_after"))
        .and_then(Value::as_str)
        .and_then(crate::reap::parse_rfc3339)
        .is_some_and(|at| at <= i64::try_from(now).unwrap_or(i64::MAX))
}

/// Whether server type `t` (a `GET /server_types` item) can be ordered in `location` now —
/// `None` when the item doesn't say. Read from its per-location list (`"locations": [{"name":
/// "nbg1", "deprecation": null}, …]`); an answer without one (older API) from its `prices`
/// (one entry per location it's sold in, `{"location": "nbg1", …}`) and its own `deprecation`.
/// Pure.
fn orderable_in(t: &Value, location: &str, now: u64) -> Option<bool> {
    if let Some(locs) = t.get("locations").and_then(Value::as_array) {
        let here = locs.iter().find(|l| l.get("name").and_then(Value::as_str) == Some(location));
        return Some(here.is_some_and(|l| !off_sale(l.get("deprecation"), now)));
    }
    let prices = t.get("prices").and_then(Value::as_array)?;
    Some(prices.iter().any(|p| p.get("location").and_then(Value::as_str) == Some(location)) && !off_sale(t.get("deprecation"), now))
}

/// Whether `t`'s retirement from `location` has been announced (still orderable, but not a
/// type to suggest): a deprecation on its entry there — or, in an older answer, on the type.
fn deprecated_in(t: &Value, location: &str) -> bool {
    let set = |d: Option<&Value>| d.is_some_and(|d| !d.is_null());
    match t.get("locations").and_then(Value::as_array) {
        Some(locs) => locs.iter().any(|l| l.get("name").and_then(Value::as_str) == Some(location) && set(l.get("deprecation"))),
        None => t.get("deprecated").and_then(Value::as_bool) == Some(true) || set(t.get("deprecation")),
    }
}

/// `cx23 (2 vCPU, 4 GB)` — `, arm` for an Arm type — from a `GET /server_types` item
/// (`cores`, `memory` in GB, `architecture`).
fn type_label(t: &Value) -> String {
    let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
    let mut size = Vec::new();
    if let Some(c) = t.get("cores").and_then(Value::as_u64) {
        size.push(format!("{c} vCPU"));
    }
    if let Some(m) = t.get("memory").and_then(Value::as_f64) {
        size.push(format!("{m} GB"));
    }
    if t.get("architecture").and_then(Value::as_str) == Some("arm") {
        size.push("arm".into());
    }
    if size.is_empty() {
        name.to_string()
    } else {
        format!("{name} ({})", size.join(", "))
    }
}

/// What to say when server type `ty` can't be ordered in `location` (`types`: the `GET
/// /server_types` items): the types that can — not deprecated there — in the API's order, to
/// pick a `--server-type` from. `None` when `ty` can be ordered there, or the answer doesn't
/// say. Without a location only a type Hetzner doesn't have at all is reported. Pure.
fn type_advice(types: &[Value], ty: &str, location: Option<&str>, now: u64) -> Option<String> {
    let is = |t: &&Value| {
        t.get("name").and_then(Value::as_str) == Some(ty) || t.get("id").and_then(Value::as_u64).is_some_and(|i| i.to_string() == ty)
    };
    let why = match (types.iter().find(is), location) {
        (None, _) => format!("Hetzner has no server type `{ty}`"),
        (Some(t), Some(loc)) if orderable_in(t, loc, now) == Some(false) => format!("server type `{ty}` can't be ordered in {loc}"),
        _ => return None,
    };
    let Some(loc) = location else {
        return Some(format!("{why} (`arena api get hetzner server_types` lists them)"));
    };
    let offered: Vec<String> =
        types.iter().filter(|t| orderable_in(t, loc, now) == Some(true) && !deprecated_in(t, loc)).map(type_label).collect();
    Some(if offered.is_empty() {
        format!("{why}, and Hetzner lists no type there (`arena api get hetzner server_types`) — pick another HETZNER_LOCATION")
    } else {
        format!(
            "{why}; available there: {} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another \
             HETZNER_LOCATION",
            offered.join(", ")
        )
    })
}

/// The create-body fields this backend sets itself, refused in `--api-json` /
/// `CREATE_EXTRA_JSON` (Hetzner takes the VM's shape from its `HETZNER_*` config).
const MANAGED: &[Managed] = &[
    managed(&["name"], "the machine name"),
    managed(&["server_type"], "--server-type / HETZNER_SERVER_TYPE"),
    managed(&["image"], "HETZNER_IMAGE"),
    managed(&["location"], "HETZNER_LOCATION"),
    managed(&["ssh_keys"], "HETZNER_SSH_KEY (the cohort key)"),
    managed(&["start_after_create"], "always on"),
];

/// The server type a create of `spec` asks for: `--server-type` (`spec.server_type`), else
/// `HETZNER_SERVER_TYPE`. Pure.
fn server_type<'a>(opts: &'a HetznerOpts, spec: &'a PodSpec) -> &'a str {
    spec.server_type.as_deref().unwrap_or(&opts.server_type)
}

/// The `POST /servers` body. Pure. Hetzner's `image` is an ID *or* a name: a system image is
/// a name ("ubuntu-24.04"), but a snapshot has no name — it's referenced by numeric id. Send
/// all-digits as an int so HETZNER_IMAGE can be a snapshot id. `spec.api_extra` is
/// deep-merged in last, minus [`MANAGED`].
fn create_payload(opts: &HetznerOpts, spec: &PodSpec) -> Result<Value> {
    let image = match opts.image.parse::<u64>() {
        Ok(id) => json!(id),
        Err(_) => json!(opts.image),
    };
    let mut payload = json!({
        "name": spec.name,
        "server_type": server_type(opts, spec),
        "image": image,
        "start_after_create": true,
    });
    if let Some(loc) = &opts.location {
        payload["location"] = json!(loc);
    }
    if !opts.ssh_keys.is_empty() {
        payload["ssh_keys"] = json!(opts.ssh_keys);
    }
    apiextra::apply(&mut payload, spec.api_extra.as_ref(), MANAGED, None)?;
    Ok(payload)
}

fn parse_server(v: &Value) -> Pod {
    Pod {
        id: v
            .get("id")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default(),
        name: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        provider: "hetzner".into(),
        status: v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_uppercase(),
        // Repurpose the "type" column to show the CPU server type (no GPU here).
        gpu_type: v
            .get("server_type")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(String::from),
        // Hourly price from the embedded server_type.prices (Hetzner returns them as
        // strings, per location). Prefer the price for the server's own location, else the
        // first listed; use gross (what's actually billed). EUR — the dashboard column is
        // currency-agnostic, so it sits alongside runpod's USD as a plain cost/hr.
        cost_per_hr: v.get("server_type").and_then(|t| t.get("prices")).and_then(Value::as_array).and_then(
            |prices| {
                let loc = v
                    .get("datacenter")
                    .and_then(|d| d.get("location"))
                    .and_then(|l| l.get("name"))
                    .and_then(Value::as_str);
                prices
                    .iter()
                    .find(|p| p.get("location").and_then(Value::as_str) == loc)
                    .or_else(|| prices.first())
                    .and_then(|p| p.get("price_hourly"))
                    .and_then(|h| h.get("gross").or_else(|| h.get("net")))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
            },
        ),
        ssh_ip: v
            .get("public_net")
            .and_then(|n| n.get("ipv4"))
            .and_then(|i| i.get("ip"))
            .and_then(Value::as_str)
            .map(String::from),
        // Cloud VMs expose plain SSH on 22.
        ssh_port: Some(22),
        // CPU VMs: no GPUs, and Hetzner has no per-server maintenance window in the API.
        gpu_count: None,
        maintenance: None,
        machine_id: None,
        // Hetzner's delete/rebuild *protection* is a different thing; `pods lock` is
        // RunPod v2 only.
        locked: None,
        proxy_only: false,
        // A Hetzner server bills whole while it exists: its disk is never priced apart.
        disk_gb: None,
        volume_gb: None,
        // `protection.delete` (a bool on every server): the API refuses a DELETE while it's
        // set. Read for the auto-reaper, which never reaps a protected server; anything but a
        // bool there is `None` (unknown — never read as "unprotected").
        delete_protected: v.get("protection").and_then(|p| p.get("delete")).and_then(Value::as_bool),
        // `created`: ISO-8601 with an offset (`2016-01-30T23:50:00+00:00`).
        created_at: v
            .get("created")
            .and_then(Value::as_str)
            .and_then(crate::reap::parse_rfc3339)
            .and_then(|t| u64::try_from(t).ok()),
        // The VM's own id (cloud-init's instance id) is the reaper's identity check.
        identity: None,
        status_detail: None,
    }
}

/// Whether a listed server is the backup archive VM (`arena archive`): it carries the
/// label key [`crate::archive::LABEL_KEY`] (any value — any cohort's archive). The fleet never
/// sees it, so no fleet command (`pods *`, `proxy`, `snapshot`, the TUI, `teardown`'s pod
/// item) can list, stop, restart or terminate it — the archive is managed only by `arena
/// archive`. The test is on the label alone, never the name: an *unlabelled* server that
/// merely looks like an archive (`<prefix>-archive` made by hand) stays visible to the fleet
/// and to `teardown --check`, instead of vanishing from every view.
pub(crate) fn is_archive_server(v: &Value) -> bool {
    v.get("labels").and_then(Value::as_object).is_some_and(|l| l.contains_key(crate::archive::LABEL_KEY))
}

/// Servers per page we ask for (Hetzner's maximum; the default is 25).
const PER_PAGE: u64 = 50;
/// Hard stop for the pagination loop (50 × 20 = 1000 servers — far beyond any cohort).
const MAX_PAGES: u64 = 20;

/// One page of a 2xx `GET /servers` body: the `servers` array plus the next page number
/// (`meta.pagination.next_page`, `None` on the last page or when no pagination is given).
/// A body with no `servers` array is schema drift and an error — NOT an empty list, which
/// the proxy merge would read as "every Hetzner server terminated". Fail closed.
fn servers_page(body: &Value) -> Result<(&Vec<Value>, Option<u64>)> {
    items_page(body, "servers", "hetzner list")
}

/// One page of any 2xx Hetzner listing: its `key` array (required — fail closed, as
/// [`servers_page`]) and `meta.pagination.next_page`. `ctx` starts the error.
fn items_page<'a>(body: &'a Value, key: &str, ctx: &str) -> Result<(&'a Vec<Value>, Option<u64>)> {
    let items = body.get(key).and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!("{ctx}: unexpected response shape (no {key} array): {}", super::body_excerpt(body)))
    })?;
    let next = body
        .get("meta")
        .and_then(|m| m.get("pagination"))
        .and_then(|p| p.get("next_page"))
        .and_then(Value::as_u64);
    Ok((items, next))
}

#[async_trait]
impl Provider for HetznerProvider {
    fn name(&self) -> &'static str {
        "hetzner"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        // CPU VM: describe what we actually send (server type/image/location), not the
        // GPU fields the spec carries and we ignore.
        let loc = self
            .opts
            .location
            .as_deref()
            .map(|l| format!(", location {l}"))
            .unwrap_or_default();
        format!("{} CPU VM, image {}{}", server_type(&self.opts, spec), self.opts.image, loc)
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        // Follow Hetzner's pagination: it returns 25 servers per page by default, and a
        // server silently missing from page 1 would read as "terminated" to the proxy
        // merge (dropping its forward). A pagination loop that doesn't advance or runs past
        // MAX_PAGES fails the listing rather than returning a partial fleet as complete.
        let mut out = Vec::new();
        let mut page: u64 = 1;
        for _ in 0..MAX_PAGES {
            // Status first, then decode (crate::http): an auth/HTML error stays classified.
            let url = format!("{}/servers?page={page}&per_page={PER_PAGE}", self.base);
            let body = send_json(self.auth(self.client.get(url)), "hetzner list").await?;
            let (servers, next) = servers_page(&body)?;
            out.extend(servers.iter().filter(|v| !is_archive_server(v)).map(parse_server));
            match next {
                None => return Ok(out),
                Some(n) if n > page => page = n,
                Some(n) => {
                    return Err(Error::provider(format!(
                        "hetzner list: pagination did not advance (page {page} -> next_page {n})"
                    )))
                }
            }
        }
        Err(Error::provider(format!(
            "hetzner list: more than {MAX_PAGES} pages of servers — refusing a partial listing"
        )))
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let payload = create_payload(&self.opts, spec)?;
        let body = match send_json(self.auth(self.client.post(format!("{}/servers", self.base)).json(&payload)), "hetzner create").await {
            Ok(body) => body,
            Err(e) => return Err(self.explain_create_error(e, server_type(&self.opts, spec)).await),
        };
        // The created server is under `server`; parse what's there (IP may not be
        // populated until it finishes provisioning — `pods up` polls for that).
        Ok(parse_server(body.get("server").unwrap_or(&body)))
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        self.stop_pod_outcome(id).await.map(|_| ())
    }

    async fn stop_pod_outcome(&self, id: &str) -> Result<StopOutcome> {
        // `poweroff` pulls the plug: a file written just before the stop was lost (live R,
        // 2026-10-09), and a hard power-off risks the filesystem too. `shutdown` asks the OS
        // (ACPI) to shut down, flushing its writes; a hung OS ignores it, so after a bounded
        // wait the VM is powered off after all — and the caller is told it was forced.
        self.power_action(id, "shutdown").await?;
        let wait = self.shutdown;
        for _ in 0..wait.polls {
            tokio::time::sleep(wait.every).await;
            // A failed read is "not off yet": the power-off fallback still bounds the stop.
            if self.server_status(id).await.is_ok_and(|s| s == "off") {
                return Ok(StopOutcome::Stopped);
            }
        }
        self.power_action(id, "poweroff").await?;
        Ok(StopOutcome::ForcedOff { waited: wait.total() })
    }

    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        // A Hetzner server is a VM with a real disk: reset/poweroff/poweron keep it.
        false
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // "Restart" must recover a pod *in place*, including a wedged one — and the disk is
        // preserved either way. Hetzner's `reboot` is a soft ACPI signal that a hung OS just
        // ignores (so it silently no-ops), which is why restart "didn't work". Use a hard
        // `reset` (forced power-cycle) on a running server; a stopped server can't be reset,
        // so bring it back up with `poweron`.
        let status = self.server_status(id).await?;
        match status.as_str() {
            "off" | "stopped" => self.power_action(id, "poweron").await,
            _ => self.power_action(id, "reset").await,
        }
    }

    async fn start_pod(&self, id: &str) -> Result<()> {
        // A powered-off VM keeps its disk: poweron is all a start is.
        self.power_action(id, "poweron").await
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/servers/{}", self.base, id))), "hetzner terminate").await
    }

    fn check_create_extra(&self, extra: &Extra) -> Result<()> {
        apiextra::check(extra, MANAGED, None)
    }

    fn preview_create_body(&self, spec: &PodSpec) -> Result<Value> {
        create_payload(&self.opts, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status first, then decode: a bad token's 401 (or a proxy's HTML) is classified by
    /// its status, never "error decoding response body". Loopback server, real request path.
    #[tokio::test]
    async fn error_statuses_are_classified_before_decoding() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, client, serve};
        let srv = serve(vec![
            canned(401, "application/json", r#"{"error":{"code":"unauthorized","message":"unable to authenticate"}}"#),
            canned(503, "text/html", "<html>Service Unavailable</html>"),
            canned(200, "application/json", r#"{"server":{"id":5,"status":"off"}}"#),
            canned(403, "text/plain", "Forbidden"),
        ]);
        let p = HetznerProvider {
            api_key: "BOGUS".into(),
            client: client(),
            base: srv.base.clone(),
            opts: HetznerOpts::default(),
            shutdown: SHUTDOWN_WAIT,
        };
        let e = p.list_pods().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("hetzner list HTTP 401") && e.to_string().contains("unable to authenticate"), "{e}");
        let e = p.stop_pod("5").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Transient), "{e}");
        // restart: reads the status (off) → poweron, which is refused.
        let e = p.restart_pod("5").await.unwrap_err();
        assert!(e.to_string().contains("hetzner poweron HTTP 403"), "{e}");
        assert_eq!(e.kind(), Some(K::Auth));
        assert_eq!(srv.requests.lock().unwrap().last().unwrap(), "POST /servers/5/actions/poweron HTTP/1.1");
    }

    /// Live R (2026-10-09): `stop` was a hard `poweroff` and an unsynced file was lost. Now
    /// it asks the VM to shut down and waits for `off`; only a VM that doesn't get there in
    /// the bounded wait is powered off — and the outcome says so, for the output.
    #[tokio::test]
    async fn stop_shuts_down_gracefully_and_forces_a_power_off_only_after_the_wait() {
        use crate::http::test_server::{canned, client, serve};
        let action = || canned(201, "application/json", r#"{"action":{"id":1,"status":"running"}}"#);
        let status = |s: &str| canned(200, "application/json", &format!(r#"{{"server":{{"id":5,"status":"{s}"}}}}"#));
        let provider = |base: &str| HetznerProvider {
            api_key: "k".into(),
            client: client(),
            base: base.to_string(),
            opts: HetznerOpts::default(),
            shutdown: ShutdownWait { polls: 3, every: std::time::Duration::ZERO },
        };
        // Shuts down within the wait (a failed read on the way is just "not yet"): stopped,
        // never powered off.
        let srv = serve(vec![action(), status("running"), canned(503, "text/html", "<html>busy</html>"), status("off")]);
        assert_eq!(provider(&srv.base).stop_pod_outcome("5").await.unwrap(), StopOutcome::Stopped);
        assert_eq!(
            *srv.requests.lock().unwrap(),
            ["POST /servers/5/actions/shutdown HTTP/1.1", "GET /servers/5 HTTP/1.1", "GET /servers/5 HTTP/1.1", "GET /servers/5 HTTP/1.1"]
        );
        // Still running after every read: powered off, and the outcome says it was forced.
        let srv = serve(vec![action(), status("running"), status("stopping"), status("running"), action()]);
        let p = provider(&srv.base);
        let out = p.stop_pod_outcome("5").await.unwrap();
        assert_eq!(out, StopOutcome::ForcedOff { waited: std::time::Duration::ZERO });
        assert_eq!(srv.requests.lock().unwrap().last().unwrap(), "POST /servers/5/actions/poweroff HTTP/1.1");
        assert_eq!(srv.requests.lock().unwrap().len(), 5);
        // `stop_pod` is the same graceful stop (the TUI and every other caller get it too).
        let srv = serve(vec![action(), status("off")]);
        provider(&srv.base).stop_pod("5").await.unwrap();
        assert_eq!(srv.requests.lock().unwrap()[0], "POST /servers/5/actions/shutdown HTTP/1.1");
        // A refused shutdown is the stop's error; nothing is forced.
        let srv = serve(vec![canned(403, "application/json", r#"{"error":{"code":"forbidden","message":"no"}}"#)]);
        let e = provider(&srv.base).stop_pod_outcome("5").await.unwrap_err();
        assert!(e.to_string().contains("hetzner shutdown HTTP 403"), "{e}");
        assert_eq!(srv.requests.lock().unwrap().len(), 1);
        // The real wait is 90 s, read every 5 s.
        assert_eq!(SHUTDOWN_WAIT.total(), std::time::Duration::from_secs(90));
    }

    /// S9: the fleet listing drops every server carrying the archive label key (any cohort's
    /// archive) and keeps all others — an unlabelled `<prefix>-archive` lookalike included —
    /// across pages, over the real request path.
    #[tokio::test]
    async fn list_pods_omits_archive_labelled_servers_and_keeps_the_rest() {
        use crate::http::test_server::{canned, client, serve};
        let page1 = json!({
            "servers": [
                {"id": 1, "name": "arena9-apple", "status": "running", "labels": {}},
                {"id": 2, "name": "arena9-archive", "status": "running", "labels": {"arena-archive": "arena9"}},
                {"id": 3, "name": "arena8-archive", "status": "running", "labels": {"arena-archive": "arena8", "x": "y"}},
            ],
            "meta": {"pagination": {"page": 1, "next_page": 2}}
        });
        let page2 = json!({
            "servers": [
                {"id": 4, "name": "devtest-archive", "status": "running"},
                {"id": 5, "name": "odd", "status": "running", "labels": {"arena-archive-tier": "snap"}},
                {"id": 6, "name": "arena9-bloom", "status": "off", "labels": {"team": "arena-archive"}},
            ],
            "meta": {"pagination": {"page": 2, "next_page": null}}
        });
        let srv = serve(vec![
            canned(200, "application/json", &page1.to_string()),
            canned(200, "application/json", &page2.to_string()),
        ]);
        let p = HetznerProvider { api_key: "k".into(), client: client(), base: srv.base.clone(), opts: HetznerOpts::default(), shutdown: SHUTDOWN_WAIT };
        let names: Vec<String> = p.list_pods().await.unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["arena9-apple", "devtest-archive", "odd", "arena9-bloom"]);
    }

    #[test]
    fn servers_page_reads_array_and_next_page() {
        let last = json!({"servers": [{"id": 1}], "meta": {"pagination": {"page": 1, "next_page": null}}});
        let (servers, next) = servers_page(&last).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(next, None);

        let more = json!({"servers": [], "meta": {"pagination": {"page": 1, "next_page": 2}}});
        assert_eq!(servers_page(&more).unwrap().1, Some(2));

        // No meta at all (older/mock responses) = a single page.
        assert_eq!(servers_page(&json!({"servers": []})).unwrap().1, None);
    }

    #[test]
    fn servers_page_fails_closed_on_schema_drift() {
        for body in [json!({}), json!({"servers": null}), json!({"error": {"code": "x"}}), json!([])] {
            let err = servers_page(&body).unwrap_err().to_string();
            assert!(err.contains("unexpected response shape"), "{body}: {err}");
        }
    }

    #[test]
    fn parses_server_with_ip_and_type() {
        let v = json!({
            "id": 42,
            "name": "arena8-apple",
            "status": "running",
            "server_type": {"name": "cx22", "cores": 2},
            "public_net": {"ipv4": {"ip": "1.2.3.4"}}
        });
        let p = parse_server(&v);
        assert_eq!(p.id, "42");
        assert_eq!(p.name, "arena8-apple");
        assert_eq!(p.status, "RUNNING");
        assert_eq!(p.gpu_type.as_deref(), Some("cx22"));
        assert_eq!(p.ssh_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(p.ssh_port, Some(22)); // plain SSH on a cloud VM
        assert_eq!(p.provider, "hetzner");
        assert_eq!(p.created_at, None, "no `created`: unknown");
    }

    /// `created` (the API's ISO-8601 with an offset) is the server's creation — what the
    /// reaper's coverage checks reach back to, across reboots (R2-1). Anything odd: unknown.
    #[test]
    fn parses_the_servers_creation_time() {
        let at = |c: serde_json::Value| parse_server(&json!({"id": 1, "name": "devtest-a", "status": "running", "created": c})).created_at;
        assert_eq!(at(json!("2016-01-30T23:50:00+00:00")), Some(1_454_197_800));
        assert_eq!(at(json!("2016-01-31T00:50:00+01:00")), Some(1_454_197_800));
        for odd in [json!("yesterday"), json!(1_454_197_800), json!(null), json!("2016-02-30T00:00:00+00:00")] {
            assert_eq!(at(odd.clone()), None, "{odd}");
        }
    }

    /// `protection.delete` is read for the reaper (on, off) — and anything else, a missing
    /// block or a non-bool, is unknown, never "unprotected". It is not a lock: `locked` stays
    /// `None`, so stop/restart's lock refusals don't read it.
    #[test]
    fn delete_protection_is_read_and_unknown_when_odd() {
        let with = |protection: serde_json::Value| {
            let mut v = json!({"id": 9, "name": "devtest-alpha", "status": "running"});
            if !protection.is_null() {
                v["protection"] = protection;
            }
            parse_server(&v)
        };
        assert_eq!(with(json!({"delete": true, "rebuild": true})).delete_protected, Some(true));
        assert_eq!(with(json!({"delete": false, "rebuild": false})).delete_protected, Some(false));
        for odd in [json!(null), json!({}), json!({"delete": "true"}), json!({"delete": 1}), json!("on")] {
            let p = with(odd.clone());
            assert_eq!(p.delete_protected, None, "{odd}");
            assert_eq!(p.locked, None, "{odd}");
        }
        assert_eq!(with(json!({"delete": true})).locked, None, "protection is not a lock");
    }

    #[test]
    fn parses_initializing_server_without_ip() {
        // Right after create, the IP block may be absent — must not panic.
        let v = json!({"id": 7, "name": "arena8-autumn", "status": "initializing"});
        let p = parse_server(&v);
        assert_eq!(p.status, "INITIALIZING");
        assert_eq!(p.ssh_ip, None);
        assert_eq!(p.ssh_port, Some(22));
    }

    fn spec(name: &str, server_type: Option<&str>) -> PodSpec {
        PodSpec {
            name: name.into(),
            server_type: server_type.map(String::from),
            ..PodSpec::from_config(&crate::config::Config::parse(""))
        }
    }

    /// `--server-type` (`spec.server_type`) is the create body's `server_type` and what the
    /// dry run describes; without it, `HETZNER_SERVER_TYPE` (else cx23) as before.
    #[test]
    fn the_server_type_flag_goes_into_the_create_body_over_config() {
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("HETZNER_SERVER_TYPE=cx33\n"));
        let p = HetznerProvider::new("k", opts.clone());
        for (flag, want) in [(Some("cpx42"), "cpx42"), (None, "cx33")] {
            let body = create_payload(&opts, &spec("devtest-a", flag)).unwrap();
            assert_eq!(body["server_type"], want, "{flag:?}");
            assert_eq!(p.preview_create_body(&spec("devtest-a", flag)).unwrap()["server_type"], want);
            assert!(p.describe(&spec("devtest-a", flag)).starts_with(&format!("{want} CPU VM")), "{flag:?}");
        }
        assert_eq!(create_payload(&HetznerOpts::default(), &spec("x", None)).unwrap()["server_type"], "cx23");
    }

    /// The options as `build` reads them: the default key named after the prefix — and that
    /// it was the default, for the missing-key advice — or HETZNER_SSH_KEY's; the cohort key's
    /// `.pub` to upload. Unset keys keep today's defaults.
    #[test]
    fn options_from_config_say_where_the_key_name_came_from() {
        let o = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nSHARED_SSH_KEY_PATH=/k/arena9_key\n"));
        assert_eq!((o.ssh_keys.as_slice(), o.ssh_keys_from), (&["arena9".to_string()][..], "MACHINE_NAME_PREFIX"));
        assert_eq!(o.pubkey_file.as_deref(), Some("/k/arena9_key.pub"));
        assert_eq!((o.server_type.as_str(), o.image.as_str(), o.location.as_deref()), ("cx23", "ubuntu-24.04", Some("nbg1")));
        let o = HetznerOpts::from_config(&crate::config::Config::parse(
            "MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=arena8\nHETZNER_LOCATION=fsn1\nHETZNER_SERVER_TYPE=cpx42\n",
        ));
        assert_eq!((o.ssh_keys.as_slice(), o.ssh_keys_from), (&["arena8".to_string()][..], "HETZNER_SSH_KEY"));
        assert_eq!((o.server_type.as_str(), o.location.as_deref(), o.pubkey_file), ("cpx42", Some("fsn1"), None));
        // An empty HETZNER_SSH_KEY is unset, as before: the prefix's key.
        let o = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=\n"));
        assert_eq!((o.ssh_keys[0].as_str(), o.ssh_keys_from), ("arena9", "MACHINE_NAME_PREFIX"));
    }

    /// The shape relied on: `GET /server_types` items with a per-location list (`locations[].name`,
    /// `locations[].deprecation.unavailable_after`), `cores`, `memory` (GB), `architecture`.
    /// cpx41 went off sale in nbg1 (it's still in fsn1/ash); cpx22's retirement there is only
    /// announced (orderable, not suggested); ccx13 is US-only; cax11 is Arm.
    const SERVER_TYPES: &str = r#"{"server_types": [
        {"id": 104, "name": "cx23", "cores": 2, "memory": 4.0, "disk": 40, "architecture": "x86", "cpu_type": "shared",
         "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.0060", "gross": "0.0071"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.0060", "gross": "0.0071"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null}, {"id": 2, "name": "nbg1", "deprecation": null}]},
        {"id": 11, "name": "cpx41", "cores": 8, "memory": 16.0, "architecture": "x86",
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null},
                       {"id": 2, "name": "nbg1", "deprecation": {"announced": "2025-09-01T00:00:00+00:00", "unavailable_after": "2025-12-31T00:00:00+00:00"}},
                       {"id": 4, "name": "ash", "deprecation": null}]},
        {"id": 113, "name": "cpx42", "cores": 8, "memory": 16.0, "architecture": "x86", "locations": [{"id": 2, "name": "nbg1", "deprecation": null}]},
        {"id": 111, "name": "cpx22", "cores": 2, "memory": 4.0, "architecture": "x86",
         "locations": [{"id": 2, "name": "nbg1", "deprecation": {"announced": "2026-09-01T00:00:00+00:00", "unavailable_after": "2099-01-01T00:00:00+00:00"}}]},
        {"id": 45, "name": "cax11", "cores": 2, "memory": 4.0, "architecture": "arm", "locations": [{"id": 2, "name": "nbg1", "deprecation": null}]},
        {"id": 96, "name": "ccx13", "cores": 2, "memory": 8.0, "architecture": "x86", "locations": [{"id": 4, "name": "ash", "deprecation": null}]}
    ], "meta": {"pagination": {"page": 1, "next_page": null}}}"#;

    /// `GET /ssh_keys` as the project answers it (key material included — never quoted back).
    const SSH_KEYS: &str = r#"{"ssh_keys": [
        {"id": 2323, "name": "arena8", "fingerprint": "b7:2f:30", "public_key": "ssh-ed25519 AAAAC3NzaArena8 arena8", "labels": {}},
        {"id": 2324, "name": "ops", "fingerprint": "c1:00:aa", "public_key": "ssh-ed25519 AAAAC3NzaOps ops", "labels": {}}
    ], "meta": {"pagination": {"page": 1, "next_page": null}}}"#;

    fn provider(base: &str, opts: HetznerOpts) -> HetznerProvider {
        use crate::http::test_server::client;
        HetznerProvider { api_key: "k".into(), client: client(), base: base.to_string(), opts, shutdown: SHUTDOWN_WAIT }
    }

    /// Item 4, live (evals node, 2026-10-15): `pods up --provider hetzner` asked for the key named
    /// after the prefix (`arena9`), the project only had `arena8`, and the create failed with a
    /// bare `404 SSH key not found`. The error now names the key it looked for, the project's
    /// keys, and both ways out — HETZNER_SSH_KEY or uploading the cohort key under that name —
    /// after the API's own answer, over the real request path; the project's key material is
    /// never printed.
    #[tokio::test]
    async fn a_missing_ssh_key_names_the_projects_keys_and_what_to_do() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        let srv = serve(vec![
            canned(404, "application/json", r#"{"error":{"code":"not_found","message":"SSH key not found","details":{}}}"#),
            canned(200, "application/json", SSH_KEYS),
            canned(200, "application/json", SERVER_TYPES),
        ]);
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nSHARED_SSH_KEY_PATH=/k/arena9_key\n"));
        let e = provider(&srv.base, opts).create_pod(&spec("arena9-evals", None)).await.unwrap_err();
        let m = e.to_string();
        assert!(m.contains("hetzner create HTTP 404") && m.contains("SSH key not found"), "the API's answer first: {m}");
        assert!(
            m.contains(
                "the Hetzner project has no SSH key `arena9` (it has: arena8, ops): set HETZNER_SSH_KEY in the config to one of \
                 those (unset, the key is the one named after MACHINE_NAME_PREFIX), or upload /k/arena9_key.pub to it as `arena9`"
            ),
            "{m}"
        );
        assert!(!m.contains("ssh-ed25519") && !m.contains("AAAA"), "no key material: {m}");
        assert!(!m.contains("server type"), "cx23 is sold in nbg1 — nothing to say about it: {m}");
        assert_eq!(e.kind(), Some(K::Other));
        assert_eq!(
            *srv.requests.lock().unwrap(),
            ["POST /servers HTTP/1.1", "GET /ssh_keys?page=1&per_page=50 HTTP/1.1", "GET /server_types?page=1&per_page=50 HTTP/1.1"]
        );
        assert_eq!(serde_json::from_str::<Value>(&srv.bodies.lock().unwrap()[0]).unwrap()["ssh_keys"], json!(["arena9"]));
    }

    /// Item 4: `cpx41` is refused in nbg1, where `cpx42` is sold. Whatever Hetzner's wording —
    /// an `invalid_input`, or one that reads like capacity (`not available in`) — the error lists
    /// the types nbg1 sells (not the retired, the announced-retiring or the US-only ones) and how
    /// to pick one, and is `Rejected`: never retried as if capacity might come back. The
    /// `--server-type` reached the request body.
    #[tokio::test]
    async fn a_type_the_location_doesnt_sell_lists_the_types_it_does() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        for (status, refusal, was) in [
            (422, r#"{"error":{"code":"invalid_input","message":"unsupported location for server type","details":{}}}"#, K::Other),
            (412, r#"{"error":{"code":"resource_unavailable","message":"server type cpx41 not available in nbg1"}}"#, K::Capacity),
        ] {
            assert_eq!(Error::provider_http(reqwest::StatusCode::from_u16(status).unwrap(), &refusal, "hetzner create").kind(), Some(was));
            let srv = serve(vec![
                canned(status, "application/json", refusal),
                canned(200, "application/json", SSH_KEYS),
                canned(200, "application/json", SERVER_TYPES),
            ]);
            let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=arena8\n"));
            let e = provider(&srv.base, opts).create_pod(&spec("arena9-evals", Some("cpx41"))).await.unwrap_err();
            let m = e.to_string();
            assert!(m.contains(&format!("hetzner create HTTP {status}")), "{m}");
            assert!(
                m.contains(
                    "server type `cpx41` can't be ordered in nbg1; available there: cx23 (2 vCPU, 4 GB), cpx42 (8 vCPU, 16 GB), \
                     cax11 (2 vCPU, 4 GB, arm) — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE)"
                ),
                "{m}"
            );
            assert!(!m.contains("SSH key"), "the key is there: {m}");
            assert_eq!(e.kind(), Some(K::Rejected), "{m}");
            assert_eq!(serde_json::from_str::<Value>(&srv.bodies.lock().unwrap()[0]).unwrap()["server_type"], "cpx41");
        }
    }

    /// What isn't the request's fault is left as it was, with no extra request: a bad token, a
    /// throttle, a 5xx. And a refusal the lookups can't explain — they fail, or find nothing
    /// wrong — keeps the API's own error, unchanged.
    #[tokio::test]
    async fn only_a_refusal_is_explained_and_a_failed_lookup_adds_nothing() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        let opts = || HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\n"));
        for (status, kind) in [(401, K::Auth), (429, K::RateLimited), (503, K::Transient)] {
            let srv = serve(vec![canned(status, "application/json", r#"{"error":{"code":"x","message":"no"}}"#)]);
            let e = provider(&srv.base, opts()).create_pod(&spec("arena9-a", None)).await.unwrap_err();
            assert_eq!(e.kind(), Some(kind), "{e}");
            assert_eq!(srv.requests.lock().unwrap().len(), 1, "{status}: nothing looked up");
        }
        let refusal = r#"{"error":{"code":"uniqueness_error","message":"server name is already used"}}"#;
        let want = Error::provider_http(reqwest::StatusCode::CONFLICT, &refusal, "hetzner create").to_string();
        // The lookups fail…
        let srv = serve(vec![
            canned(409, "application/json", refusal),
            canned(500, "text/html", "<html>oops</html>"),
            canned(200, "application/json", r#"{"unexpected": true}"#),
        ]);
        let e = provider(&srv.base, opts()).create_pod(&spec("arena9-a", None)).await.unwrap_err();
        assert_eq!((e.to_string(), e.kind()), (want.clone(), Some(K::Other)));
        // …or find the key and the type where they should be.
        let keys = r#"{"ssh_keys": [{"id": 1, "name": "arena9"}]}"#;
        let srv = serve(vec![canned(409, "application/json", refusal), canned(200, "application/json", keys), canned(200, "application/json", SERVER_TYPES)]);
        let e = provider(&srv.base, opts()).create_pod(&spec("arena9-a", None)).await.unwrap_err();
        assert_eq!((e.to_string(), e.kind()), (want, Some(K::Other)));
        assert_eq!(srv.requests.lock().unwrap().len(), 3);
    }

    /// One missing-key case: the configured key names, the setting that named them, the `.pub`
    /// to upload, the project's keys, and the advice expected.
    type KeyCase<'a> = (&'a [&'a str], &'static str, Option<&'a str>, &'a [Value], &'a str);

    /// The missing-key advice: by name or id; the keys the project has (none: upload is the way);
    /// which setting to change — HETZNER_SSH_KEY when it named the key; the file to upload. Pure.
    #[test]
    fn missing_key_advice_names_the_keys_and_the_way_out() {
        let keys: Vec<Value> = serde_json::from_str::<Value>(SSH_KEYS).unwrap()["ssh_keys"].as_array().unwrap().clone();
        let opts = |names: &[&str], from: &'static str, pubkey: Option<&str>| HetznerOpts {
            ssh_keys: names.iter().map(|s| s.to_string()).collect(),
            ssh_keys_from: from,
            pubkey_file: pubkey.map(String::from),
            ..HetznerOpts::default()
        };
        // Present, by name or by id: nothing to say.
        assert_eq!(missing_key_advice(&opts(&["arena8"], "MACHINE_NAME_PREFIX", None), &keys), None);
        assert_eq!(missing_key_advice(&opts(&["2324"], "HETZNER_SSH_KEY", None), &keys), None);
        let cases: &[KeyCase] = &[
            (
                &["arena9"],
                "MACHINE_NAME_PREFIX",
                Some("ssh/devtest_key.pub"),
                &keys,
                "the Hetzner project has no SSH key `arena9` (it has: arena8, ops): set HETZNER_SSH_KEY in the config to one of \
                 those (unset, the key is the one named after MACHINE_NAME_PREFIX), or upload ssh/devtest_key.pub to it as \
                 `arena9` (console: Security → SSH keys)",
            ),
            (
                &["arena7"],
                "HETZNER_SSH_KEY",
                None,
                &keys,
                "the Hetzner project has no SSH key `arena7` (it has: arena8, ops): set HETZNER_SSH_KEY in the config to one of \
                 those, or upload the cohort key's public half (SHARED_SSH_KEY_PATH + .pub) to it as `arena7` (console: \
                 Security → SSH keys)",
            ),
            (
                &["arena9"],
                "MACHINE_NAME_PREFIX",
                Some("/k/a.pub"),
                &[],
                "the Hetzner project has no SSH keys at all, so not `arena9`: upload /k/a.pub to it as `arena9` (console: \
                 Security → SSH keys)",
            ),
        ];
        for &(names, from, pubkey, have, want) in cases {
            assert_eq!(missing_key_advice(&opts(names, from, pubkey), have).as_deref(), Some(want), "{names:?}");
        }
    }

    /// Whether a type can be ordered somewhere, from either shape of the answer — the
    /// per-location list, or (older) its prices and its own deprecation — with a retirement
    /// counting only once its date has passed. Pure.
    #[test]
    fn orderable_reads_the_location_list_or_the_prices() {
        let types: Value = serde_json::from_str(SERVER_TYPES).unwrap();
        let t = |name: &str| types["server_types"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap().clone();
        let at = |s: &str| u64::try_from(crate::reap::parse_rfc3339(s).unwrap()).unwrap();
        let now = at("2026-10-16T00:00:00+00:00");
        let cases = [
            (t("cx23"), "nbg1", now, Some(true)),
            (t("cx23"), "ash", now, Some(false)),
            (t("cpx41"), "nbg1", now, Some(false)),
            (t("cpx41"), "nbg1", at("2025-11-01T00:00:00+00:00"), Some(true)), // before its date
            (t("cpx41"), "fsn1", now, Some(true)),
            (t("cpx22"), "nbg1", now, Some(true)), // announced, still sold
            // Older answers: no `locations`, a price per location sold in.
            (json!({"name": "cx22", "prices": [{"location": "nbg1"}], "deprecation": null}), "nbg1", now, Some(true)),
            (json!({"name": "cx22", "prices": [{"location": "fsn1"}]}), "nbg1", now, Some(false)),
            (
                json!({"name": "cx11", "prices": [{"location": "nbg1"}], "deprecated": true,
                       "deprecation": {"announced": "2024-01-01T00:00:00+00:00", "unavailable_after": "2024-06-01T00:00:00+00:00"}}),
                "nbg1",
                now,
                Some(false),
            ),
            (json!({"name": "mystery"}), "nbg1", now, None),
        ];
        for (t, loc, now, want) in cases {
            assert_eq!(orderable_in(&t, loc, now), want, "{} in {loc}", t["name"]);
        }
        assert!(deprecated_in(&t("cpx22"), "nbg1") && !deprecated_in(&t("cpx42"), "nbg1") && !deprecated_in(&t("cpx41"), "fsn1"));
        assert!(deprecated_in(&json!({"name": "cx11", "prices": [], "deprecated": true}), "nbg1"));
    }

    /// When to list the types, and which. Pure.
    #[test]
    fn type_advice_lists_the_types_on_sale_there() {
        let types: Vec<Value> = serde_json::from_str::<Value>(SERVER_TYPES).unwrap()["server_types"].as_array().unwrap().clone();
        let now = u64::try_from(crate::reap::parse_rfc3339("2026-10-16T00:00:00+00:00").unwrap()).unwrap();
        let sold = "cx23 (2 vCPU, 4 GB), cpx42 (8 vCPU, 16 GB), cax11 (2 vCPU, 4 GB, arm)";
        let cases: &[(&str, Option<&str>, Option<String>)] = &[
            ("cx23", Some("nbg1"), None),
            ("cpx22", Some("nbg1"), None), // retiring, still orderable: not this create's problem
            ("104", Some("nbg1"), None),   // by id
            ("cpx41", Some("nbg1"), Some(format!("server type `cpx41` can't be ordered in nbg1; available there: {sold} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION"))),
            ("cpx99", Some("nbg1"), Some(format!("Hetzner has no server type `cpx99`; available there: {sold} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION"))),
            ("cx23", Some("ash"), Some("server type `cx23` can't be ordered in ash; available there: cpx41 (8 vCPU, 16 GB), ccx13 (2 vCPU, 8 GB) — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION".into())),
            ("cx23", Some("hel1"), Some("server type `cx23` can't be ordered in hel1, and Hetzner lists no type there (`arena api get hetzner server_types`) — pick another HETZNER_LOCATION".into())),
            ("cpx41", None, None),
            ("cpx99", None, Some("Hetzner has no server type `cpx99` (`arena api get hetzner server_types` lists them)".into())),
        ];
        for (ty, loc, want) in cases {
            assert_eq!(type_advice(&types, ty, *loc, now), want.clone(), "{ty} in {loc:?}");
        }
        // An answer that doesn't say (no locations, no prices): nothing claimed.
        assert_eq!(type_advice(&[json!({"name": "cx23"})], "cx23", Some("nbg1"), now), None);
    }
}
