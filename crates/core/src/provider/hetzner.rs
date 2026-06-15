//! Hetzner Cloud backend (https://api.hetzner.cloud/v1) — CPU-only VMs.
//!
//! Unlike RunPod/Vast this isn't a GPU container host: it provisions an ordinary
//! cloud VM, so the GPU-centric fields of a [`PodSpec`] don't apply. Hetzner instead
//! takes its sizing (`server_type`), OS (`image`), `location`, and `ssh_keys` from
//! its own `HETZNER_*` config, read once at construction; from a `PodSpec` it uses
//! only the `name`. VMs come up on a real public IP with SSH on `:22`, so they slot
//! straight into the same proxy/`pods up` flow as the GPU providers.
//!
//! Responses are parsed defensively from `serde_json::Value`, as with the others.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::Provider;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

const BASE: &str = "https://api.hetzner.cloud/v1";

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
        // Hetzner's server object doesn't carry an hourly price; leave it None.
        cost_per_hr: None,
        ssh_ip: v
            .get("public_net")
            .and_then(|n| n.get("ipv4"))
            .and_then(|i| i.get("ip"))
            .and_then(Value::as_str)
            .map(String::from),
        // Cloud VMs expose plain SSH on 22.
        ssh_port: Some(22),
    }
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
        let resp = self
            .auth(self.client.get(format!("{}/servers", self.base)))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "hetzner list"));
        }
        let arr = body
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(arr.iter().map(parse_server).collect())
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let mut payload = json!({
            "name": spec.name,
            "server_type": self.opts.server_type,
            "image": self.opts.image,
            "start_after_create": true,
        });
        if let Some(loc) = &self.opts.location {
            payload["location"] = json!(loc);
        }
        if !self.opts.ssh_keys.is_empty() {
            payload["ssh_keys"] = json!(self.opts.ssh_keys);
        }
        let resp = self
            .auth(self.client.post(format!("{}/servers", self.base)).json(&payload))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "hetzner create"));
        }
        // The created server is under `server`; parse what's there (IP may not be
        // populated until it finishes provisioning — `pods up` polls for that).
        Ok(parse_server(body.get("server").unwrap_or(&body)))
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        let resp = self
            .auth(self.client.post(format!("{}/servers/{}/actions/poweroff", self.base, id)))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "hetzner stop"));
        }
        Ok(())
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // Clean OS reboot — preserves the VM and its disk.
        let resp = self
            .auth(self.client.post(format!("{}/servers/{}/actions/reboot", self.base, id)))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "hetzner reboot"));
        }
        Ok(())
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        let resp = self
            .auth(self.client.delete(format!("{}/servers/{}", self.base, id)))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "hetzner terminate"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
