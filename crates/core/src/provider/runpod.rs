//! RunPod backend, talking to the REST API (https://rest.runpod.io/v1).
//!
//! Responses are parsed defensively from `serde_json::Value` so a schema tweak on
//! RunPod's side degrades a field to `None` rather than crashing the tool.

use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::Provider;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

const BASE: &str = "https://rest.runpod.io/v1";

pub struct RunpodProvider {
    api_key: String,
    client: Client,
}

impl RunpodProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: Client::new(),
        }
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.api_key)
    }
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
        cost_per_hr: v.get("costPerHr").and_then(Value::as_f64),
        ssh_ip,
        ssh_port,
    }
}

#[async_trait]
impl Provider for RunpodProvider {
    fn name(&self) -> &'static str {
        "runpod"
    }

    fn describe(&self, spec: &PodSpec) -> String {
        let volume = if spec.volume_gb > 0 {
            format!(", volume {}GB", spec.volume_gb)
        } else {
            String::new()
        };
        format!(
            "{} x{}, {}, disk {}GB{volume}, image {}",
            spec.gpu_type, spec.gpu_count, spec.cloud_type, spec.disk_gb, spec.image
        )
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        let resp = self.auth(self.client.get(format!("{BASE}/pods"))).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "list pods"));
        }
        let arr = body
            .as_array()
            .cloned()
            .or_else(|| body.get("pods").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        Ok(arr.iter().map(parse_pod).collect())
    }

    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let env: serde_json::Map<String, Value> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let ports: Vec<String> = spec.ports.split(',').map(|s| s.trim().to_string()).collect();
        let payload = json!({
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
        let resp = self
            .auth(self.client.post(format!("{BASE}/pods")).json(&payload))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &body, "create pod"));
        }
        Ok(parse_pod(&body))
    }

    async fn stop_pod(&self, id: &str) -> Result<()> {
        let resp = self
            .auth(self.client.post(format!("{BASE}/pods/{id}/stop")))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "stop pod"));
        }
        Ok(())
    }

    async fn restart_pod(&self, id: &str) -> Result<()> {
        // Dedicated restart endpoint: restarts the container in place (keeps the pod
        // and its disk), unlike stop/start which deallocates.
        let resp = self
            .auth(self.client.post(format!("{BASE}/pods/{id}/restart")))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "restart pod"));
        }
        Ok(())
    }

    async fn terminate_pod(&self, id: &str) -> Result<()> {
        let resp = self
            .auth(self.client.delete(format!("{BASE}/pods/{id}")))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            return Err(Error::provider_http(status, &body, "terminate pod"));
        }
        Ok(())
    }
}

/// One entry from RunPod's GPU catalog (its GraphQL `gpuTypes`).
#[derive(Debug, Clone)]
pub struct GpuType {
    /// The exact name to pass to `--gpu` (e.g. "NVIDIA A100 80GB PCIe").
    pub id: String,
    /// Short display name (e.g. "A100 PCIe").
    pub display_name: String,
    pub memory_gb: u32,
}

/// Fetch RunPod's full GPU catalog via GraphQL (the REST v1 API has no gpu-types route).
/// This is the authoritative, live list of `--gpu` names — including ones the local
/// preset table doesn't alias.
pub async fn fetch_gpu_types(api_key: &str) -> Result<Vec<GpuType>> {
    let client = Client::new();
    let url = format!("https://api.runpod.io/graphql?api_key={api_key}");
    let q = json!({ "query": "{ gpuTypes { id displayName memoryInGb } }" });
    let resp = client.post(&url).json(&q).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        return Err(Error::provider_http(status, &v, "fetch gpu types"));
    }
    let arr = v
        .get("data")
        .and_then(|d| d.get("gpuTypes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|x| {
            Some(GpuType {
                id: x.get("id")?.as_str()?.to_string(),
                display_name: x.get("displayName").and_then(Value::as_str).unwrap_or("").to_string(),
                memory_gb: x.get("memoryInGb").and_then(Value::as_u64).unwrap_or(0) as u32,
            })
        })
        .collect())
}
