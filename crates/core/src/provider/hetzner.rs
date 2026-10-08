//! Hetzner Cloud backend (https://api.hetzner.cloud/v1) — CPU-only VMs.
//!
//! Unlike RunPod/Vast this isn't a GPU container host: it provisions an ordinary
//! cloud VM, so the GPU-centric fields of a [`PodSpec`] don't apply. Hetzner instead
//! takes its sizing (`server_type`), OS (`image`), `location`, and `ssh_keys` from
//! its own `HETZNER_*` config, read once at construction; from a `PodSpec` it uses
//! only the `name`. VMs come up on a real public IP with SSH on `:22`, so they slot
//! straight into the same proxy/`pods up` flow as the GPU providers.
//!
//! `--api-json` merges into the create body ([`create_payload`]) — `labels`, `user_data`
//! (cloud-init), `firewalls`, `networks`… — minus the fields this module sets ([`MANAGED`]).
//!
//! Responses are parsed defensively from `serde_json::Value`, as with the others.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::Provider;
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
        }
    }
}

pub struct HetznerProvider {
    api_key: String,
    client: Client,
    base: String,
    opts: HetznerOpts,
}

impl HetznerProvider {
    pub fn new(api_key: impl Into<String>, opts: HetznerOpts) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
            base: BASE.to_string(),
            opts,
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

    /// POST a power action (`reset`/`poweron`/`reboot`/`poweroff`) to a server.
    async fn power_action(&self, id: &str, action: &str) -> Result<()> {
        send_ok(
            self.auth(self.client.post(format!("{}/servers/{}/actions/{}", self.base, id, action))),
            &format!("hetzner {action}"),
        )
        .await
    }
}

/// The create-body fields this backend sets itself, refused in `--api-json` /
/// `CREATE_EXTRA_JSON` (Hetzner takes the VM's shape from its `HETZNER_*` config).
const MANAGED: &[Managed] = &[
    managed(&["name"], "the machine name"),
    managed(&["server_type"], "HETZNER_SERVER_TYPE"),
    managed(&["image"], "HETZNER_IMAGE"),
    managed(&["location"], "HETZNER_LOCATION"),
    managed(&["ssh_keys"], "HETZNER_SSH_KEY (the cohort key)"),
    managed(&["start_after_create"], "always on"),
];

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
        "server_type": opts.server_type,
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
    }
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
    let servers = body.get("servers").and_then(Value::as_array).ok_or_else(|| {
        Error::provider(format!(
            "hetzner list: unexpected response shape (no servers array): {}",
            super::body_excerpt(body)
        ))
    })?;
    let next = body
        .get("meta")
        .and_then(|m| m.get("pagination"))
        .and_then(|p| p.get("next_page"))
        .and_then(Value::as_u64);
    Ok((servers, next))
}

#[async_trait]
impl Provider for HetznerProvider {
    fn name(&self) -> &'static str {
        "hetzner"
    }

    fn describe(&self, _spec: &PodSpec) -> String {
        // CPU VM: describe what we actually send (server type/image/location), not the
        // GPU fields the spec carries and we ignore.
        let loc = self
            .opts
            .location
            .as_deref()
            .map(|l| format!(", location {l}"))
            .unwrap_or_default();
        format!("{} CPU VM, image {}{}", self.opts.server_type, self.opts.image, loc)
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
            out.extend(servers.iter().map(parse_server));
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
        let body = send_json(self.auth(self.client.post(format!("{}/servers", self.base)).json(&payload)), "hetzner create").await?;
        // The created server is under `server`; parse what's there (IP may not be
        // populated until it finishes provisioning — `pods up` polls for that).
        Ok(parse_server(body.get("server").unwrap_or(&body)))
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        self.power_action(id, "poweroff").await
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
        let p = HetznerProvider { api_key: "BOGUS".into(), client: client(), base: srv.base.clone(), opts: HetznerOpts::default() };
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
}
