//! Hetzner Cloud backend (https://api.hetzner.cloud/v1) — CPU-only VMs.
//!
//! Unlike RunPod/Vast this isn't a GPU container host: it provisions an ordinary
//! cloud VM, so the GPU-centric fields of a [`PodSpec`] don't apply. Hetzner instead
//! takes its sizing (`server_type`), OS (`image`), `locations`, and `ssh_keys` from
//! its own `HETZNER_*` config, read once at construction; from a `PodSpec` it uses
//! only the `name` and `server_type` (`--server-type`, over `HETZNER_SERVER_TYPE`). VMs come
//! up on a real public IP with SSH on `:22`, so they slot straight into the same
//! proxy/`pods up` flow as the GPU providers.
//!
//! Where a VM goes is no one's preference by default ([`Locations`]): with `HETZNER_LOCATION`
//! unset, a create tries every location that sells the server type, nearest first (timed from
//! where arena runs, [`Distances`]), and moves on when one has no room (live 2026-10-26: cx23
//! read "low availability" in fsn1, nbg1 and hel1 at once). A location set there pins it, or a
//! list orders the walk.
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
use std::collections::HashMap;
use std::time::Duration;

pub(crate) const BASE: &str = "https://api.hetzner.cloud/v1";

/// Hetzner-specific creation options, sourced from `HETZNER_*` config keys.
#[derive(Debug, Clone)]
pub struct HetznerOpts {
    pub server_type: String,
    pub image: String,
    /// Where creates go ([`Locations`]; `HETZNER_LOCATION`). Default: any location.
    pub locations: Locations,
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
            // HETZNER_SERVER_TYPE. Any location that sells it ([`Locations::ANY`]): never
            // Hetzner's own auto-placement, which could land an EU-only x86 type on a US DC
            // and fail with "error during placement".
            server_type: "cx23".into(),
            image: "ubuntu-24.04".into(),
            locations: Locations::ANY,
            ssh_keys: Vec::new(),
            ssh_keys_from: "MACHINE_NAME_PREFIX",
            pubkey_file: None,
        }
    }
}

impl HetznerOpts {
    /// The server type a create of `spec` asks for: `--server-type`, else `HETZNER_SERVER_TYPE`.
    pub fn server_type_for<'a>(&'a self, spec: &'a PodSpec) -> &'a str {
        server_type(self, spec)
    }

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
            server_type: cfg.get("HETZNER_SERVER_TYPE").map_or(d.server_type, String::from),
            image: cfg.get("HETZNER_IMAGE").map_or(d.image, String::from),
            locations: Locations::parse(cfg.get("HETZNER_LOCATION")),
            ssh_keys,
            ssh_keys_from,
            pubkey_file: set("SHARED_SSH_KEY_PATH").map(|p| format!("{p}.pub")),
        }
    }
}

/// Where creates go, from `HETZNER_LOCATION`: the locations named, in their order, and — when
/// `any` — then every other location that sells the server type ([`location_walk`]).
/// - unset, empty or `any`: any location, nearest first;
/// - `nbg1`: only nbg1 (as before this setting took a list);
/// - `nbg1,fsn1`: those two, in that order;
/// - `nbg1,any`: nbg1 first, then any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locations {
    /// Tried first, in this order (lowercased, deduped).
    pub listed: Vec<String>,
    /// Then every other location that sells the type.
    pub any: bool,
}

impl Locations {
    /// Any location: what an unset `HETZNER_LOCATION` means.
    pub const ANY: Self = Self { listed: Vec::new(), any: true };

    /// `HETZNER_LOCATION`'s value (`None` = unset) — commas or spaces between names. Pure.
    pub fn parse(raw: Option<&str>) -> Self {
        let mut listed: Vec<String> = Vec::new();
        let mut any = false;
        for word in raw.unwrap_or("").split(|c: char| c == ',' || c.is_whitespace()).map(str::trim).filter(|w| !w.is_empty()) {
            let word = word.to_ascii_lowercase();
            if word == "any" {
                any = true;
            } else if !listed.contains(&word) {
                listed.push(word);
            }
        }
        Self { any: any || listed.is_empty(), listed }
    }

    /// The one location a create is pinned to, if that's all there is to try.
    pub fn pinned(&self) -> Option<&str> {
        match (self.listed.as_slice(), self.any) {
            ([one], false) => Some(one),
            _ => None,
        }
    }

    /// The first location named, if any — the archive's default (one server and its volumes,
    /// which must share a location, so it can't walk).
    pub fn first(&self) -> Option<&str> {
        self.listed.first().map(String::as_str)
    }

    /// How `config check` and the create preview say it: `any (nearest first, the next when
    /// one has no room)`, `nbg1`, `nbg1, fsn1 (in that order)`, `nbg1, then any`. Pure.
    pub fn describe(&self) -> String {
        match (self.listed.as_slice(), self.any) {
            ([], _) => "any (nearest first, the next when one has no room)".into(),
            ([one], false) => one.clone(),
            (many, false) => format!("{} (in that order)", many.join(", ")),
            (some, true) => format!("{}, then any", some.join(", ")),
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

/// How far each Hetzner location is from here, for the `any` walk ([`location_walk`]): the
/// fastest of [`PROBE_TRIES`] TCP connects to the location's speed-test host
/// (`nbg1-speed.hetzner.com:443`, Hetzner's own; not part of the API). Where arena runs is
/// where the SSH comes from: on prod that's the proxy box every student connection goes
/// through. Live 2026-10-26, from ap and the dev box (both in nbg1): nbg1 0.4 ms, fsn1 2.8,
/// hel1 24, ash 95, sin 159, hil 167 — and the EU prices are one price, so cheapest-first
/// couldn't tell nbg1 from hel1.
///
/// Each location is timed once per process, all at once, and only for a walk with `any` in it.
/// One that doesn't answer is unmeasured and walks after the measured ones (by price). A round
/// where nothing answered (no network, connects blocked) is forgotten, so the next walk tries
/// again.
struct Distances {
    /// Where location `l` is timed (`host:port`); `None`: nothing is (tests).
    host: Option<ProbeHost>,
    /// Each location timed so far (`None`: no answer).
    timed: std::sync::Mutex<HashMap<String, Option<Duration>>>,
}

/// Location → the `host:port` its distance is timed against.
type ProbeHost = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Connects per location; the fastest counts (the first can pay for a route lookup).
const PROBE_TRIES: u32 = 3;

/// How long the name lookup or one connect may take before the location counts as unmeasured:
/// far above any real round trip (Singapore from nbg1: 159 ms).
const PROBE_WAIT: Duration = Duration::from_secs(1);

impl Distances {
    fn timed_from_here() -> Self {
        Self { host: Some(Box::new(|l: &str| format!("{l}-speed.hetzner.com:443"))), timed: Default::default() }
    }

    #[cfg(test)]
    fn known(d: &[(&str, u64)]) -> Self {
        let timed = d.iter().map(|&(l, ms)| (l.to_string(), Some(Duration::from_millis(ms)))).collect();
        Self { host: None, timed: std::sync::Mutex::new(timed) }
    }

    /// The distances to `names` that are known, timing the ones not timed yet.
    async fn to(&self, names: &[&str]) -> HashMap<String, Duration> {
        let timed = || self.timed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(host) = &self.host {
            let untimed: Vec<String> = {
                let t = timed();
                names.iter().filter(|l| !t.contains_key(**l)).map(|l| l.to_string()).collect()
            };
            let mut set = tokio::task::JoinSet::new();
            for l in untimed {
                let addr = host(&l);
                set.spawn(async move { (l, crate::rtt::connect_rtt(addr, PROBE_TRIES, PROBE_WAIT).await) });
            }
            let mut round = Vec::new();
            while let Some(r) = set.join_next().await {
                round.extend(r.ok());
            }
            let mut t = timed();
            if round.iter().any(|(_, d)| d.is_some()) || t.values().any(Option::is_some) {
                t.extend(round);
            }
        }
        timed().iter().filter_map(|(l, d)| d.map(|d| (l.clone(), d))).collect()
    }
}

pub struct HetznerProvider {
    api_key: String,
    client: Client,
    base: String,
    opts: HetznerOpts,
    shutdown: ShutdownWait,
    distances: Distances,
}

impl HetznerProvider {
    pub fn new(api_key: impl Into<String>, opts: HetznerOpts) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
            opts,
            shutdown: SHUTDOWN_WAIT,
            distances: Distances::timed_from_here(),
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
    /// - `GET /server_types`: a type that can't be ordered at `location` (the one create pinned
    ///   there; `None` after a walk of several) → the types that can ([`type_advice`]), tagged
    ///   `Rejected`: placement skips the option instead of waiting for capacity that won't come
    ///   (a "not available in" wording reads as capacity otherwise).
    ///
    /// The API's own answer stays first, and a lookup that fails or finds nothing wrong adds
    /// nothing: this only ever explains the error, never replaces it.
    async fn explain_create_error(&self, e: Error, server_type: &str, location: Option<&str>) -> Error {
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
            self.get_all("server_types").await.ok().and_then(|types| type_advice(&types, server_type, location, now))
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

impl HetznerProvider {
    /// The locations a create of `spec` tries, in order ([`location_walk`]); `None` = no
    /// `location` in the body. Only a walk with `any` in it reads `GET /server_types` and times
    /// the type's locations ([`Distances`]). Without
    /// that answer the listed locations are all there is — or, with none listed, one create
    /// without a location: Hetzner then places it, as it would anyway. An `--api-json`
    /// `datacenter` names the place itself (Hetzner refuses it next to a `location`): one
    /// create, as asked. `Err`: nothing to try (a type Hetzner doesn't have, or sells nowhere).
    async fn walk_for(&self, ty: &str, spec: &PodSpec) -> Result<Vec<Option<String>>> {
        if spec.api_extra.as_ref().is_some_and(|x| x.contains_key("datacenter")) {
            return Ok(vec![None]);
        }
        let locations = &self.opts.locations;
        if !locations.any {
            return Ok(locations.listed.iter().cloned().map(Some).collect());
        }
        match self.get_all("server_types").await {
            Ok(types) => {
                let walk = self.walk_in(&types, ty).await.map_err(|why| Error::Provider { kind: crate::error::ProviderErrorKind::Rejected, message: format!("hetzner create: {why}") })?;
                Ok(walk.into_iter().map(Some).collect())
            }
            Err(_) if !locations.listed.is_empty() => Ok(locations.listed.iter().cloned().map(Some).collect()),
            Err(_) => Ok(vec![None]),
        }
    }

    /// [`location_walk`] for `ty` over the `GET /server_types` items `types`, with `any`'s
    /// locations timed ([`Distances`]). `Err`: what to say when there'd be nothing to try.
    async fn walk_in(&self, types: &[Value], ty: &str) -> std::result::Result<Vec<String>, String> {
        let locations = &self.opts.locations;
        let near = if locations.any {
            self.distances.to(&find_type(types, ty).map(locations_of).unwrap_or_default()).await
        } else {
            HashMap::new()
        };
        location_walk(types, ty, locations, crate::snapshot::unix_now(), &near)
    }

    /// What a create of `spec` would get, read-only, for `arena offers`: the server type it asks
    /// for (`--server-type`, else `HETZNER_SERVER_TYPE`), its size, the locations it would try in
    /// order (the create's own walk, timed the same way) and its gross €/h in the first. One
    /// `GET /server_types`. `Err`: it couldn't be read, or there'd be nothing to create.
    pub async fn offer(&self, spec: &PodSpec) -> Result<ServerType> {
        let ty = server_type(&self.opts, spec);
        let types = self.get_all("server_types").await?;
        let walk = self.walk_in(&types, ty).await.map_err(Error::Config)?;
        server_offer(&types, ty, walk).ok_or_else(|| Error::Config(format!("Hetzner has no server type `{ty}`")))
    }

    /// Every server type on sale somewhere now, cheapest first, for `arena offers --all`: what
    /// `--server-type` takes. One `GET /server_types`.
    pub async fn catalog(&self) -> Result<Vec<ServerType>> {
        Ok(server_types(&self.get_all("server_types").await?, crate::snapshot::unix_now()))
    }
}

/// A Hetzner server type as `arena offers` shows it: what `--server-type` names, its size, and
/// its gross €/h (Hetzner bills in EUR; arena never converts it) in the first of `locations`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ServerType {
    pub name: String,
    pub cores: Option<u32>,
    /// `shared` or `dedicated` vCPUs.
    pub cpu_type: Option<String>,
    /// `x86` or `arm`.
    pub architecture: Option<String>,
    pub memory_gb: Option<f64>,
    pub disk_gb: Option<u32>,
    /// Gross €/h in `locations[0]`; `None` when Hetzner lists no price there.
    pub eur_per_hour: Option<f64>,
    /// A create's walk, in order ([`HetznerProvider::offer`]); in the catalog, every location
    /// that sells it now, cheapest first ([`server_types`]).
    pub locations: Vec<String>,
}

/// `t` (a `GET /server_types` item) as a [`ServerType`] over `locations`. Pure.
fn server_type_of(t: &Value, locations: Vec<String>) -> ServerType {
    let text = |k: &str| t.get(k).and_then(Value::as_str).map(String::from);
    ServerType {
        name: text("name").unwrap_or_else(|| "?".into()),
        cores: t.get("cores").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok()),
        cpu_type: text("cpu_type"),
        architecture: text("architecture"),
        memory_gb: t.get("memory").and_then(Value::as_f64),
        disk_gb: t.get("disk").and_then(Value::as_u64).and_then(|d| u32::try_from(d).ok()),
        eur_per_hour: locations.first().and_then(|l| price_in(t, l)).map(|m| m as f64 / 1_000_000.0),
        locations,
    }
}

/// Server type `ty` as a create walking `walk` gets it; `None` when Hetzner has no such type.
/// Pure.
fn server_offer(types: &[Value], ty: &str, walk: Vec<String>) -> Option<ServerType> {
    find_type(types, ty).map(|t| server_type_of(t, walk))
}

/// Every type orderable somewhere now ([`orderable_in`]), each over the locations that sell
/// it, cheapest first (then the API's order); the types cheapest first, then by name. A
/// location with no price sorts last. Pure.
fn server_types(types: &[Value], now: u64) -> Vec<ServerType> {
    let mut out: Vec<ServerType> = types
        .iter()
        .filter_map(|t| {
            let mut sold: Vec<&str> = locations_of(t).into_iter().filter(|l| orderable_in(t, l, now) == Some(true)).collect();
            sold.sort_by_key(|l| price_in(t, l).unwrap_or(i64::MAX));
            (!sold.is_empty()).then(|| server_type_of(t, sold.into_iter().map(String::from).collect()))
        })
        .collect();
    out.sort_by(|a, b| {
        let eur = |s: &ServerType| s.eur_per_hour.unwrap_or(f64::MAX);
        eur(a).total_cmp(&eur(b)).then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Whether a refused create means "no room for this type here", so the next location may have
/// some: Hetzner's `resource_unavailable` (HTTP 412, "error during placement" — which no
/// capacity wording catches), a `placement_error`, a type or location taken off sale there
/// ("unsupported location for server type", a 422 — live: cpx41 in nbg1), or any error already
/// read as capacity. Never an auth failure, a throttle or a 5xx: those
/// stop the walk (a bad token is bad everywhere; the retry layer owns throttles and blips).
/// The archive walks its server types on the same test.
pub fn is_placement_error(e: &Error) -> bool {
    use crate::error::ProviderErrorKind as K;
    if matches!(e.kind(), Some(K::Auth | K::RateLimited | K::Transient)) {
        return false;
    }
    let m = e.to_string().to_lowercase();
    e.kind() == Some(K::Capacity)
        || ["resource_unavailable", "placement_error", "server_type_unavailable", "server location disabled", "unsupported location"]
            .iter()
            .any(|n| m.contains(n))
}

/// A provider error's own words (without `provider error: `), for a message that quotes several.
fn message_of(e: &Error) -> String {
    match e {
        Error::Provider { message, .. } => message.clone(),
        other => other.to_string(),
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

/// A flag of `t`'s entry for `location` in its per-location list — `available` (live, cx33/cx43/
/// cx53 and three cax types read `false` in nbg1 with no deprecation; a hint of low stock, not a
/// refusal: a cx23 create in nbg1 went through while it read `false` there, live hz2) or
/// `recommended` — or `None` when the answer doesn't carry it (an older one, or no entry there).
/// Pure.
fn location_flag(t: &Value, location: &str, flag: &str) -> Option<bool> {
    let locs = t.get("locations")?.as_array()?;
    locs.iter().find(|l| l.get("name").and_then(Value::as_str) == Some(location))?.get(flag)?.as_bool()
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

/// `label` with `note` added inside its parentheses — `cx33 (4 vCPU, 8 GB, low availability)` —
/// or in new ones when it has none (`cx33 (low availability)`). Pure.
fn tagged(label: &str, note: &str) -> String {
    match label.strip_suffix(')') {
        Some(open) => format!("{open}, {note})"),
        None => format!("{label} ({note})"),
    }
}

/// What to say when server type `ty` can't be ordered in `location` (`types`: the `GET
/// /server_types` items): the types that can — not deprecated there — to pick a `--server-type`
/// from. Hetzner's `recommended` ones first, then the rest, then the ones it marks `available:
/// false` there, tagged `low availability`: last, since one may fail on capacity (live hz F3),
/// but listed, since one may also go through (live hz2: a cx23 in nbg1 did). Each group in the
/// API's order. `None` when `ty` can be ordered there (low on stock or not: that is capacity,
/// which a retry may get), or the answer doesn't say. Without a location only a type Hetzner
/// doesn't have at all is reported. Pure.
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
    let mut sold: Vec<&Value> = types.iter().filter(|t| orderable_in(t, loc, now) == Some(true) && !deprecated_in(t, loc)).collect();
    let low = |t: &Value| location_flag(t, loc, "available") == Some(false);
    sold.sort_by_key(|t| (low(t), location_flag(t, loc, "recommended") != Some(true)));
    let offered: Vec<String> = sold.iter().map(|t| if low(t) { tagged(&type_label(t), "low availability") } else { type_label(t) }).collect();
    Some(if sold.is_empty() {
        format!("{why}, and Hetzner lists no type there (`arena api get hetzner server_types`) — pick another HETZNER_LOCATION (unset: any that sells it)")
    } else {
        format!(
            "{why}; available there: {} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another \
             HETZNER_LOCATION (unset: any that sells it)",
            offered.join(", ")
        )
    })
}

/// `t`'s gross hourly price in `location`, in millionths (prices are strings, `"0.0104000000"`),
/// or `None` when it lists none there. Pure.
fn price_in(t: &Value, location: &str) -> Option<i64> {
    let p = t.get("prices")?.as_array()?.iter().find(|p| p.get("location").and_then(Value::as_str) == Some(location))?;
    let hourly = p.get("price_hourly")?;
    let v: f64 = hourly.get("gross").or_else(|| hourly.get("net"))?.as_str()?.parse().ok()?;
    v.is_finite().then(|| (v * 1_000_000.0).round() as i64)
}

/// Server type `ty` in the `GET /server_types` items, by name or id. Pure.
fn find_type<'a>(types: &'a [Value], ty: &str) -> Option<&'a Value> {
    types.iter().find(|t| {
        t.get("name").and_then(Value::as_str) == Some(ty) || t.get("id").and_then(Value::as_u64).is_some_and(|i| i.to_string() == ty)
    })
}

/// `t`'s locations as the answer lists them: the per-location entries, else (an older answer)
/// the prices'. Pure.
fn locations_of(t: &Value) -> Vec<&str> {
    match t.get("locations").and_then(Value::as_array) {
        Some(locs) => locs.iter().filter_map(|l| l.get("name").and_then(Value::as_str)).collect(),
        None => t.get("prices").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("location").and_then(Value::as_str)).collect(),
    }
}

/// The locations a create of server type `ty` tries, in order (`types`: the `GET /server_types`
/// items): the ones `locations` lists first, as listed; then, with `any`, every other location
/// that sells `ty` now ([`orderable_in`]) — nearest first (`near`, [`Distances`]; unmeasured
/// ones after), then cheapest, then the ones Hetzner doesn't mark low on stock (`available:
/// false` still goes through at times: live hz2), then the ones not announced as retiring, then
/// its `recommended` ones, then the API's order. A location that doesn't sell `ty` is never
/// added; a listed one is tried regardless (the operator asked). `Err`: what to say when
/// there'd be nothing to try. Pure.
fn location_walk(
    types: &[Value],
    ty: &str,
    locations: &Locations,
    now: u64,
    near: &HashMap<String, Duration>,
) -> std::result::Result<Vec<String>, String> {
    let mut walk = locations.listed.clone();
    if !locations.any {
        return Ok(walk);
    }
    let Some(t) = find_type(types, ty) else {
        return if walk.is_empty() {
            Err(format!("Hetzner has no server type `{ty}` (`arena api get hetzner server_types` lists them)"))
        } else {
            Ok(walk)
        };
    };
    let mut sold: Vec<(usize, &str)> = locations_of(t).into_iter().enumerate().filter(|(_, l)| orderable_in(t, l, now) == Some(true)).collect();
    sold.sort_by_key(|&(i, l)| {
        (
            near.get(l).copied().unwrap_or(Duration::MAX),
            price_in(t, l).unwrap_or(i64::MAX),
            location_flag(t, l, "available") == Some(false),
            deprecated_in(t, l),
            location_flag(t, l, "recommended") != Some(true),
            i,
        )
    });
    for (_, l) in sold {
        if !walk.iter().any(|w| w == l) {
            walk.push(l.to_string());
        }
    }
    if walk.is_empty() {
        Err(format!("server type `{ty}` can't be ordered in any Hetzner location right now (`arena api get hetzner server_types` lists them)"))
    } else {
        Ok(walk)
    }
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
/// deep-merged in last, minus [`MANAGED`]. `location`: this attempt's ([`location_walk`]);
/// `None` sends none.
fn create_payload(opts: &HetznerOpts, spec: &PodSpec, location: Option<&str>) -> Result<Value> {
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
    if let Some(loc) = location {
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
        // currency-agnostic, so it sits alongside runpod's USD as a plain cost/hr. The
        // location is the server's own `location` (live 2026-10-26: no `datacenter` any more),
        // or an older answer's `datacenter.location` — a VM placed in sin bills sin's price.
        cost_per_hr: v.get("server_type").and_then(|t| t.get("prices")).and_then(Value::as_array).and_then(
            |prices| {
                let loc = v
                    .pointer("/location/name")
                    .or_else(|| v.pointer("/datacenter/location/name"))
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
        started_at: None,
        identity: None,
        status_detail: None,
        // `nbg1 (Nuremberg, DE)`: the location's name, with its city and country when given.
        location: hetzner_location(v),
    }
}

/// A server's location for people: its `location` (or an older answer's
/// `datacenter.location`) as `nbg1 (Nuremberg, DE)`, just `nbg1` without a city. Pure.
fn hetzner_location(v: &Value) -> Option<String> {
    let loc = v.get("location").filter(|l| l.is_object()).or_else(|| v.pointer("/datacenter/location"))?;
    let field = |k: &str| loc.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let name = field("name")?;
    Some(match (field("city"), field("country")) {
        (Some(city), Some(cc)) => format!("{name} ({city}, {cc})"),
        (Some(city), None) => format!("{name} ({city})"),
        _ => name.to_string(),
    })
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
        format!("{} CPU VM, image {}, location {}", server_type(&self.opts, spec), self.opts.image, self.opts.locations.describe())
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

    /// One create per location of the walk ([`HetznerProvider::walk_for`]), until one takes it:
    /// a location with no room ([`is_placement_error`]) moves on to the next; any other refusal
    /// stops, explained. When no location had room the error is `Capacity` (so `--keep-trying`
    /// waits for some) and says what each answered. A refused create makes no server, so the
    /// name is still free for the next location.
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        use crate::error::ProviderErrorKind as K;
        let ty = server_type(&self.opts, spec);
        let walk = self.walk_for(ty, spec).await?;
        let url = format!("{}/servers", self.base);
        let mut no_room: Vec<String> = Vec::new();
        for (i, loc) in walk.iter().enumerate() {
            let payload = create_payload(&self.opts, spec, loc.as_deref())?;
            let e = match send_json(self.auth(self.client.post(&url).json(&payload)), "hetzner create").await {
                // The created server is under `server`; parse what's there (IP may not be
                // populated until it finishes provisioning — `pods up` polls for that).
                Ok(body) => return Ok(parse_server(body.get("server").unwrap_or(&body))),
                Err(e) => e,
            };
            let full = is_placement_error(&e);
            let here = loc.as_deref().unwrap_or("Hetzner's choice");
            if full && i + 1 < walk.len() {
                no_room.push(format!("{here}: {}", message_of(&e)));
                continue;
            }
            let e = match (full, no_room.is_empty()) {
                (true, true) => Error::Provider { kind: K::Capacity, message: message_of(&e) },
                (true, false) => {
                    no_room.push(format!("{here}: {}", message_of(&e)));
                    let tried: Vec<&str> = walk.iter().map(|l| l.as_deref().unwrap_or("Hetzner's choice")).collect();
                    Error::Provider {
                        kind: K::Capacity,
                        message: format!("hetzner create: no room for {ty} in {} — {}", tried.join(", "), no_room.join("; ")),
                    }
                }
                (false, true) => e,
                (false, false) => match e {
                    Error::Provider { kind, message } => {
                        Error::Provider { kind, message: format!("{message} (in {here}, after no room in: {})", no_room.join("; ")) }
                    }
                    other => other,
                },
            };
            return Err(self.explain_create_error(e, ty, if walk.len() == 1 { loc.as_deref() } else { None }).await);
        }
        Err(Error::provider("hetzner create: no location to try"))
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

    /// The body of the walk's first create when that needs no lookup: the first listed
    /// location (or none, for `any` alone — `describe` says where it would go).
    fn preview_create_body(&self, spec: &PodSpec) -> Result<Value> {
        create_payload(&self.opts, spec, self.opts.locations.first())
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
            distances: Distances::known(&[]),
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
            distances: Distances::known(&[]),
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
        let p = HetznerProvider { api_key: "k".into(), client: client(), base: srv.base.clone(), opts: HetznerOpts::default(), shutdown: SHUTDOWN_WAIT, distances: Distances::known(&[]) };
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
            let body = create_payload(&opts, &spec("devtest-a", flag), None).unwrap();
            assert_eq!(body["server_type"], want, "{flag:?}");
            assert_eq!(p.preview_create_body(&spec("devtest-a", flag)).unwrap()["server_type"], want);
            assert!(p.describe(&spec("devtest-a", flag)).starts_with(&format!("{want} CPU VM")), "{flag:?}");
        }
        assert_eq!(create_payload(&HetznerOpts::default(), &spec("x", None), None).unwrap()["server_type"], "cx23");
    }

    /// The options as `build` reads them: the default key named after the prefix — and that
    /// it was the default, for the missing-key advice — or HETZNER_SSH_KEY's; the cohort key's
    /// `.pub` to upload. Unset keys keep their defaults — any location, since that setting was
    /// added (before, nbg1).
    #[test]
    fn options_from_config_say_where_the_key_name_came_from() {
        let o = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nSHARED_SSH_KEY_PATH=/k/arena9_key\n"));
        assert_eq!((o.ssh_keys.as_slice(), o.ssh_keys_from), (&["arena9".to_string()][..], "MACHINE_NAME_PREFIX"));
        assert_eq!(o.pubkey_file.as_deref(), Some("/k/arena9_key.pub"));
        assert_eq!((o.server_type.as_str(), o.image.as_str(), &o.locations), ("cx23", "ubuntu-24.04", &Locations::ANY));
        let o = HetznerOpts::from_config(&crate::config::Config::parse(
            "MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=arena8\nHETZNER_LOCATION=fsn1\nHETZNER_SERVER_TYPE=cpx42\n",
        ));
        assert_eq!((o.ssh_keys.as_slice(), o.ssh_keys_from), (&["arena8".to_string()][..], "HETZNER_SSH_KEY"));
        assert_eq!((o.server_type.as_str(), o.locations.pinned(), o.pubkey_file), ("cpx42", Some("fsn1"), None));
        // An empty HETZNER_SSH_KEY is unset, as before: the prefix's key.
        let o = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=\n"));
        assert_eq!((o.ssh_keys[0].as_str(), o.ssh_keys_from), ("arena9", "MACHINE_NAME_PREFIX"));
    }

    /// `GET /server_types` as Hetzner answers it (live, 2026-10-18: every key, per type and per
    /// location), trimmed to seven types and the locations the tests ask about. Per location:
    /// `deprecation` (`unavailable_after` passed = retired there), `available` (`false` with no
    /// deprecation = low on stock there right now) and `recommended`. As live: cpx41 is retired
    /// everywhere but ash (`prices` still lists the rest); cx33 and cx23 are low on stock in fsn1/hel1,
    /// cx33 in nbg1 too; cpx42 and cax11 are recommended in hel1. Changed for the tests: cpx42 is
    /// recommended in nbg1 too (live: none there is), cpx22's nbg1 retirement is announced (still
    /// orderable, never suggested), and ccx13 keeps only its ash entry (a US-only type).
    const SERVER_TYPES: &str = r#"{"server_types": [
        {"id": 114, "name": "cx23", "description": "CX 23", "category": "cost_optimized", "cores": 2, "memory": 4, "disk": 40, "cpu_type": "shared",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.0104000000", "gross": "0.0104000000000000"}, "price_monthly": {"net": "6.4900000000", "gross": "6.4900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.0104000000", "gross": "0.0104000000000000"}, "price_monthly": {"net": "6.4900000000", "gross": "6.4900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "hel1", "price_hourly": {"net": "0.0104000000", "gross": "0.0104000000000000"}, "price_monthly": {"net": "6.4900000000", "gross": "6.4900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null, "available": false, "recommended": false},
                       {"id": 2, "name": "nbg1", "deprecation": null, "available": true, "recommended": false},
                       {"id": 3, "name": "hel1", "deprecation": null, "available": false, "recommended": false}]},
        {"id": 25, "name": "cpx41", "description": "CPX 41", "category": "regular_purpose", "cores": 8, "memory": 16, "disk": 240, "cpu_type": "shared",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.2267000000", "gross": "0.2267000000000000"}, "price_monthly": {"net": "141.4900000000", "gross": "141.4900000000000000"}, "included_traffic": 4398046511104, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.2267000000", "gross": "0.2267000000000000"}, "price_monthly": {"net": "141.4900000000", "gross": "141.4900000000000000"}, "included_traffic": 4398046511104, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "hel1", "price_hourly": {"net": "0.2267000000", "gross": "0.2267000000000000"}, "price_monthly": {"net": "141.4900000000", "gross": "141.4900000000000000"}, "included_traffic": 4398046511104, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "ash", "price_hourly": {"net": "0.2267000000", "gross": "0.2267000000000000"}, "price_monthly": {"net": "141.4900000000", "gross": "141.4900000000000000"}, "included_traffic": 4398046511104, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": {"announced": "2025-10-16T06:00:00Z", "unavailable_after": "2025-12-31T23:59:59Z"}, "available": false, "recommended": false},
                       {"id": 2, "name": "nbg1", "deprecation": {"announced": "2025-10-16T06:00:00Z", "unavailable_after": "2025-12-31T23:59:59Z"}, "available": false, "recommended": false},
                       {"id": 3, "name": "hel1", "deprecation": {"announced": "2025-10-16T06:00:00Z", "unavailable_after": "2025-12-31T23:59:59Z"}, "available": false, "recommended": false},
                       {"id": 4, "name": "ash", "deprecation": null, "available": true, "recommended": false}]},
        {"id": 111, "name": "cpx42", "description": "CPX 42", "category": "regular_purpose", "cores": 8, "memory": 16, "disk": 320, "cpu_type": "shared",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.1314000000", "gross": "0.1314000000000000"}, "price_monthly": {"net": "81.9900000000", "gross": "81.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.1314000000", "gross": "0.1314000000000000"}, "price_monthly": {"net": "81.9900000000", "gross": "81.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "hel1", "price_hourly": {"net": "0.1314000000", "gross": "0.1314000000000000"}, "price_monthly": {"net": "81.9900000000", "gross": "81.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null, "available": true, "recommended": false},
                       {"id": 2, "name": "nbg1", "deprecation": null, "available": true, "recommended": true},
                       {"id": 3, "name": "hel1", "deprecation": null, "available": true, "recommended": true}]},
        {"id": 109, "name": "cpx22", "description": "CPX 22", "category": "regular_purpose", "cores": 2, "memory": 4, "disk": 80, "cpu_type": "shared",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "nbg1", "price_hourly": {"net": "0.0368000000", "gross": "0.0368000000000000"}, "price_monthly": {"net": "22.9900000000", "gross": "22.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 2, "name": "nbg1", "deprecation": {"announced": "2026-09-01T00:00:00Z", "unavailable_after": "2099-01-01T00:00:00Z"}, "available": true, "recommended": false}]},
        {"id": 45, "name": "cax11", "description": "CAX11", "category": "cost_optimized", "cores": 2, "memory": 4, "disk": 40, "cpu_type": "shared",
         "storage_type": "local", "architecture": "arm", "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.0112000000", "gross": "0.0112000000000000"}, "price_monthly": {"net": "6.9900000000", "gross": "6.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.0112000000", "gross": "0.0112000000000000"}, "price_monthly": {"net": "6.9900000000", "gross": "6.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "hel1", "price_hourly": {"net": "0.0112000000", "gross": "0.0112000000000000"}, "price_monthly": {"net": "6.9900000000", "gross": "6.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null, "available": false, "recommended": false},
                       {"id": 2, "name": "nbg1", "deprecation": null, "available": true, "recommended": false},
                       {"id": 3, "name": "hel1", "deprecation": null, "available": true, "recommended": true}]},
        {"id": 96, "name": "ccx13", "description": "CCX13 Dedicated CPU", "category": "general_purpose", "cores": 2, "memory": 8, "disk": 80, "cpu_type": "dedicated",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "ash", "price_hourly": {"net": "0.0817000000", "gross": "0.0817000000000000"}, "price_monthly": {"net": "50.9900000000", "gross": "50.9900000000000000"}, "included_traffic": 1099511627776, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 4, "name": "ash", "deprecation": null, "available": true, "recommended": false}]},
        {"id": 115, "name": "cx33", "description": "CX 33", "category": "cost_optimized", "cores": 4, "memory": 8, "disk": 80, "cpu_type": "shared",
         "storage_type": "local", "architecture": "x86", "deprecated": false, "deprecation": null,
         "prices": [{"location": "fsn1", "price_hourly": {"net": "0.0160000000", "gross": "0.0160000000000000"}, "price_monthly": {"net": "9.9900000000", "gross": "9.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "nbg1", "price_hourly": {"net": "0.0160000000", "gross": "0.0160000000000000"}, "price_monthly": {"net": "9.9900000000", "gross": "9.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}},
                    {"location": "hel1", "price_hourly": {"net": "0.0160000000", "gross": "0.0160000000000000"}, "price_monthly": {"net": "9.9900000000", "gross": "9.9900000000000000"}, "included_traffic": 21990232555520, "price_per_tb_traffic": {"net": "1.2000000000", "gross": "1.2000000000000000"}}],
         "locations": [{"id": 1, "name": "fsn1", "deprecation": null, "available": false, "recommended": false},
                       {"id": 2, "name": "nbg1", "deprecation": null, "available": false, "recommended": false},
                       {"id": 3, "name": "hel1", "deprecation": null, "available": false, "recommended": false}]}
    ], "meta": {"pagination": {"page": 1, "per_page": 50, "previous_page": null, "next_page": null, "last_page": 1, "total_entries": 7}}}"#;

    /// `GET /ssh_keys` as the project answers it (key material included — never quoted back).
    const SSH_KEYS: &str = r#"{"ssh_keys": [
        {"id": 2323, "name": "arena8", "fingerprint": "b7:2f:30", "public_key": "ssh-ed25519 AAAAC3NzaArena8 arena8", "labels": {}},
        {"id": 2324, "name": "ops", "fingerprint": "c1:00:aa", "public_key": "ssh-ed25519 AAAAC3NzaOps ops", "labels": {}}
    ], "meta": {"pagination": {"page": 1, "next_page": null}}}"#;

    fn provider(base: &str, opts: HetznerOpts) -> HetznerProvider {
        use crate::http::test_server::client;
        HetznerProvider { api_key: "k".into(), client: client(), base: base.to_string(), opts, shutdown: SHUTDOWN_WAIT, distances: Distances::known(&[]) }
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
        let opts = HetznerOpts::from_config(&crate::config::Config::parse(
            "MACHINE_NAME_PREFIX=arena9\nSHARED_SSH_KEY_PATH=/k/arena9_key\nHETZNER_LOCATION=nbg1\n",
        ));
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
    /// an `invalid_input` (live), or one that reads like capacity (`not available in`) — the error
    /// lists the types nbg1 sells (not the retired, the announced-retiring or the US-only), the
    /// recommended one first and the low-on-stock cx33 last, tagged (live hz F3, hz2), and how to pick one,
    /// and is `Rejected`: never retried as if capacity might come back. The `--server-type`
    /// reached the request body.
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
            let opts = HetznerOpts::from_config(&crate::config::Config::parse(
                "MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=arena8\nHETZNER_LOCATION=nbg1\n",
            ));
            let e = provider(&srv.base, opts).create_pod(&spec("arena9-evals", Some("cpx41"))).await.unwrap_err();
            let m = e.to_string();
            assert!(m.contains(&format!("hetzner create HTTP {status}")), "{m}");
            assert!(
                m.contains(
                    "server type `cpx41` can't be ordered in nbg1; available there: cpx42 (8 vCPU, 16 GB), cx23 (2 vCPU, 4 GB), \
                     cax11 (2 vCPU, 4 GB, arm), cx33 (4 vCPU, 8 GB, low availability) — pass --server-type <one of those> (or set \
                     HETZNER_SERVER_TYPE)"
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
        let opts = || HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_LOCATION=nbg1\n"));
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
            (t("cpx41"), "ash", now, Some(true)),
            (t("cx33"), "nbg1", now, Some(true)), // low on stock right now: still sold there (capacity)
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
        assert!(deprecated_in(&t("cpx22"), "nbg1") && !deprecated_in(&t("cpx42"), "nbg1") && !deprecated_in(&t("cpx41"), "ash"));
        assert!(deprecated_in(&json!({"name": "cx11", "prices": [], "deprecated": true}), "nbg1"));
        // The live flags, per location; an answer without them (or no entry there) says nothing.
        let flags = |t: &Value, loc: &str| (location_flag(t, loc, "available"), location_flag(t, loc, "recommended"));
        assert_eq!(flags(&t("cx33"), "nbg1"), (Some(false), Some(false)));
        assert_eq!(flags(&t("cpx42"), "hel1"), (Some(true), Some(true)));
        assert_eq!(flags(&t("cx23"), "ash"), (None, None));
        assert_eq!(flags(&json!({"name": "cx22", "locations": [{"name": "nbg1", "deprecation": null}]}), "nbg1"), (None, None));
    }

    /// When to list the types, and which: sold there and not retiring; Hetzner's recommended ones
    /// first, the ones it marks `available: false` there last, tagged `low availability`. Pure.
    #[test]
    fn type_advice_lists_the_types_on_sale_there() {
        let types: Vec<Value> = serde_json::from_str::<Value>(SERVER_TYPES).unwrap()["server_types"].as_array().unwrap().clone();
        let now = u64::try_from(crate::reap::parse_rfc3339("2026-10-16T00:00:00+00:00").unwrap()).unwrap();
        let sold = "cpx42 (8 vCPU, 16 GB), cx23 (2 vCPU, 4 GB), cax11 (2 vCPU, 4 GB, arm), cx33 (4 vCPU, 8 GB, low availability)";
        let cases: &[(&str, Option<&str>, Option<String>)] = &[
            ("cx23", Some("nbg1"), None),
            ("cpx22", Some("nbg1"), None), // retiring, still orderable: not this create's problem
            ("cx33", Some("nbg1"), None),  // low on stock there right now: capacity, which a retry may get
            ("cx23", Some("hel1"), None),
            ("114", Some("nbg1"), None),   // by id
            ("cpx41", Some("nbg1"), Some(format!("server type `cpx41` can't be ordered in nbg1; available there: {sold} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION (unset: any that sells it)"))),
            ("cpx99", Some("nbg1"), Some(format!("Hetzner has no server type `cpx99`; available there: {sold} — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION (unset: any that sells it)"))),
            ("cx23", Some("ash"), Some("server type `cx23` can't be ordered in ash; available there: cpx41 (8 vCPU, 16 GB), ccx13 (2 vCPU, 8 GB) — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION (unset: any that sells it)".into())),
            // hel1: both free ones are recommended (live), and come first; the low-on-stock cx23 and cx33 last, tagged.
            ("cpx41", Some("hel1"), Some("server type `cpx41` can't be ordered in hel1; available there: cpx42 (8 vCPU, 16 GB), cax11 (2 vCPU, 4 GB, arm), cx23 (2 vCPU, 4 GB, low availability), cx33 (4 vCPU, 8 GB, low availability) — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION (unset: any that sells it)".into())),
            ("cx23", Some("hil"), Some("server type `cx23` can't be ordered in hil, and Hetzner lists no type there (`arena api get hetzner server_types`) — pick another HETZNER_LOCATION (unset: any that sells it)".into())),
            ("cpx41", None, None),
            ("cpx99", None, Some("Hetzner has no server type `cpx99` (`arena api get hetzner server_types` lists them)".into())),
        ];
        for (ty, loc, want) in cases {
            assert_eq!(type_advice(&types, ty, *loc, now), want.clone(), "{ty} in {loc:?}");
        }
        // An answer that doesn't say (no locations, no prices): nothing claimed.
        assert_eq!(type_advice(&[json!({"name": "cx23"})], "cx23", Some("nbg1"), now), None);
        // Every type sold there low on stock: still listed (one may go through), each tagged — a
        // bare name too.
        let low = [
            json!({"name": "cx33", "locations": [{"name": "nbg1", "deprecation": null, "available": false, "recommended": false}]}),
            json!({"name": "cx23", "cores": 2, "memory": 4, "locations": [{"name": "nbg1", "deprecation": null, "available": false, "recommended": true}]}),
        ];
        assert_eq!(
            type_advice(&low, "cpx41", Some("nbg1"), now).as_deref(),
            Some(
                "Hetzner has no server type `cpx41`; available there: cx23 (2 vCPU, 4 GB, low availability), cx33 (low availability) \
                 — pass --server-type <one of those> (or set HETZNER_SERVER_TYPE), or pick another HETZNER_LOCATION (unset: any that sells it)"
            )
        );
        // Low on stock outranks recommended and the API's order: a type that's available goes first.
        let mixed = [
            json!({"name": "cx23", "locations": [{"name": "nbg1", "deprecation": null, "available": false, "recommended": true}]}),
            json!({"name": "cax11", "locations": [{"name": "nbg1", "deprecation": null, "available": true, "recommended": false}]}),
        ];
        assert!(
            type_advice(&mixed, "cpx41", Some("nbg1"), now).unwrap().contains("available there: cax11, cx23 (low availability) — "),
            "{:?}",
            type_advice(&mixed, "cpx41", Some("nbg1"), now)
        );
        // An older answer without the flags: offered as before.
        let unflagged = [json!({"name": "cx22", "cores": 2, "memory": 4, "locations": [{"name": "nbg1", "deprecation": null}]})];
        assert!(type_advice(&unflagged, "cpx41", Some("nbg1"), now).unwrap().contains("available there: cx22 (2 vCPU, 4 GB) — "));
    }

    /// `HETZNER_LOCATION`: unset, empty or `any` is any location; one name pins it; a list orders
    /// the walk (commas or spaces, case and repeats ignored); `any` in a list adds the rest.
    #[test]
    fn hetzner_location_reads_as_any_one_or_an_ordered_list() {
        let l = |raw: Option<&str>| Locations::parse(raw);
        for raw in [None, Some(""), Some("  "), Some("any"), Some("ANY"), Some(",")] {
            assert_eq!(l(raw), Locations::ANY, "{raw:?}");
        }
        assert_eq!(l(Some("nbg1")).pinned(), Some("nbg1"));
        assert_eq!(l(Some(" NBG1, fsn1 nbg1 ")), Locations { listed: vec!["nbg1".into(), "fsn1".into()], any: false });
        assert_eq!(l(Some("hel1,any")), Locations { listed: vec!["hel1".into()], any: true });
        assert_eq!(l(Some("hel1,any")).pinned(), None);
        assert_eq!((l(None).first(), l(Some("fsn1,nbg1")).first()), (None, Some("fsn1")));
        let said: Vec<String> = [None, Some("nbg1"), Some("nbg1,fsn1"), Some("nbg1,any")].iter().map(|r| l(*r).describe()).collect();
        assert_eq!(said, ["any (nearest first, the next when one has no room)", "nbg1", "nbg1, fsn1 (in that order)", "nbg1, then any"]);
    }

    /// The walk for `any`, from the recorded `GET /server_types`, with nothing timed (so by
    /// price): every location that sells the type now — cheapest, then not low on stock, then
    /// not retiring, then recommended, then the API's order — after the listed ones. Retired and unlisted locations never join; a
    /// type Hetzner doesn't have, or sells nowhere now, is said instead. Pure.
    #[test]
    fn the_location_walk_takes_every_location_that_sells_the_type() {
        let types: Vec<Value> = serde_json::from_str::<Value>(SERVER_TYPES).unwrap()["server_types"].as_array().unwrap().clone();
        let now = 1_792_000_000; // 2026-10: cpx41 retired in the EU DCs, cpx22's retirement in nbg1 only announced.
        let walk = |ty: &str, raw: Option<&str>| location_walk(&types, ty, &Locations::parse(raw), now, &HashMap::new());
        // cx23: one price; nbg1 isn't low on stock, fsn1 and hel1 are (API order between them).
        assert_eq!(walk("cx23", None).unwrap(), ["nbg1", "fsn1", "hel1"]);
        assert_eq!(walk("114", None).unwrap(), ["nbg1", "fsn1", "hel1"], "by id too");
        // cpx42: all in stock, one price; Hetzner's recommended ones first.
        assert_eq!(walk("cpx42", None).unwrap(), ["nbg1", "hel1", "fsn1"]);
        // Retired everywhere but ash; sold only in ash; a retirement only announced still sells.
        assert_eq!(walk("cpx41", None).unwrap(), ["ash"]);
        assert_eq!(walk("ccx13", None).unwrap(), ["ash"]);
        assert_eq!(walk("cpx22", None).unwrap(), ["nbg1"]);
        // Listed first, as listed (even where it isn't sold — the operator asked), then the rest.
        assert_eq!(walk("cx23", Some("hel1,any")).unwrap(), ["hel1", "nbg1", "fsn1"]);
        assert_eq!(walk("cx23", Some("ash, hel1")).unwrap(), ["ash", "hel1"]);
        assert_eq!(walk("cpx99", Some("fsn1,any")).unwrap(), ["fsn1"]);
        let e = walk("cpx99", None).unwrap_err();
        assert!(e.contains("Hetzner has no server type `cpx99`"), "{e}");
        let after = 4_100_000_000; // 2099: cpx22 retired in nbg1, its only location.
        let e = location_walk(&types, "cpx22", &Locations::ANY, after, &HashMap::new()).unwrap_err();
        assert!(e.contains("server type `cpx22` can't be ordered in any Hetzner location right now"), "{e}");

        // Price first (a cheaper location wins over stock and API order); a location with no
        // price last; an older answer without per-location entries is read from its prices.
        let priced = json!([{"name": "t1",
            "prices": [{"location": "fsn1", "price_hourly": {"gross": "0.0200"}}, {"location": "sin", "price_hourly": {"gross": "0.0100"}},
                       {"location": "hel1", "price_hourly": {"net": "0.0150"}}],
            "locations": [{"name": "fsn1", "deprecation": null, "available": true}, {"name": "nbg1", "deprecation": null, "available": true},
                          {"name": "sin", "deprecation": null, "available": false}, {"name": "hel1", "deprecation": null, "available": true}]},
            {"name": "old", "deprecation": null,
             "prices": [{"location": "hel1", "price_hourly": {"gross": "0.0300"}}, {"location": "fsn1", "price_hourly": {"gross": "0.0100"}}]}]);
        let types = priced.as_array().unwrap();
        assert_eq!(location_walk(types, "t1", &Locations::ANY, now, &HashMap::new()).unwrap(), ["sin", "hel1", "fsn1", "nbg1"]);
        assert_eq!(location_walk(types, "old", &Locations::ANY, now, &HashMap::new()).unwrap(), ["fsn1", "hel1"]);
    }

    /// With distances, `any` walks nearest first: price only between equally near locations, the
    /// unmeasured ones last (by price among themselves), and listed ones still first. Pure.
    #[test]
    fn the_nearest_location_goes_first_and_price_only_breaks_ties() {
        let ms = |d: &[(&str, u64)]| d.iter().map(|&(l, ms)| (l.to_string(), Duration::from_millis(ms))).collect::<HashMap<_, _>>();
        let types: Vec<Value> = serde_json::from_str::<Value>(SERVER_TYPES).unwrap()["server_types"].as_array().unwrap().clone();
        let now = 1_792_000_000;
        // cx23 by price alone is nbg1, fsn1, hel1 (nbg1 isn't low on stock): from Helsinki, hel1.
        let walk = location_walk(&types, "cx23", &Locations::ANY, now, &ms(&[("hel1", 1), ("nbg1", 24), ("fsn1", 26)]));
        assert_eq!(walk.unwrap(), ["hel1", "nbg1", "fsn1"]);
        let walk = location_walk(&types, "cx23", &Locations::ANY, now, &ms(&[("fsn1", 1), ("hel1", 24)]));
        assert_eq!(walk.unwrap(), ["fsn1", "hel1", "nbg1"], "nbg1 wasn't timed: last");

        // t1: sin is cheapest but furthest; fsn1 and nbg1 are as near, and fsn1 has a price.
        let priced = json!([{"name": "t1",
            "prices": [{"location": "fsn1", "price_hourly": {"gross": "0.0200"}}, {"location": "sin", "price_hourly": {"gross": "0.0100"}},
                       {"location": "hel1", "price_hourly": {"net": "0.0150"}}],
            "locations": [{"name": "nbg1", "deprecation": null, "available": true}, {"name": "sin", "deprecation": null, "available": true},
                          {"name": "fsn1", "deprecation": null, "available": true}, {"name": "hel1", "deprecation": null, "available": true}]}]);
        let types = priced.as_array().unwrap();
        let near = ms(&[("fsn1", 3), ("nbg1", 3), ("hel1", 24), ("sin", 159)]);
        assert_eq!(location_walk(types, "t1", &Locations::ANY, now, &near).unwrap(), ["fsn1", "nbg1", "hel1", "sin"]);
        assert_eq!(location_walk(types, "t1", &Locations::parse(Some("sin,any")), now, &near).unwrap(), ["sin", "fsn1", "nbg1", "hel1"]);
        assert_eq!(location_walk(types, "t1", &Locations::parse(Some("sin,hel1")), now, &near).unwrap(), ["sin", "hel1"], "a list isn't reordered");
    }

    /// Over the real request path: the walk follows the distances, not the catalog's order.
    #[tokio::test]
    async fn a_create_tries_the_nearest_location_first() {
        use crate::http::test_server::{canned, serve};
        let full = r#"{"error":{"code":"resource_unavailable","message":"error during placement","details":{}}}"#;
        let srv = serve(vec![
            canned(200, "application/json", SERVER_TYPES),
            canned(412, "application/json", full),
            canned(201, "application/json", &server_created("devtest-a", "fsn1")),
        ]);
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\n"));
        let mut p = provider(&srv.base, opts);
        p.distances = Distances::known(&[("hel1", 1), ("fsn1", 24), ("nbg1", 26)]);
        p.create_pod(&spec("devtest-a", None)).await.unwrap();
        assert_eq!(sent_locations(&srv), [Some("hel1".into()), Some("fsn1".into())]);
    }

    /// Timing: each location once per process, all at once; one that doesn't answer is
    /// unmeasured (and not timed again while others answered); a round where nothing answered
    /// is forgotten, so the next walk tries again. Loopback ports stand in for the speed hosts.
    #[tokio::test]
    async fn locations_are_timed_once_and_one_that_doesnt_answer_walks_unmeasured() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let open_port = open.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));
        let timing = |answers: bool| {
            let asked = asked.clone();
            Distances {
                host: Some(Box::new(move |l: &str| {
                    asked.fetch_add(1, Ordering::SeqCst);
                    format!("127.0.0.1:{}", if answers && l == "near" { open_port } else { closed })
                })),
                timed: Default::default(),
            }
        };
        let d = timing(true);
        let got = d.to(&["near", "gone"]).await;
        assert_eq!(got.keys().collect::<Vec<_>>(), ["near"]);
        assert!(got["near"] < PROBE_WAIT, "{got:?}");
        assert_eq!(asked.load(Ordering::SeqCst), 2, "both timed, once each");
        assert_eq!(d.to(&["gone", "near"]).await.len(), 1);
        assert_eq!(asked.load(Ordering::SeqCst), 2, "neither timed again");

        asked.store(0, Ordering::SeqCst);
        let silent = timing(false);
        assert!(silent.to(&["near", "gone"]).await.is_empty());
        assert!(silent.to(&["near"]).await.is_empty());
        assert_eq!(asked.load(Ordering::SeqCst), 3, "a round where nothing answered isn't kept");
        drop(open);
    }

    fn server_created(name: &str, location: &str) -> String {
        json!({"server": {"id": 77, "name": name, "status": "initializing", "public_net": {"ipv4": {"ip": "203.0.113.9"}},
            "server_type": {"name": "cx23"}, "location": {"name": location}}})
        .to_string()
    }

    /// The location of each create, as sent (`None`: no `location` in the body).
    fn sent_locations(srv: &crate::http::test_server::Server) -> Vec<Option<String>> {
        let (requests, bodies) = (srv.requests.lock().unwrap().clone(), srv.bodies.lock().unwrap().clone());
        requests.iter().zip(bodies).filter(|(r, _)| r.starts_with("POST /servers ")).map(|(_, b)| {
            serde_json::from_str::<Value>(&b).unwrap().get("location").and_then(Value::as_str).map(String::from)
        }).collect()
    }

    /// The default walk, over the real request path: the catalog once, then one create per
    /// location until one has room. Hetzner's "no room" (`resource_unavailable`, "error during
    /// placement" — no capacity wording) moves on; the same name goes to the next location.
    #[tokio::test]
    async fn a_location_with_no_room_moves_the_create_to_the_next() {
        use crate::http::test_server::{canned, serve};
        let full = r#"{"error":{"code":"resource_unavailable","message":"error during placement","details":{}}}"#;
        let srv = serve(vec![
            canned(200, "application/json", SERVER_TYPES),
            canned(412, "application/json", full),
            canned(201, "application/json", &server_created("devtest-a", "fsn1")),
        ]);
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\n"));
        let pod = provider(&srv.base, opts).create_pod(&spec("devtest-a", None)).await.unwrap();
        assert_eq!((pod.id.as_str(), pod.name.as_str(), pod.ssh_ip.as_deref()), ("77", "devtest-a", Some("203.0.113.9")));
        assert_eq!(
            *srv.requests.lock().unwrap(),
            ["GET /server_types?page=1&per_page=50 HTTP/1.1", "POST /servers HTTP/1.1", "POST /servers HTTP/1.1"]
        );
        assert_eq!(sent_locations(&srv), [Some("nbg1".into()), Some("fsn1".into())]);
        let names: Vec<String> = srv.bodies.lock().unwrap()[1..].iter().map(|b| serde_json::from_str::<Value>(b).unwrap()["name"].to_string()).collect();
        assert_eq!(names, ["\"devtest-a\"", "\"devtest-a\""]);
    }

    /// No room anywhere: `Capacity` (so `--keep-trying` waits for some, where "error during
    /// placement" alone read as `Other` and failed at once), naming every location tried and
    /// what each said. A pinned location that's full is `Capacity` too, and walks nowhere.
    #[tokio::test]
    async fn no_room_in_any_location_is_capacity_and_says_where_it_tried() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        let full = |loc: &str| format!(r#"{{"error":{{"code":"resource_unavailable","message":"error during placement ({loc})"}}}}"#);
        let keys = r#"{"ssh_keys": [{"id": 1, "name": "devtest"}]}"#;
        let srv = serve(vec![
            canned(200, "application/json", SERVER_TYPES),
            canned(412, "application/json", &full("nbg1")),
            canned(412, "application/json", &full("fsn1")),
            canned(412, "application/json", &full("hel1")),
            canned(200, "application/json", keys),
            canned(200, "application/json", SERVER_TYPES),
        ]);
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\n"));
        let e = provider(&srv.base, opts).create_pod(&spec("devtest-a", None)).await.unwrap_err();
        let m = e.to_string();
        assert_eq!(e.kind(), Some(K::Capacity), "{m}");
        assert!(m.contains("hetzner create: no room for cx23 in nbg1, fsn1, hel1 — nbg1: hetzner create HTTP 412"), "{m}");
        for loc in ["nbg1", "fsn1", "hel1"] {
            assert!(m.contains(&format!("error during placement ({loc})")), "{loc}: {m}");
        }
        assert_eq!(sent_locations(&srv), [Some("nbg1".into()), Some("fsn1".into()), Some("hel1".into())]);

        let srv = serve(vec![canned(412, "application/json", &full("hel1")), canned(200, "application/json", keys), canned(200, "application/json", SERVER_TYPES)]);
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\nHETZNER_LOCATION=hel1\n"));
        let e = provider(&srv.base, opts).create_pod(&spec("devtest-a", None)).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Capacity), "{e}");
        assert_eq!(sent_locations(&srv), [Some("hel1".into())], "pinned: no catalog read, no other location");
        assert!(!srv.requests.lock().unwrap()[0].starts_with("GET"), "the create came first");
    }

    /// Anything but "no room" stops the walk at once — a bad token or a throttle is the same in
    /// every location, a 5xx is the retry layer's — and an explained refusal after a full location
    /// says where each happened.
    #[tokio::test]
    async fn a_refusal_that_isnt_about_room_stops_the_walk() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        let opts = || HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\n"));
        for (status, kind) in [(401, K::Auth), (429, K::RateLimited), (503, K::Transient)] {
            let srv = serve(vec![canned(200, "application/json", SERVER_TYPES), canned(status, "application/json", r#"{"error":{"code":"x","message":"no"}}"#)]);
            let e = provider(&srv.base, opts()).create_pod(&spec("devtest-a", None)).await.unwrap_err();
            assert_eq!(e.kind(), Some(kind), "{e}");
            assert_eq!(sent_locations(&srv), [Some("nbg1".into())], "{status}: one create");
        }
        let srv = serve(vec![
            canned(200, "application/json", SERVER_TYPES),
            canned(412, "application/json", r#"{"error":{"code":"resource_unavailable","message":"error during placement"}}"#),
            canned(404, "application/json", r#"{"error":{"code":"not_found","message":"SSH key not found"}}"#),
            canned(200, "application/json", SSH_KEYS),
            canned(200, "application/json", SERVER_TYPES),
        ]);
        let e = provider(&srv.base, opts()).create_pod(&spec("devtest-a", None)).await.unwrap_err();
        let m = e.to_string();
        assert!(m.contains("SSH key not found") && m.contains("(in fsn1, after no room in: nbg1: hetzner create HTTP 412"), "{m}");
        assert!(m.contains("the Hetzner project has no SSH key `devtest`"), "still explained: {m}");
        assert_eq!(sent_locations(&srv), [Some("nbg1".into()), Some("fsn1".into())]);
    }

    /// Without the catalog (its read failed) `any` can't choose: one create without a location,
    /// so Hetzner places it — or, with locations listed, just those. An `--api-json` `datacenter`
    /// is the place itself: one create, no `location` (Hetzner refuses both), no catalog read.
    #[tokio::test]
    async fn without_the_catalog_or_with_a_datacenter_there_is_one_place_to_try() {
        use crate::http::test_server::{canned, serve};
        let cfg = |text: &str| HetznerOpts::from_config(&crate::config::Config::parse(text));
        let srv = serve(vec![canned(500, "text/html", "<html>oops</html>"), canned(201, "application/json", &server_created("devtest-a", "hel1"))]);
        provider(&srv.base, cfg("MACHINE_NAME_PREFIX=devtest\n")).create_pod(&spec("devtest-a", None)).await.unwrap();
        assert_eq!(sent_locations(&srv), [None]);
        let srv = serve(vec![
            canned(500, "text/html", "<html>oops</html>"),
            canned(412, "application/json", r#"{"error":{"code":"resource_unavailable","message":"error during placement"}}"#),
            canned(201, "application/json", &server_created("devtest-a", "fsn1")),
        ]);
        provider(&srv.base, cfg("MACHINE_NAME_PREFIX=devtest\nHETZNER_LOCATION=\"hel1 fsn1 any\"\n")).create_pod(&spec("devtest-a", None)).await.unwrap();
        assert_eq!(sent_locations(&srv), [Some("hel1".into()), Some("fsn1".into())]);

        let srv = serve(vec![canned(201, "application/json", &server_created("devtest-a", "fsn1"))]);
        let mut with_dc = spec("devtest-a", None);
        with_dc.api_extra = Some(serde_json::from_str(r#"{"datacenter": "fsn1-dc14"}"#).unwrap());
        provider(&srv.base, cfg("MACHINE_NAME_PREFIX=devtest\n")).create_pod(&with_dc).await.unwrap();
        assert_eq!(sent_locations(&srv), [None]);
        assert_eq!(serde_json::from_str::<Value>(&srv.bodies.lock().unwrap()[0]).unwrap()["datacenter"], "fsn1-dc14");
    }

    /// What `describe` and the preview say for each setting: the old pinned wording unchanged.
    #[test]
    fn the_preview_says_where_a_create_goes() {
        let p = |text: &str| HetznerProvider::new("k", HetznerOpts::from_config(&crate::config::Config::parse(text)));
        let s = spec("devtest-a", None);
        assert_eq!(p("HETZNER_LOCATION=nbg1\n").describe(&s), "cx23 CPU VM, image ubuntu-24.04, location nbg1");
        assert_eq!(p("").describe(&s), "cx23 CPU VM, image ubuntu-24.04, location any (nearest first, the next when one has no room)");
        assert_eq!(p("").preview_create_body(&s).unwrap().get("location"), None);
        assert_eq!(p("HETZNER_LOCATION=hel1,any\n").preview_create_body(&s).unwrap()["location"], "hel1");
    }

    /// One test for both walks (fleet locations, archive server types): no room → walk on; a
    /// token, quota or throttle → stop.
    #[test]
    fn no_room_is_told_apart_from_refusals_that_follow_everywhere() {
        use crate::error::ProviderErrorKind as K;
        let http = |status: u16, body: &str| Error::provider_http(reqwest::StatusCode::from_u16(status).unwrap(), &body, "hetzner create");
        assert!(is_placement_error(&http(412, r#"{"error":{"code":"resource_unavailable","message":"error during placement"}}"#)));
        assert!(is_placement_error(&http(412, r#"{"error":{"code":"resource_unavailable","message":"server type cpx41 not available in nbg1"}}"#)));
        assert!(is_placement_error(&Error::capacity("no capacity")));
        assert!(is_placement_error(&http(422, r#"{"error":{"code":"invalid_input","message":"unsupported location for server type"}}"#)));
        assert!(!is_placement_error(&http(401, r#"{"error":{"code":"unauthorized","message":"placement_error"}}"#)));
        assert!(!is_placement_error(&http(429, r#"{"error":{"code":"rate_limit_exceeded","message":"resource_unavailable"}}"#)));
        assert!(!is_placement_error(&http(503, r#"{"error":{"code":"unavailable","message":"resource_unavailable"}}"#)));
        assert!(!is_placement_error(&http(403, r#"{"error":{"code":"resource_limit_exceeded","message":"server limit reached"}}"#)));
        assert!(!is_placement_error(&http(404, r#"{"error":{"code":"not_found","message":"SSH key not found"}}"#)));
        assert_eq!(http(412, r#"{"error":{"code":"resource_unavailable","message":"error during placement"}}"#).kind(), Some(K::Other), "why the walk tags it");
    }

    /// The $/h is the price where the server is: its own `location` (today's answers), else an
    /// older answer's `datacenter.location`, else the first price listed.
    #[test]
    fn the_hourly_price_is_the_one_where_the_server_runs() {
        let prices = json!([{"location": "fsn1", "price_hourly": {"gross": "0.0368"}}, {"location": "sin", "price_hourly": {"gross": "0.0497"}}]);
        let server = |place: Value| {
            let mut v = json!({"id": 1, "name": "devtest-a", "status": "running", "server_type": {"name": "cpx22", "prices": prices}});
            v.as_object_mut().unwrap().extend(place.as_object().unwrap().clone());
            parse_server(&v).cost_per_hr
        };
        assert_eq!(server(json!({"location": {"name": "sin"}})), Some(0.0497));
        assert_eq!(server(json!({"datacenter": {"location": {"name": "sin"}}})), Some(0.0497));
        assert_eq!(server(json!({})), Some(0.0368));
    }

    /// `offers --all`: every type on sale somewhere, over the locations that sell it now
    /// (cpx41's EU retirement passed, so only ash; cpx22's is announced, so still nbg1), cheapest
    /// type first, its price from its cheapest location. Pure.
    #[test]
    fn the_catalog_lists_what_is_on_sale_cheapest_first() {
        let types: Vec<Value> = serde_json::from_str::<Value>(SERVER_TYPES).unwrap()["server_types"].as_array().unwrap().clone();
        let rows = server_types(&types, 1_792_000_000);
        let got: Vec<(&str, Option<f64>, Vec<&str>)> =
            rows.iter().map(|r| (r.name.as_str(), r.eur_per_hour, r.locations.iter().map(String::as_str).collect())).collect();
        let eu = vec!["fsn1", "nbg1", "hel1"];
        assert_eq!(
            got,
            [
                ("cx23", Some(0.0104), eu.clone()),
                ("cax11", Some(0.0112), eu.clone()),
                ("cx33", Some(0.016), eu.clone()),
                ("cpx22", Some(0.0368), vec!["nbg1"]),
                ("ccx13", Some(0.0817), vec!["ash"]),
                ("cpx42", Some(0.1314), eu),
                ("cpx41", Some(0.2267), vec!["ash"]),
            ]
        );
        let cax11 = &rows[1];
        assert_eq!(
            (cax11.cores, cax11.cpu_type.as_deref(), cax11.architecture.as_deref(), cax11.memory_gb, cax11.disk_gb),
            (Some(2), Some("shared"), Some("arm"), Some(4.0), Some(40))
        );
        assert_eq!(rows[4].cpu_type.as_deref(), Some("dedicated"));
        // A location with no price sorts after the priced ones; a type sold nowhere isn't listed.
        let t = json!([{"name": "t1", "cores": 2,
            "prices": [{"location": "hel1", "price_hourly": {"gross": "0.0300"}}, {"location": "fsn1", "price_hourly": {"gross": "0.0200"}}],
            "locations": [{"name": "nbg1", "deprecation": null}, {"name": "hel1", "deprecation": null}, {"name": "fsn1", "deprecation": null}]},
            {"name": "gone", "locations": []}]);
        let rows = server_types(t.as_array().unwrap(), 1_792_000_000);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].locations.clone(), rows[0].eur_per_hour), (vec!["fsn1".into(), "hel1".into(), "nbg1".into()], Some(0.02)));
    }

    /// `arena offers` on Hetzner, over the real request path: the type create would ask for
    /// (`--server-type` over config), the walk it would take (nearest first, timed as create
    /// times it) and the price where that walk starts; a type Hetzner doesn't have is an error.
    #[tokio::test]
    async fn an_offer_is_what_a_create_would_get() {
        use crate::http::test_server::{canned, serve};
        let srv = serve(vec![canned(200, "application/json", SERVER_TYPES); 3]);
        let mut p = provider(&srv.base, HetznerOpts::default());
        p.distances = Distances::known(&[("hel1", 1), ("fsn1", 24), ("nbg1", 26)]);
        let cx23 = p.offer(&spec("devtest-a", None)).await.unwrap();
        assert_eq!(
            cx23,
            ServerType {
                name: "cx23".into(),
                cores: Some(2),
                cpu_type: Some("shared".into()),
                architecture: Some("x86".into()),
                memory_gb: Some(4.0),
                disk_gb: Some(40),
                eur_per_hour: Some(0.0104),
                locations: vec!["hel1".into(), "fsn1".into(), "nbg1".into()],
            }
        );
        let cpx41 = p.offer(&spec("devtest-a", Some("cpx41"))).await.unwrap();
        assert_eq!((cpx41.locations, cpx41.eur_per_hour, cpx41.disk_gb), (vec!["ash".to_string()], Some(0.2267), Some(240)));
        let e = p.offer(&spec("devtest-a", Some("cx99"))).await.unwrap_err();
        assert!(matches!(&e, Error::Config(m) if m.contains("Hetzner has no server type `cx99`")), "a refusal, not a failed read: {e:?}");
    }

    /// The offer's walk is the create's, location for location: a listed `HETZNER_LOCATION` is
    /// walked as listed (never reordered by distance, no catalog read on create), and `any` after
    /// it adds the rest nearest first — over the real request path, with `walk_in` shared.
    #[tokio::test]
    async fn an_offer_walks_where_a_create_walks() {
        use crate::http::test_server::{canned, serve};
        let near = [("nbg1", 1), ("hel1", 2), ("fsn1", 24)];
        for (setting, want) in [("fsn1,hel1", vec!["fsn1", "hel1"]), ("fsn1,any", vec!["fsn1", "nbg1", "hel1"])] {
            let opts = HetznerOpts::from_config(&crate::config::Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nHETZNER_LOCATION={setting}\n")));
            let full = r#"{"error":{"code":"resource_unavailable","message":"error during placement"}}"#;
            let mut answers = if opts.locations.any { vec![canned(200, "application/json", SERVER_TYPES)] } else { Vec::new() };
            answers.extend(want.iter().map(|_| canned(412, "application/json", full)));
            // The failed create's explanation reads the project's keys and the types again.
            answers.extend([canned(200, "application/json", r#"{"ssh_keys": [{"id": 1, "name": "devtest"}]}"#), canned(200, "application/json", SERVER_TYPES)]);
            let srv = serve(answers);
            let mut p = provider(&srv.base, opts.clone());
            p.distances = Distances::known(&near);
            assert!(p.create_pod(&spec("devtest-a", None)).await.is_err(), "{setting}: no room anywhere");
            let sent: Vec<Option<String>> = want.iter().map(|l| Some(l.to_string())).collect();
            assert_eq!(sent_locations(&srv), sent, "{setting}: create");
            let srv = serve(vec![canned(200, "application/json", SERVER_TYPES)]);
            let mut p = provider(&srv.base, opts);
            p.distances = Distances::known(&near);
            assert_eq!(p.offer(&spec("devtest-a", None)).await.unwrap().locations, want, "{setting}: offer");
        }
        // A listed location where the type is retired is still where a create goes first (it
        // doesn't read the catalog): the offer lists it as is, never drops it or refuses.
        let opts = HetznerOpts::from_config(&crate::config::Config::parse("MACHINE_NAME_PREFIX=devtest\nHETZNER_LOCATION=fsn1,ash\n"));
        let srv = serve(vec![canned(200, "application/json", SERVER_TYPES)]);
        let cpx41 = provider(&srv.base, opts).offer(&spec("devtest-a", Some("cpx41"))).await.unwrap();
        assert_eq!(cpx41.locations, ["fsn1", "ash"]);
    }
}
