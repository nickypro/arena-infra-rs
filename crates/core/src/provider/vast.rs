//! Vast.ai backend, talking to the v0 REST API (https://console.vast.ai/api/v0).
//!
//! Vast's model differs from RunPod's named-pod model: you don't create a machine
//! by name, you *rent an offer*. So [`create_pod`](VastProvider::create_pod) first
//! searches the marketplace for the cheapest rentable offer that satisfies the
//! [`PodSpec`] (GPU type/count, disk), then rents it via `PUT /asks/{id}/`. The
//! machine name is carried as the instance `label`, keeping the rest of the system
//! provider-agnostic.
//!
//! As with RunPod, responses are parsed defensively from `serde_json::Value` so a
//! schema tweak on Vast's side degrades a field to `None` rather than crashing.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::Provider;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

const BASE: &str = "https://console.vast.ai/api/v0";

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

    /// Search the marketplace and return the cheapest offer matching `spec`, or
    /// `None` if nothing suitable is rentable. Kept separate from `create_pod` so
    /// the selection logic stays testable and a caller could preview offers.
    async fn cheapest_offer(&self, spec: &PodSpec) -> Result<Option<Offer>> {
        // Server-side query: narrow to rentable on-demand offers of the right GPU,
        // cheapest first. We still re-filter client-side (below) because Vast's
        // matching is fuzzy and we'd rather under-trust the server than rent the
        // wrong machine.
        let query = json!({
            "rentable": {"eq": true},
            "num_gpus": {"gte": spec.gpu_count},
            "gpu_name": {"eq": query_gpu_name(&spec.gpu_type)},
            "disk_space": {"gte": spec.disk_gb},
            "type": "on-demand",
            "order": [["dph_total", "asc"]],
            "limit": 64,
        });
        let resp = self
            .auth(self.client.put(format!("{}/search/asks/", self.base)).json(&query))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "vast search"));
        }
        let offers = body
            .get("offers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(select_offer(&offers, spec))
    }
}

/// A rentable marketplace offer, distilled to what we need to choose and rent one.
#[derive(Debug, Clone, PartialEq)]
struct Offer {
    /// The ask id passed to `PUT /asks/{id}/`.
    id: u64,
    gpu_name: String,
    num_gpus: u32,
    disk_space: f64,
    dph_total: f64,
}

fn parse_offer(v: &Value) -> Option<Offer> {
    Some(Offer {
        id: v.get("id").and_then(Value::as_u64)?,
        gpu_name: v.get("gpu_name").and_then(Value::as_str).unwrap_or("").to_string(),
        num_gpus: v.get("num_gpus").and_then(Value::as_u64).unwrap_or(0) as u32,
        disk_space: v.get("disk_space").and_then(Value::as_f64).unwrap_or(0.0),
        dph_total: v.get("dph_total").and_then(Value::as_f64).unwrap_or(f64::INFINITY),
    })
}

/// Client-side re-filter + selection: among offers that actually satisfy the spec,
/// return the cheapest. Pure, so it's unit-tested without touching the network.
fn select_offer(offers: &[Value], spec: &PodSpec) -> Option<Offer> {
    let want = normalize_gpu(&spec.gpu_type);
    offers
        .iter()
        .filter_map(parse_offer)
        .filter(|o| {
            o.num_gpus >= spec.gpu_count
                && o.disk_space + 0.5 >= spec.disk_gb as f64
                && normalize_gpu(&o.gpu_name).contains(&want)
        })
        .min_by(|a, b| a.dph_total.total_cmp(&b.dph_total))
}

/// Normalize a GPU name for tolerant comparison: drop a leading vendor word,
/// lowercase, and treat `_` and `-` as spaces. So RunPod's "NVIDIA RTX A4000",
/// Vast's "RTX A4000", and a query's "RTX_A4000" all compare equal-ish.
fn normalize_gpu(name: &str) -> String {
    let lower = name.to_lowercase().replace(['_', '-'], " ");
    let lower = lower.trim();
    let stripped = lower
        .strip_prefix("nvidia ")
        .or_else(|| lower.strip_prefix("amd "))
        .unwrap_or(lower);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The form Vast's search expects for `gpu_name`: vendor word dropped, spaces ->
/// `_`, but original casing preserved (Vast uses "RTX_A4000", not "Rtx_A4000").
/// e.g. "NVIDIA RTX A4000" -> "RTX_A4000".
fn query_gpu_name(name: &str) -> String {
    let trimmed = name.trim();
    // Strip a leading vendor word case-insensitively, keeping the remainder's case.
    let stripped = ["NVIDIA ", "nvidia ", "AMD ", "amd "]
        .iter()
        .find_map(|p| trimmed.strip_prefix(p))
        .unwrap_or(trimmed);
    stripped.split_whitespace().collect::<Vec<_>>().join("_")
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
        cost_per_hr: v.get("dph_total").and_then(Value::as_f64),
        ssh_ip: v.get("ssh_host").and_then(Value::as_str).map(String::from),
        ssh_port: v.get("ssh_port").and_then(Value::as_u64).map(|n| n as u16),
    }
}

#[async_trait]
impl Provider for VastProvider {
    fn name(&self) -> &'static str {
        "vast"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        format!(
            "cheapest rentable {} x{} offer (disk >= {}GB), image {}",
            spec.gpu_type, spec.gpu_count, spec.disk_gb, spec.image
        )
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        let resp = self
            .auth(self.client.get(format!("{}/instances/", self.base)))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "vast list"));
        }
        let arr = body
            .get("instances")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| body.as_array().cloned())
            .unwrap_or_default();
        Ok(arr.iter().map(parse_instance).collect())
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let offer = self.cheapest_offer(spec).await?.ok_or_else(|| {
            // No matching offer == the marketplace has no capacity for this spec.
            Error::capacity(format!(
                "vast: no rentable offer matching {} x{} (disk >= {}GB)",
                spec.gpu_type, spec.gpu_count, spec.disk_gb
            ))
        })?;

        let env: serde_json::Map<String, Value> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let payload = json!({
            "client_id": "me",
            "image": spec.image,
            "disk": spec.disk_gb,
            "label": spec.name,
            "runtype": "ssh",
            "env": env,
        });
        let resp = self
            .auth(
                self.client
                    .put(format!("{}/asks/{}/", self.base, offer.id))
                    .json(&payload),
            )
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "vast create"));
        }
        // Vast returns {"success": true, "new_contract": <instance id>}; the full
        // instance object isn't echoed, so synthesize the Pod from what we know.
        if body.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(Error::provider(format!("vast create rejected: {body}")));
        }
        let id = body
            .get("new_contract")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default();
        Ok(Pod {
            id,
            name: spec.name.clone(),
            provider: "vast".into(),
            status: "CREATING".into(),
            gpu_type: Some(offer.gpu_name),
            cost_per_hr: Some(offer.dph_total),
            ssh_ip: None,
            ssh_port: None,
        })
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        // Vast stops an instance by setting its desired state.
        let resp = self
            .auth(
                self.client
                    .put(format!("{}/instances/{}/", self.base, id))
                    .json(&json!({"state": "stopped"})),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "vast stop"));
        }
        Ok(())
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // Vast has no single reboot endpoint; a restart is stop then start. Vast keeps
        // the stopped instance, so this preserves it (just cycles the container).
        for state in ["stopped", "running"] {
            let resp = self
                .auth(
                    self.client
                        .put(format!("{}/instances/{}/", self.base, id))
                        .json(&json!({ "state": state })),
                )
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                return Err(Error::provider_http(status, &body, "vast restart"));
            }
        }
        Ok(())
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        let resp = self
            .auth(self.client.delete(format!("{}/instances/{}/", self.base, id)))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "vast terminate"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn normalizes_gpu_names_across_providers() {
        assert_eq!(normalize_gpu("NVIDIA RTX A4000"), "rtx a4000");
        assert_eq!(normalize_gpu("RTX A4000"), "rtx a4000");
        assert_eq!(normalize_gpu("RTX_A4000"), "rtx a4000");
        assert_eq!(query_gpu_name("NVIDIA RTX A4000"), "RTX_A4000");
    }

    #[test]
    fn selects_cheapest_matching_offer() {
        let offers = vec![
            json!({"id": 1, "gpu_name": "RTX A4000", "num_gpus": 1, "disk_space": 200.0, "dph_total": 0.30}),
            json!({"id": 2, "gpu_name": "RTX A4000", "num_gpus": 1, "disk_space": 200.0, "dph_total": 0.20}),
            json!({"id": 3, "gpu_name": "RTX 4090",  "num_gpus": 1, "disk_space": 200.0, "dph_total": 0.05}),
        ];
        let got = select_offer(&offers, &spec("NVIDIA RTX A4000", 1, 100));
        assert_eq!(got.map(|o| o.id), Some(2)); // cheapest of the A4000s, not the 4090
    }

    #[test]
    fn rejects_offers_that_miss_count_or_disk() {
        let offers = vec![
            json!({"id": 1, "gpu_name": "RTX A4000", "num_gpus": 1, "disk_space": 200.0, "dph_total": 0.20}),
            json!({"id": 2, "gpu_name": "RTX A4000", "num_gpus": 4, "disk_space": 50.0,  "dph_total": 0.10}),
        ];
        // Want 2 GPUs and 100GB disk: id=1 has too few GPUs, id=2 too little disk.
        assert_eq!(select_offer(&offers, &spec("NVIDIA RTX A4000", 2, 100)), None);
    }

    #[test]
    fn parses_instance_with_label_and_ssh() {
        let v = json!({
            "id": 12345,
            "label": "arena8-apple",
            "actual_status": "running",
            "gpu_name": "RTX A4000",
            "dph_total": 0.22,
            "ssh_host": "ssh4.vast.ai",
            "ssh_port": 31000
        });
        let p = parse_instance(&v);
        assert_eq!(p.name, "arena8-apple");
        assert_eq!(p.id, "12345");
        assert_eq!(p.status, "RUNNING");
        assert_eq!(p.ssh_ip.as_deref(), Some("ssh4.vast.ai"));
        assert_eq!(p.ssh_port, Some(31000));
    }

    #[test]
    fn parses_instance_without_label_falls_back_to_id() {
        let v = json!({"id": 999, "actual_status": "exited"});
        let p = parse_instance(&v);
        assert_eq!(p.name, "vast-999");
        assert_eq!(p.status, "EXITED");
    }
}
