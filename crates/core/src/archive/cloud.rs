//! The archive's Hetzner API seam: [`ArchiveCloud`], the real [`HetznerArchiveCloud`], and
//! (`cfg(test)` / feature `test-util`) a stateful [`FakeArchiveCloud`] that records every call.
//!
//! Separate from the fleet's `HetznerProvider` on purpose: that one only ever sees servers
//! *without* the archive label (`is_archive_server`), and the archive needs things a pod never
//! does — volumes, protection, SSH-key objects, actions, prices — under its own token
//! (`ARCHIVE_HETZNER_API_KEY`, a separate project recommended). Every listing **fails closed**:
//! an unexpected shape or a pagination that doesn't advance is an error, never "no archive" —
//! `up` would otherwise create a second one, and `teardown --check` would call it clear.
//! Responses are read status-first ([`crate::http`]).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::{json, Value};

use super::{ArchiveNames, Tier, IMAGE, LABEL_KEY};
use crate::error::{Error, Result};
// Hetzner's own reading of its errors (a project limit is a quota, not a bad token).
use crate::provider::hetzner::{send_json, send_ok};

/// Resource labels (Hetzner's are string → string).
pub type Labels = BTreeMap<String, String>;

/// A server as the archive sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudServer {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub location: String,
    pub ipv4: Option<String>,
    pub labels: Labels,
    pub delete_protected: bool,
    pub server_type: String,
}

/// A volume.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudVolume {
    pub id: u64,
    pub name: String,
    pub size_gb: u32,
    pub location: String,
    /// The server it's attached to.
    pub server: Option<u64>,
    /// The filesystem Hetzner put on it at creation (`ext4`), `None` = created unformatted.
    pub format: Option<String>,
    /// Its device path on the server (`/dev/disk/by-id/scsi-0HC_Volume_<id>`) — what fstab and
    /// `resize2fs` name, so a renamed `/dev/sdX` never matters.
    pub linux_device: String,
    pub labels: Labels,
    pub delete_protected: bool,
}

impl CloudVolume {
    /// Its tier, from its tier label.
    pub fn tier(&self) -> Option<Tier> {
        self.labels.get(super::TIER_LABEL_KEY).and_then(|t| Tier::parse(t))
    }
}

/// An SSH-key object (the archive's admin pubkey, given to Hetzner for the server's root).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudSshKey {
    pub id: u64,
    pub name: String,
    pub public_key: String,
    pub labels: Labels,
}

/// What the archive found in the project. For one prefix this also holds same-NAMED resources
/// that lack the label (an `up` must refuse those, never adopt or collide with them).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inventory {
    pub servers: Vec<CloudServer>,
    pub volumes: Vec<CloudVolume>,
    pub ssh_keys: Vec<CloudSshKey>,
}

impl Inventory {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.volumes.is_empty() && self.ssh_keys.is_empty()
    }

    /// The prefixes whose archive this holds (from the label), sorted.
    pub fn prefixes(&self) -> Vec<String> {
        let mut p: Vec<String> = self
            .servers
            .iter()
            .map(|s| &s.labels)
            .chain(self.volumes.iter().map(|v| &v.labels))
            .chain(self.ssh_keys.iter().map(|k| &k.labels))
            .filter_map(|l| l.get(LABEL_KEY).cloned())
            .collect();
        p.sort();
        p.dedup();
        p
    }

    /// Only `names`' labelled resources.
    pub fn owned_by(&self, names: &ArchiveNames) -> Inventory {
        Inventory {
            servers: self.servers.iter().filter(|s| names.owns(&s.labels)).cloned().collect(),
            volumes: self.volumes.iter().filter(|v| names.owns(&v.labels)).cloned().collect(),
            ssh_keys: self.ssh_keys.iter().filter(|k| names.owns(&k.labels)).cloned().collect(),
        }
    }

    /// This cohort's (labelled) volume of `tier`, if exactly one.
    pub fn volume(&self, names: &ArchiveNames, tier: Tier) -> Option<&CloudVolume> {
        let mut it = self.volumes.iter().filter(|v| names.owns(&v.labels) && v.tier() == Some(tier));
        let first = it.next()?;
        it.next().is_none().then_some(first)
    }

    /// This cohort's (labelled) server, if exactly one.
    pub fn server(&self, names: &ArchiveNames) -> Option<&CloudServer> {
        let mut it = self.servers.iter().filter(|s| names.owns(&s.labels));
        let first = it.next()?;
        it.next().is_none().then_some(first)
    }
}

/// Live prices for the plan and status lines (EUR excluding VAT).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    /// Per GB of volume per month.
    pub volume_gb_month: f64,
    /// The server type in the location, per month (`None` = not listed there).
    pub server_month: Option<f64>,
    /// A primary IPv4 in the location, per month (`None` = not listed).
    pub ipv4_month: Option<f64>,
}

impl Prices {
    /// Hetzner's list prices (EUR excl. VAT) as its `/pricing` answered on 2026-10-13 (a cx23 in
    /// nbg1, a primary IPv4, a GB of volume): what every surface prices the archive at when the
    /// live list can't be read — the same table everywhere, so no surface keeps a figure of its
    /// own (W7 live: `teardown` said ≈ €7/month from constants of its own, without the IPv4,
    /// where `up`/`status`/`balance` said €8.62).
    pub const LIST: Prices = Prices { volume_gb_month: 0.0767, server_month: Some(6.49), ipv4_month: Some(0.60) };

    /// What `servers` archive servers (each with its primary IPv4) and `gb` of volumes cost a
    /// month — THE rule every surface prices the archive by (`archive up`/`down`/`resize`/
    /// `status`, `teardown --check`, `balance`). A part the live list lacks (the type isn't sold
    /// in that location) is priced from [`Prices::LIST`], never left out.
    pub fn monthly_for(&self, servers: usize, gb: u32) -> f64 {
        let server = self.server_month.or(Self::LIST.server_month).unwrap_or(0.0) + self.ipv4_month.or(Self::LIST.ipv4_month).unwrap_or(0.0);
        servers as f64 * server + f64::from(gb) * self.volume_gb_month
    }

    /// [`Prices::monthly_for`] for none or one server.
    pub fn monthly(&self, server: bool, gb: u32) -> f64 {
        self.monthly_for(usize::from(server), gb)
    }
}

/// `€X.XX/month` — how every surface writes the archive's monthly cost.
pub fn eur_month(eur: f64) -> String {
    format!("€{eur:.2}/month")
}

/// The prices an archive of that server type in that location is shown at: the live `/pricing`
/// when it answers, else [`Prices::LIST`] — and whether they are live.
pub async fn prices_or_list(cloud: &dyn ArchiveCloud, server_type: &str, location: &str) -> (Prices, bool) {
    match cloud.prices(server_type, location).await {
        Ok(p) => (p, true),
        Err(_) => (Prices::LIST, false),
    }
}

/// The prices one cohort's archive (`owned`) is shown at by every surface: [`prices_or_list`]
/// for its server type and location ([`priced_as`]).
pub async fn archive_prices(cloud: &dyn ArchiveCloud, owned: &Inventory) -> Prices {
    let (server_type, location) = priced_as(owned);
    prices_or_list(cloud, &server_type, &location).await.0
}

/// The server type and location one cohort's archive (`owned`) is priced at: its server's;
/// with only its volumes left (orphans), the first default type in their location.
pub fn priced_as(owned: &Inventory) -> (String, String) {
    match owned.servers.first() {
        Some(s) => (s.server_type.clone(), s.location.clone()),
        None => (super::DEFAULT_SERVER_TYPES[0].to_string(), owned.volumes.first().map(|v| v.location.clone()).unwrap_or_else(|| "nbg1".into())),
    }
}

/// Hours in a month, for a monthly price per hour: Hetzner bills by the hour, capped at the
/// monthly price — which an always-on archive always reaches.
pub const HOURS_PER_MONTH: f64 = 730.0;

/// What this cohort's archive bills per hour (EUR excl. VAT): [`Prices::monthly_for`] its
/// server(s) and volumes ([`prices_or_list`]) over [`HOURS_PER_MONTH`] — the figure `archive
/// status` and `teardown --check` show per month. For `arena balance`'s Hetzner row — the fleet
/// listing leaves the archive out, so nothing else prices it. Reads only (one inventory, one
/// price list); `Ok(None)`: this cohort has no archive.
pub async fn eur_per_hr(cloud: &dyn ArchiveCloud, names: &ArchiveNames) -> Result<Option<f64>> {
    let owned = cloud.inventory(Some(&names.prefix)).await?.owned_by(names);
    if owned.servers.is_empty() && owned.volumes.is_empty() {
        return Ok(None);
    }
    let gb: u32 = owned.volumes.iter().map(|v| v.size_gb).sum();
    Ok(Some(archive_prices(cloud, &owned).await.monthly_for(owned.servers.len(), gb) / HOURS_PER_MONTH))
}

/// The Hetzner calls the archive makes. `inventory` is the only read every command needs;
/// `archive sync` makes none at all (it takes no `ArchiveCloud`).
#[async_trait]
pub trait ArchiveCloud: Send + Sync {
    /// Labelled resources: `label_selector=arena-archive=<prefix>` (or any prefix with `None`),
    /// every page; for a prefix also the same-named unlabelled ones. Fails closed.
    async fn inventory(&self, prefix: Option<&str>) -> Result<Inventory>;
    async fn create_ssh_key(&self, name: &str, public_key: &str, labels: &Labels) -> Result<CloudSshKey>;
    /// `POST /servers`: the server and its create action.
    async fn create_server(&self, body: &Value) -> Result<(CloudServer, u64)>;
    /// `POST /volumes`: the volume and its actions (create, and attach when `server` is set).
    async fn create_volume(&self, body: &Value) -> Result<(CloudVolume, Vec<u64>)>;
    async fn attach(&self, volume: u64, server: u64) -> Result<u64>;
    async fn detach(&self, volume: u64) -> Result<u64>;
    async fn resize(&self, volume: u64, size_gb: u32) -> Result<u64>;
    /// `{delete: on, rebuild: on}`.
    async fn protect_server(&self, id: u64, on: bool) -> Result<u64>;
    /// `POST /servers/{id}/actions/poweron`: start a server that is off (a shutdown inside it, a
    /// console power-off) — `up` does, rather than wait for SSH that never comes.
    async fn power_on(&self, id: u64) -> Result<u64>;
    /// `{delete: on}`.
    async fn protect_volume(&self, id: u64, on: bool) -> Result<u64>;
    async fn delete_server(&self, id: u64) -> Result<()>;
    async fn delete_volume(&self, id: u64) -> Result<()>;
    async fn delete_ssh_key(&self, id: u64) -> Result<()>;
    /// Poll `GET /actions/{id}` until it succeeds (`Ok`), errors or `limit` passes (`Err`).
    async fn wait_action(&self, id: u64, limit: Duration) -> Result<()>;
    /// `GET /pricing` for a server type in a location.
    async fn prices(&self, server_type: &str, location: &str) -> Result<Prices>;
}

/// The `POST /servers` body. Pure (S4): `ssh_keys` is exactly the archive's admin key object —
/// never the fleet's default key (`HETZNER_SSH_KEY`, else the cohort key named after the
/// prefix, which every participant holds) — no `user_data`, the label, IPv4 + IPv6.
pub fn server_body(names: &ArchiveNames, server_type: &str, location: &str, admin_key_id: u64) -> Value {
    json!({
        "name": names.server(),
        "server_type": server_type,
        "image": IMAGE,
        "location": location,
        "ssh_keys": [admin_key_id],
        "labels": names.labels(),
        "start_after_create": true,
        "public_net": {"enable_ipv4": true, "enable_ipv6": true},
    })
}

/// The `POST /volumes` body. Pure (S4): created attached, formatted ext4 **by Hetzner** at
/// creation (the only place a format is ever asked for — arena never runs mkfs), not
/// automounted (setup mounts it at a fixed path), labelled with the prefix and its tier.
pub fn volume_body(names: &ArchiveNames, tier: Tier, size_gb: u32, server_id: u64) -> Value {
    json!({
        "name": names.volume(tier),
        "size": size_gb,
        "server": server_id,
        "format": "ext4",
        "automount": false,
        "labels": names.volume_labels(tier),
    })
}

/// Whether a create-server error means "try the next server type" (and only that: a bad token
/// or a quota error must stop, not walk the list) — the fleet's test for "try the next
/// location", one definition for both walks.
pub use crate::provider::hetzner::is_placement_error;

// ---------------------------------------------------------------------------------------
// Parsing (pure, fail closed)
// ---------------------------------------------------------------------------------------

fn drift(what: &str, v: &Value) -> Error {
    Error::provider(format!("hetzner archive: unexpected {what} shape: {}", crate::provider::body_excerpt(v)))
}

fn labels_of(v: &Value) -> Result<Labels> {
    match v.get("labels") {
        None | Some(Value::Null) => Ok(Labels::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())).ok_or_else(|| drift("labels", v)))
            .collect(),
        Some(_) => Err(drift("labels", v)),
    }
}

/// One server object.
pub fn parse_server(v: &Value) -> Result<CloudServer> {
    let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| drift("server", v))?;
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| drift("server", v))?.to_string();
    Ok(CloudServer {
        id,
        name,
        status: v.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string(),
        location: v
            .pointer("/datacenter/location/name")
            .or_else(|| v.pointer("/location/name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ipv4: v.pointer("/public_net/ipv4/ip").and_then(Value::as_str).map(String::from),
        labels: labels_of(v)?,
        delete_protected: v.pointer("/protection/delete").and_then(Value::as_bool).unwrap_or(false),
        server_type: v.pointer("/server_type/name").and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

/// One volume object.
pub fn parse_volume(v: &Value) -> Result<CloudVolume> {
    let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| drift("volume", v))?;
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| drift("volume", v))?.to_string();
    let size_gb = v.get("size").and_then(Value::as_u64).and_then(|s| u32::try_from(s).ok()).ok_or_else(|| drift("volume", v))?;
    let server = match v.get("server") {
        None | Some(Value::Null) => None,
        Some(s) => Some(s.as_u64().ok_or_else(|| drift("volume", v))?),
    };
    Ok(CloudVolume {
        id,
        name,
        size_gb,
        location: v.pointer("/location/name").and_then(Value::as_str).unwrap_or("").to_string(),
        server,
        format: v.get("format").and_then(Value::as_str).map(String::from),
        linux_device: v.get("linux_device").and_then(Value::as_str).unwrap_or("").to_string(),
        labels: labels_of(v)?,
        delete_protected: v.pointer("/protection/delete").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// One SSH-key object.
pub fn parse_ssh_key(v: &Value) -> Result<CloudSshKey> {
    let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| drift("ssh key", v))?;
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| drift("ssh key", v))?.to_string();
    Ok(CloudSshKey {
        id,
        name,
        public_key: v.get("public_key").and_then(Value::as_str).unwrap_or("").to_string(),
        labels: labels_of(v)?,
    })
}

/// One page of a list: the `key` array (required — a body without it is drift, never an empty
/// list) and the next page number.
fn list_page<'a>(body: &'a Value, key: &str) -> Result<(&'a Vec<Value>, Option<u64>)> {
    let items = body.get(key).and_then(Value::as_array).ok_or_else(|| drift(&format!("{key} list"), body))?;
    Ok((items, body.pointer("/meta/pagination/next_page").and_then(Value::as_u64)))
}

/// The action id in a response (`{"action": {"id": …}}`).
fn action_id(body: &Value) -> Result<u64> {
    body.pointer("/action/id").and_then(Value::as_u64).ok_or_else(|| drift("action", body))
}

/// A price string (`"5.4900000000"`) from `price_monthly.net`.
fn price(v: Option<&Value>) -> Option<f64> {
    v.and_then(|p| p.pointer("/price_monthly/net").or_else(|| p.get("net"))).and_then(Value::as_str).and_then(|s| s.parse().ok())
}

/// `GET /pricing` → the archive's prices. Pure.
pub fn parse_prices(body: &Value, server_type: &str, location: &str) -> Result<Prices> {
    let pricing = body.get("pricing").ok_or_else(|| drift("pricing", body))?;
    let volume_gb_month = pricing
        .pointer("/volume/price_per_gb_month/net")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| drift("pricing", body))?;
    let in_location = |list: Option<&Value>| -> Option<f64> {
        list.and_then(Value::as_array)?.iter().find(|p| p.get("location").and_then(Value::as_str) == Some(location)).and_then(|p| price(Some(p)))
    };
    let server_month = pricing
        .get("server_types")
        .and_then(Value::as_array)
        .and_then(|types| types.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(server_type)))
        .and_then(|t| in_location(t.get("prices")));
    let ipv4_month = pricing
        .get("primary_ips")
        .and_then(Value::as_array)
        .and_then(|ips| ips.iter().find(|t| t.get("type").and_then(Value::as_str) == Some("ipv4")))
        .and_then(|t| in_location(t.get("prices")));
    Ok(Prices { volume_gb_month, server_month, ipv4_month })
}

// ---------------------------------------------------------------------------------------
// The real client
// ---------------------------------------------------------------------------------------

/// `https://api.hetzner.cloud/v1` with the archive's token.
pub struct HetznerArchiveCloud {
    token: String,
    client: Client,
    base: String,
    /// How often [`ArchiveCloud::wait_action`] polls.
    poll: Duration,
}

/// Items per page (Hetzner's maximum) and the page cap (fail rather than return a partial list).
const PER_PAGE: u64 = 50;
const MAX_PAGES: u64 = 20;

impl HetznerArchiveCloud {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), client: Client::new(), base: crate::provider::hetzner::BASE.to_string(), poll: Duration::from_secs(2) }
    }

    /// Point at another base URL (tests: a loopback server).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    #[cfg(test)]
    fn for_test(base: &str) -> Self {
        Self { token: "k".into(), client: crate::http::test_server::client(), base: base.into(), poll: Duration::from_millis(1) }
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.token)
    }

    /// Every page of `GET /<path>?<query>`'s `key` array.
    async fn list_all(&self, path: &str, key: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page: u64 = 1;
        for _ in 0..MAX_PAGES {
            let mut q: Vec<(&str, String)> = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("per_page", PER_PAGE.to_string()));
            let rb = self.auth(self.client.get(format!("{}/{path}", self.base)).query(&q));
            let body = send_json(rb, &format!("hetzner archive list {path}")).await?;
            let (items, next) = list_page(&body, key)?;
            out.extend(items.iter().cloned());
            match next {
                None => return Ok(out),
                Some(n) if n > page => page = n,
                Some(n) => return Err(Error::provider(format!("hetzner archive list {path}: pagination did not advance ({page} -> {n})"))),
            }
        }
        Err(Error::provider(format!("hetzner archive list {path}: more than {MAX_PAGES} pages — refusing a partial listing")))
    }

    async fn post(&self, path: &str, body: &Value, ctx: &str) -> Result<Value> {
        send_json(self.auth(self.client.post(format!("{}/{path}", self.base)).json(body)), ctx).await
    }
}

/// Merge `extra` into `into`, skipping ids already there.
fn merge_by_id<T>(into: &mut Vec<T>, extra: Vec<T>, id: impl Fn(&T) -> u64) {
    for x in extra {
        if !into.iter().any(|y| id(y) == id(&x)) {
            into.push(x);
        }
    }
}

#[async_trait]
impl ArchiveCloud for HetznerArchiveCloud {
    async fn inventory(&self, prefix: Option<&str>) -> Result<Inventory> {
        let selector = match prefix {
            Some(p) => format!("{LABEL_KEY}={p}"),
            None => LABEL_KEY.to_string(),
        };
        let q = [("label_selector", selector)];
        let mut inv = Inventory {
            servers: self.list_all("servers", "servers", &q).await?.iter().map(parse_server).collect::<Result<_>>()?,
            volumes: self.list_all("volumes", "volumes", &q).await?.iter().map(parse_volume).collect::<Result<_>>()?,
            ssh_keys: self.list_all("ssh_keys", "ssh_keys", &q).await?.iter().map(parse_ssh_key).collect::<Result<_>>()?,
        };
        // The same NAMES without the label: an `up` must see a lookalike to refuse it (Hetzner
        // names are unique per project, so a create would fail on it anyway — mid-way).
        if let Some(p) = prefix {
            let names = ArchiveNames { prefix: p.to_string() };
            let by_name = |n: String| [("name", n)];
            let s = self.list_all("servers", "servers", &by_name(names.server())).await?;
            merge_by_id(&mut inv.servers, s.iter().map(parse_server).collect::<Result<_>>()?, |x| x.id);
            for tier in Tier::ALL {
                let v = self.list_all("volumes", "volumes", &by_name(names.volume(tier))).await?;
                merge_by_id(&mut inv.volumes, v.iter().map(parse_volume).collect::<Result<_>>()?, |x| x.id);
            }
            let k = self.list_all("ssh_keys", "ssh_keys", &by_name(names.ssh_key())).await?;
            merge_by_id(&mut inv.ssh_keys, k.iter().map(parse_ssh_key).collect::<Result<_>>()?, |x| x.id);
        }
        Ok(inv)
    }

    async fn create_ssh_key(&self, name: &str, public_key: &str, labels: &Labels) -> Result<CloudSshKey> {
        let body = self.post("ssh_keys", &json!({"name": name, "public_key": public_key, "labels": labels}), "hetzner create ssh key").await?;
        parse_ssh_key(body.get("ssh_key").ok_or_else(|| drift("ssh key", &body))?)
    }

    async fn create_server(&self, req: &Value) -> Result<(CloudServer, u64)> {
        let body = self.post("servers", req, "hetzner create server").await?;
        let server = parse_server(body.get("server").ok_or_else(|| drift("server", &body))?)?;
        Ok((server, action_id(&body)?))
    }

    async fn create_volume(&self, req: &Value) -> Result<(CloudVolume, Vec<u64>)> {
        let body = self.post("volumes", req, "hetzner create volume").await?;
        let volume = parse_volume(body.get("volume").ok_or_else(|| drift("volume", &body))?)?;
        let mut actions = Vec::new();
        if let Ok(id) = action_id(&body) {
            actions.push(id);
        }
        for a in body.get("next_actions").and_then(Value::as_array).into_iter().flatten() {
            actions.extend(a.get("id").and_then(Value::as_u64));
        }
        Ok((volume, actions))
    }

    async fn attach(&self, volume: u64, server: u64) -> Result<u64> {
        let body = json!({"server": server, "automount": false});
        action_id(&self.post(&format!("volumes/{volume}/actions/attach"), &body, "hetzner attach volume").await?)
    }

    async fn detach(&self, volume: u64) -> Result<u64> {
        action_id(&self.post(&format!("volumes/{volume}/actions/detach"), &json!({}), "hetzner detach volume").await?)
    }

    async fn resize(&self, volume: u64, size_gb: u32) -> Result<u64> {
        action_id(&self.post(&format!("volumes/{volume}/actions/resize"), &json!({"size": size_gb}), "hetzner resize volume").await?)
    }

    async fn protect_server(&self, id: u64, on: bool) -> Result<u64> {
        let body = json!({"delete": on, "rebuild": on});
        action_id(&self.post(&format!("servers/{id}/actions/change_protection"), &body, "hetzner server protection").await?)
    }

    async fn protect_volume(&self, id: u64, on: bool) -> Result<u64> {
        action_id(&self.post(&format!("volumes/{id}/actions/change_protection"), &json!({"delete": on}), "hetzner volume protection").await?)
    }

    async fn power_on(&self, id: u64) -> Result<u64> {
        action_id(&self.post(&format!("servers/{id}/actions/poweron"), &json!({}), "hetzner power on server").await?)
    }

    async fn delete_server(&self, id: u64) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/servers/{id}", self.base))), "hetzner delete server").await
    }

    async fn delete_volume(&self, id: u64) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/volumes/{id}", self.base))), "hetzner delete volume").await
    }

    async fn delete_ssh_key(&self, id: u64) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/ssh_keys/{id}", self.base))), "hetzner delete ssh key").await
    }

    async fn wait_action(&self, id: u64, limit: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let body = send_json(self.auth(self.client.get(format!("{}/actions/{id}", self.base))), "hetzner action").await?;
            match body.pointer("/action/status").and_then(Value::as_str) {
                Some("success") => return Ok(()),
                Some("error") => {
                    let why = body.pointer("/action/error/message").and_then(Value::as_str).unwrap_or("no message");
                    let cmd = body.pointer("/action/command").and_then(Value::as_str).unwrap_or("action");
                    return Err(Error::provider(format!("hetzner {cmd} (action {id}) failed: {why}")));
                }
                Some(_) => {}
                None => return Err(drift("action", &body)),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout { what: format!("hetzner action {id}"), after: limit });
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    async fn prices(&self, server_type: &str, location: &str) -> Result<Prices> {
        let body = send_json(self.auth(self.client.get(format!("{}/pricing", self.base))), "hetzner pricing").await?;
        parse_prices(&body, server_type, location)
    }
}

// ---------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fake::{CloudCall, FakeArchiveCloud};

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;

    /// One recorded call.
    #[derive(Debug, Clone, PartialEq)]
    pub enum CloudCall {
        Inventory(Option<String>),
        CreateSshKey(String),
        CreateServer(Value),
        CreateVolume(Value),
        Attach(u64, u64),
        Detach(u64),
        Resize(u64, u32),
        ProtectServer(u64, bool),
        ProtectVolume(u64, bool),
        PowerOn(u64),
        DeleteServer(u64),
        DeleteVolume(u64),
        DeleteSshKey(u64),
        WaitAction(u64),
        Prices,
    }

    impl CloudCall {
        /// Whether it changes anything in the project (reads and waits don't).
        pub fn is_mutating(&self) -> bool {
            !matches!(self, CloudCall::Inventory(_) | CloudCall::WaitAction(_) | CloudCall::Prices)
        }

        /// Whether it deletes something.
        pub fn is_delete(&self) -> bool {
            matches!(self, CloudCall::DeleteServer(_) | CloudCall::DeleteVolume(_) | CloudCall::DeleteSshKey(_))
        }
    }

    #[derive(Debug, Default)]
    struct World {
        inv: Inventory,
        next_id: u64,
        calls: Vec<CloudCall>,
        /// Fail every inventory call with this message.
        fail_inventory: Option<String>,
        /// Server types that answer `resource_unavailable`.
        unavailable: Vec<String>,
        /// Fail the n-th (1-based) volume create.
        fail_volume_create: Option<usize>,
        /// `/pricing` fails.
        fail_prices: bool,
        volume_creates: usize,
    }

    /// A stateful in-memory Hetzner project for the archive: creates, attaches, protects and
    /// deletes what it holds (refusing a delete of a protected resource, like Hetzner), records
    /// every call, and fails where a test asks.
    #[derive(Debug, Default)]
    pub struct FakeArchiveCloud {
        world: Mutex<World>,
    }

    impl FakeArchiveCloud {
        pub fn new() -> Self {
            Self { world: Mutex::new(World { next_id: 100, ..Default::default() }) }
        }

        /// Start from `inv` (ids of new resources follow on from 100).
        pub fn with(inv: Inventory) -> Self {
            let f = Self::new();
            f.world.lock().unwrap().inv = inv;
            f
        }

        /// Every inventory call fails (`None` heals it).
        pub fn fail_inventory(&self, why: Option<&str>) {
            self.world.lock().unwrap().fail_inventory = why.map(String::from);
        }

        /// These server types answer `resource_unavailable`.
        pub fn unavailable(&self, types: &[&str]) {
            self.world.lock().unwrap().unavailable = types.iter().map(|s| s.to_string()).collect();
        }

        /// `/pricing` fails (or answers again).
        pub fn fail_prices(&self, fail: bool) {
            self.world.lock().unwrap().fail_prices = fail;
        }

        /// The `n`-th volume create (1-based, counting from now) fails with a 500.
        pub fn fail_volume_create(&self, n: usize) {
            let mut w = self.world.lock().unwrap();
            w.fail_volume_create = Some(n);
            w.volume_creates = 0;
        }

        pub fn calls(&self) -> Vec<CloudCall> {
            self.world.lock().unwrap().calls.clone()
        }

        pub fn clear_calls(&self) {
            self.world.lock().unwrap().calls.clear();
        }

        /// The mutating calls so far.
        pub fn mutations(&self) -> Vec<CloudCall> {
            self.calls().into_iter().filter(CloudCall::is_mutating).collect()
        }

        /// What the project holds now.
        pub fn state(&self) -> Inventory {
            self.world.lock().unwrap().inv.clone()
        }

        fn record(&self, c: CloudCall) -> std::sync::MutexGuard<'_, World> {
            let mut w = self.world.lock().unwrap();
            w.calls.push(c);
            w
        }
    }

    fn id_of(w: &mut World) -> u64 {
        w.next_id += 1;
        w.next_id
    }

    fn labels_from(v: &Value) -> Labels {
        v.get("labels")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default()
    }

    fn not_found(what: &str, id: u64) -> Error {
        Error::provider(format!("hetzner {what} HTTP 404 Not Found: {{\"error\":{{\"code\":\"not_found\",\"message\":\"{what} {id} not found\"}}}}"))
    }

    fn protected(what: &str, id: u64) -> Error {
        Error::provider(format!("hetzner delete {what} HTTP 423: {{\"error\":{{\"code\":\"protected\",\"message\":\"{what} {id} is protected\"}}}}"))
    }

    #[async_trait]
    impl ArchiveCloud for FakeArchiveCloud {
        async fn inventory(&self, prefix: Option<&str>) -> Result<Inventory> {
            let w = self.record(CloudCall::Inventory(prefix.map(String::from)));
            if let Some(why) = &w.fail_inventory {
                return Err(Error::provider(why.clone()));
            }
            let keep = |l: &Labels, name: &str| match prefix {
                None => l.contains_key(LABEL_KEY),
                Some(p) => {
                    let n = ArchiveNames { prefix: p.to_string() };
                    n.owns(l) || [n.server(), n.volume(Tier::Snap), n.volume(Tier::Big), n.ssh_key()].iter().any(|x| x == name)
                }
            };
            Ok(Inventory {
                servers: w.inv.servers.iter().filter(|s| keep(&s.labels, &s.name)).cloned().collect(),
                volumes: w.inv.volumes.iter().filter(|v| keep(&v.labels, &v.name)).cloned().collect(),
                ssh_keys: w.inv.ssh_keys.iter().filter(|k| keep(&k.labels, &k.name)).cloned().collect(),
            })
        }

        async fn create_ssh_key(&self, name: &str, public_key: &str, labels: &Labels) -> Result<CloudSshKey> {
            let mut w = self.record(CloudCall::CreateSshKey(name.to_string()));
            if w.inv.ssh_keys.iter().any(|k| k.name == name) {
                return Err(Error::provider("hetzner create ssh key HTTP 409: uniqueness_error"));
            }
            let id = id_of(&mut w);
            let k = CloudSshKey { id, name: name.into(), public_key: public_key.into(), labels: labels.clone() };
            w.inv.ssh_keys.push(k.clone());
            Ok(k)
        }

        async fn create_server(&self, body: &Value) -> Result<(CloudServer, u64)> {
            let mut w = self.record(CloudCall::CreateServer(body.clone()));
            let ty = body["server_type"].as_str().unwrap_or("").to_string();
            if w.unavailable.contains(&ty) {
                return Err(Error::provider(format!(
                    "hetzner create server HTTP 412: {{\"error\":{{\"code\":\"resource_unavailable\",\"message\":\"{ty} is unavailable\"}}}}"
                )));
            }
            let name = body["name"].as_str().unwrap_or("").to_string();
            if w.inv.servers.iter().any(|s| s.name == name) {
                return Err(Error::provider("hetzner create server HTTP 409: uniqueness_error"));
            }
            let id = id_of(&mut w);
            let s = CloudServer {
                id,
                name,
                status: "running".into(),
                location: body["location"].as_str().unwrap_or("").into(),
                ipv4: Some(format!("203.0.113.{}", id % 250)),
                labels: labels_from(body),
                delete_protected: false,
                server_type: ty,
            };
            w.inv.servers.push(s.clone());
            let action = id_of(&mut w);
            Ok((s, action))
        }

        async fn create_volume(&self, body: &Value) -> Result<(CloudVolume, Vec<u64>)> {
            let mut w = self.record(CloudCall::CreateVolume(body.clone()));
            w.volume_creates += 1;
            if w.fail_volume_create == Some(w.volume_creates) {
                return Err(Error::provider("hetzner create volume HTTP 500: {\"error\":{\"code\":\"server_error\"}}"));
            }
            let name = body["name"].as_str().unwrap_or("").to_string();
            if w.inv.volumes.iter().any(|v| v.name == name) {
                return Err(Error::provider("hetzner create volume HTTP 409: uniqueness_error"));
            }
            let id = id_of(&mut w);
            let server = body["server"].as_u64();
            let location = server
                .and_then(|s| w.inv.servers.iter().find(|x| x.id == s))
                .map(|s| s.location.clone())
                .or_else(|| body["location"].as_str().map(String::from))
                .unwrap_or_default();
            let v = CloudVolume {
                id,
                name,
                size_gb: body["size"].as_u64().unwrap_or(0) as u32,
                location,
                server,
                format: body["format"].as_str().map(String::from),
                linux_device: format!("/dev/disk/by-id/scsi-0HC_Volume_{id}"),
                labels: labels_from(body),
                delete_protected: false,
            };
            w.inv.volumes.push(v.clone());
            let a = id_of(&mut w);
            Ok((v, vec![a]))
        }

        async fn attach(&self, volume: u64, server: u64) -> Result<u64> {
            let mut w = self.record(CloudCall::Attach(volume, server));
            let v = w.inv.volumes.iter_mut().find(|v| v.id == volume).ok_or_else(|| not_found("volume", volume))?;
            v.server = Some(server);
            Ok(id_of(&mut w))
        }

        async fn detach(&self, volume: u64) -> Result<u64> {
            let mut w = self.record(CloudCall::Detach(volume));
            let v = w.inv.volumes.iter_mut().find(|v| v.id == volume).ok_or_else(|| not_found("volume", volume))?;
            v.server = None;
            Ok(id_of(&mut w))
        }

        async fn resize(&self, volume: u64, size_gb: u32) -> Result<u64> {
            let mut w = self.record(CloudCall::Resize(volume, size_gb));
            let v = w.inv.volumes.iter_mut().find(|v| v.id == volume).ok_or_else(|| not_found("volume", volume))?;
            v.size_gb = size_gb;
            Ok(id_of(&mut w))
        }

        async fn protect_server(&self, id: u64, on: bool) -> Result<u64> {
            let mut w = self.record(CloudCall::ProtectServer(id, on));
            let s = w.inv.servers.iter_mut().find(|s| s.id == id).ok_or_else(|| not_found("server", id))?;
            s.delete_protected = on;
            Ok(id_of(&mut w))
        }

        async fn protect_volume(&self, id: u64, on: bool) -> Result<u64> {
            let mut w = self.record(CloudCall::ProtectVolume(id, on));
            let v = w.inv.volumes.iter_mut().find(|v| v.id == id).ok_or_else(|| not_found("volume", id))?;
            v.delete_protected = on;
            Ok(id_of(&mut w))
        }

        async fn power_on(&self, id: u64) -> Result<u64> {
            let mut w = self.record(CloudCall::PowerOn(id));
            let s = w.inv.servers.iter_mut().find(|s| s.id == id).ok_or_else(|| not_found("server", id))?;
            s.status = "running".into();
            Ok(id_of(&mut w))
        }

        async fn delete_server(&self, id: u64) -> Result<()> {
            let mut w = self.record(CloudCall::DeleteServer(id));
            let s = w.inv.servers.iter().find(|s| s.id == id).ok_or_else(|| not_found("server", id))?;
            if s.delete_protected {
                return Err(protected("server", id));
            }
            w.inv.servers.retain(|s| s.id != id);
            // Deleting a server detaches its volumes (Hetzner keeps them).
            for v in w.inv.volumes.iter_mut().filter(|v| v.server == Some(id)) {
                v.server = None;
            }
            Ok(())
        }

        async fn delete_volume(&self, id: u64) -> Result<()> {
            let mut w = self.record(CloudCall::DeleteVolume(id));
            let v = w.inv.volumes.iter().find(|v| v.id == id).ok_or_else(|| not_found("volume", id))?;
            if v.delete_protected {
                return Err(protected("volume", id));
            }
            if v.server.is_some() {
                return Err(Error::provider("hetzner delete volume HTTP 423: volume is attached"));
            }
            w.inv.volumes.retain(|v| v.id != id);
            Ok(())
        }

        async fn delete_ssh_key(&self, id: u64) -> Result<()> {
            let mut w = self.record(CloudCall::DeleteSshKey(id));
            if !w.inv.ssh_keys.iter().any(|k| k.id == id) {
                return Err(not_found("ssh key", id));
            }
            w.inv.ssh_keys.retain(|k| k.id != id);
            Ok(())
        }

        async fn wait_action(&self, id: u64, _limit: Duration) -> Result<()> {
            drop(self.record(CloudCall::WaitAction(id)));
            Ok(())
        }

        async fn prices(&self, _server_type: &str, _location: &str) -> Result<Prices> {
            if self.record(CloudCall::Prices).fail_prices {
                return Err(Error::provider("hetzner pricing HTTP 503"));
            }
            Ok(Prices { volume_gb_month: 0.0572, server_month: Some(5.49), ipv4_month: Some(0.5) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_server::{canned, serve};

    /// `arena balance`'s share: the archive's server (+ IPv4) and volumes at the monthly prices
    /// per hour; volumes alone after `archive down`; nothing without an archive; a failed list
    /// is an error, never "no archive".
    #[tokio::test]
    async fn eur_per_hr_prices_the_server_and_its_volumes_per_hour() {
        let n = names();
        let fake = FakeArchiveCloud::new();
        assert_eq!(eur_per_hr(&fake, &n).await.unwrap(), None);
        let key = fake.create_ssh_key(&n.ssh_key(), "ssh-ed25519 AAAA", &n.labels()).await.unwrap();
        let (srv, _) = fake.create_server(&server_body(&n, "cx23", "nbg1", key.id)).await.unwrap();
        fake.create_volume(&volume_body(&n, Tier::Snap, 50, srv.id)).await.unwrap();
        fake.create_volume(&volume_body(&n, Tier::Big, 200, srv.id)).await.unwrap();
        // The fake's prices: cx23 €5.49 + IPv4 €0.50, €0.0572/GB.
        let want = (5.49 + 0.5 + 250.0 * 0.0572) / HOURS_PER_MONTH;
        assert!((eur_per_hr(&fake, &n).await.unwrap().unwrap() - want).abs() < 1e-12);
        fake.delete_server(srv.id).await.unwrap();
        assert!((eur_per_hr(&fake, &n).await.unwrap().unwrap() - 250.0 * 0.0572 / HOURS_PER_MONTH).abs() < 1e-12);
        // Another cohort's archive is not this one's cost.
        assert_eq!(eur_per_hr(&fake, &ArchiveNames::new("other").unwrap()).await.unwrap(), None);
        // `/pricing` down: the list prices, never no figure (orphan volumes priced as such).
        fake.fail_prices(true);
        assert!((eur_per_hr(&fake, &n).await.unwrap().unwrap() - Prices::LIST.monthly_for(0, 250) / HOURS_PER_MONTH).abs() < 1e-12);
        fake.fail_prices(false);
        fake.fail_inventory(Some("hetzner archive list servers HTTP 503"));
        assert!(eur_per_hr(&fake, &n).await.is_err());
    }

    fn names() -> ArchiveNames {
        ArchiveNames::new("arena9").unwrap()
    }

    /// S4: the create bodies, exactly.
    #[test]
    fn create_bodies_carry_only_the_admin_key_the_labels_and_ext4() {
        let s = server_body(&names(), "cx23", "nbg1", 42);
        assert_eq!(
            s,
            json!({"name":"arena9-archive","server_type":"cx23","image":"ubuntu-24.04","location":"nbg1",
                   "ssh_keys":[42],"labels":{"arena-archive":"arena9"},"start_after_create":true,
                   "public_net":{"enable_ipv4":true,"enable_ipv6":true}})
        );
        assert!(s.get("user_data").is_none());
        let v = volume_body(&names(), Tier::Snap, 50, 7);
        assert_eq!(
            v,
            json!({"name":"arena9-archive-snap","size":50,"server":7,"format":"ext4","automount":false,
                   "labels":{"arena-archive":"arena9","arena-archive-tier":"snap"}})
        );
        assert_eq!(volume_body(&names(), Tier::Big, 200, 7)["labels"]["arena-archive-tier"], "big");
    }

    /// S4 over the wire: what the real client sends is the builders' bodies, with the token as
    /// a header, and it reads back the server and its action.
    #[tokio::test]
    async fn the_client_sends_the_bodies_and_reads_the_answers() {
        let srv = serve(vec![
            canned(201, "application/json", r#"{"server":{"id":5,"name":"arena9-archive","status":"initializing","labels":{"arena-archive":"arena9"},"public_net":{"ipv4":{"ip":"192.0.2.9"}},"datacenter":{"location":{"name":"nbg1"}},"server_type":{"name":"cx23"},"protection":{"delete":false}},"action":{"id":77}}"#),
            canned(201, "application/json", r#"{"volume":{"id":8,"name":"arena9-archive-snap","size":50,"server":5,"format":"ext4","linux_device":"/dev/disk/by-id/scsi-0HC_Volume_8","location":{"name":"nbg1"},"labels":{"arena-archive":"arena9","arena-archive-tier":"snap"},"protection":{"delete":false}},"action":{"id":78},"next_actions":[{"id":79}]}"#),
            canned(201, "application/json", r#"{"action":{"id":80,"command":"start_server","status":"running"}}"#),
        ]);
        let c = HetznerArchiveCloud::for_test(&srv.base);
        let (s, a) = c.create_server(&server_body(&names(), "cx23", "nbg1", 42)).await.unwrap();
        assert_eq!((s.id, s.ipv4.as_deref(), s.location.as_str(), a), (5, Some("192.0.2.9"), "nbg1", 77));
        let (v, actions) = c.create_volume(&volume_body(&names(), Tier::Snap, 50, 5)).await.unwrap();
        assert_eq!((v.id, v.tier(), v.server, v.format.as_deref(), actions), (8, Some(Tier::Snap), Some(5), Some("ext4"), vec![78, 79]));
        // A server found off is powered on (w7 live: `up` waited 5 min for SSH, then failed).
        assert_eq!(c.power_on(5).await.unwrap(), 80);
        let reqs = srv.requests.lock().unwrap().clone();
        assert_eq!(reqs, ["POST /servers HTTP/1.1", "POST /volumes HTTP/1.1", "POST /servers/5/actions/poweron HTTP/1.1"]);
        let bodies = srv.bodies.lock().unwrap().clone();
        assert_eq!(serde_json::from_str::<Value>(&bodies[0]).unwrap(), server_body(&names(), "cx23", "nbg1", 42));
        assert_eq!(serde_json::from_str::<Value>(&bodies[1]).unwrap(), volume_body(&names(), Tier::Snap, 50, 5));
        assert!(srv.headers.lock().unwrap()[0].iter().any(|h| h.eq_ignore_ascii_case("authorization: Bearer k")));
    }

    /// The listing pages through, asks by label and (for a prefix) by name, and fails closed on
    /// drift — never "nothing there".
    #[tokio::test]
    async fn inventory_pages_by_label_and_name_and_fails_closed() {
        let empty = |k: &str| canned(200, "application/json", &format!(r#"{{"{k}":[],"meta":{{"pagination":{{"next_page":null}}}}}}"#));
        let srv = serve(vec![
            canned(200, "application/json", r#"{"servers":[{"id":1,"name":"arena9-archive","labels":{"arena-archive":"arena9"}}],"meta":{"pagination":{"next_page":2}}}"#),
            empty("servers"),
            canned(200, "application/json", r#"{"volumes":[{"id":2,"name":"arena9-archive-big","size":200,"server":1,"format":"ext4","labels":{"arena-archive":"arena9","arena-archive-tier":"big"}}]}"#),
            empty("ssh_keys"),
            // by name: the server again (deduplicated), a lookalike snap volume, nothing else
            canned(200, "application/json", r#"{"servers":[{"id":1,"name":"arena9-archive","labels":{"arena-archive":"arena9"}}]}"#),
            canned(200, "application/json", r#"{"volumes":[{"id":3,"name":"arena9-archive-snap","size":10,"server":null,"format":null,"labels":{}}]}"#),
            empty("volumes"),
            empty("ssh_keys"),
        ]);
        let c = HetznerArchiveCloud::for_test(&srv.base);
        let inv = c.inventory(Some("arena9")).await.unwrap();
        assert_eq!(inv.servers.len(), 1);
        assert_eq!(inv.volumes.iter().map(|v| v.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(inv.owned_by(&names()).volumes.len(), 1, "the lookalike isn't ours");
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(reqs[0].starts_with("GET /servers?label_selector=arena-archive%3Darena9&page=1&per_page=50"), "{reqs:?}");
        assert!(reqs[1].contains("page=2"), "{reqs:?}");
        assert!(reqs[4].starts_with("GET /servers?name=arena9-archive&"), "{reqs:?}");
        assert!(reqs[5].starts_with("GET /volumes?name=arena9-archive-snap&"), "{reqs:?}");

        for bad in [r#"{}"#, r#"{"servers":null}"#, r#"{"servers":[{"name":"x"}]}"#, r#"{"servers":[{"id":1,"name":"x","labels":[1]}]}"#] {
            let srv = serve(vec![canned(200, "application/json", bad)]);
            let e = HetznerArchiveCloud::for_test(&srv.base).inventory(None).await.unwrap_err();
            assert!(e.to_string().contains("unexpected"), "{bad}: {e}");
        }
        let srv = serve(vec![canned(401, "application/json", r#"{"error":{"code":"unauthorized"}}"#)]);
        let e = HetznerArchiveCloud::for_test(&srv.base).inventory(None).await.unwrap_err();
        assert_eq!(e.kind(), Some(crate::ProviderErrorKind::Auth), "{e}");
        let srv = serve(vec![canned(200, "application/json", r#"{"servers":[],"meta":{"pagination":{"next_page":1}}}"#)]);
        assert!(HetznerArchiveCloud::for_test(&srv.base).inventory(None).await.unwrap_err().to_string().contains("did not advance"));
    }

    #[tokio::test]
    async fn wait_action_polls_until_success_and_reports_an_error() {
        let srv = serve(vec![
            canned(200, "application/json", r#"{"action":{"id":5,"status":"running"}}"#),
            canned(200, "application/json", r#"{"action":{"id":5,"status":"success"}}"#),
            canned(200, "application/json", r#"{"action":{"id":6,"status":"error","command":"attach_volume","error":{"message":"volume is busy"}}}"#),
        ]);
        let c = HetznerArchiveCloud::for_test(&srv.base);
        c.wait_action(5, Duration::from_secs(5)).await.unwrap();
        let e = c.wait_action(6, Duration::from_secs(5)).await.unwrap_err().to_string();
        assert!(e.contains("attach_volume") && e.contains("volume is busy"), "{e}");
    }

    #[test]
    fn prices_come_from_the_location_and_type() {
        let body = json!({"pricing": {
            "volume": {"price_per_gb_month": {"net": "0.0440", "gross": "0.0524"}},
            "server_types": [{"name": "cx23", "prices": [
                {"location": "fsn1", "price_monthly": {"net": "4.0", "gross": "4.8"}},
                {"location": "nbg1", "price_monthly": {"net": "4.59", "gross": "5.46"}}]}],
            "primary_ips": [{"type": "ipv4", "prices": [{"location": "nbg1", "price_monthly": {"net": "0.50", "gross": "0.60"}}]}]
        }});
        let p = parse_prices(&body, "cx23", "nbg1").unwrap();
        assert_eq!(p, Prices { volume_gb_month: 0.044, server_month: Some(4.59), ipv4_month: Some(0.5) });
        assert!((p.monthly(true, 100) - (4.59 + 0.5 + 4.4)).abs() < 1e-9);
        assert!((p.monthly(false, 10) - 0.44).abs() < 1e-9);
        // A type not sold there is priced from the list, never left out of the total.
        let cax = parse_prices(&body, "cax11", "nbg1").unwrap();
        assert!((cax.monthly_for(2, 0) - 2.0 * (Prices::LIST.server_month.unwrap() + 0.5)).abs() < 1e-9);
        assert_eq!(eur_month(8.624), "€8.62/month");
        assert_eq!(parse_prices(&body, "cax11", "nbg1").unwrap().server_month, None);
        assert!(parse_prices(&json!({}), "cx23", "nbg1").is_err());
    }

    #[test]
    fn only_placement_errors_walk_the_server_type_list() {
        assert!(is_placement_error(&Error::provider("hetzner create server HTTP 412: {\"error\":{\"code\":\"resource_unavailable\"}}")));
        assert!(is_placement_error(&Error::provider("x placement_error y")));
        assert!(!is_placement_error(&Error::provider("hetzner create server HTTP 401 unauthorized")));
        assert!(!is_placement_error(&Error::provider("hetzner create server HTTP 403: limit exceeded")));
    }
}
