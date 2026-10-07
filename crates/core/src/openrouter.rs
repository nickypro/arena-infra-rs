//! OpenRouter **provisioning** API: mint / list / delete the runtime API keys we hand
//! out to the cohort (one per machine), so keys can be generated, rotated, and revoked
//! programmatically — e.g. to recover from a leak.
//!
//! This needs a *provisioning* key (made at openrouter.ai → Settings → Provisioning API
//! Keys), which is distinct from the runtime keys it creates. Auth is `Bearer
//! <provisioning_key>`. The created runtime key's secret is returned **once**, at
//! creation — afterwards only its `hash` (used for delete/patch) and metadata are
//! visible, so we capture the secret immediately and persist it locally.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{Error, Result};

const BASE: &str = "https://openrouter.ai/api/v1/keys";

/// A client for the provisioning API.
pub struct OpenRouter {
    provisioning_key: String,
    client: reqwest::Client,
}

/// A freshly created runtime key — the only time the `secret` is available.
#[derive(Debug, Clone)]
pub struct CreatedKey {
    pub secret: String,
    pub hash: String,
    pub name: String,
}

/// Metadata for an existing provisioned key (no secret).
#[derive(Debug, Clone, Deserialize)]
pub struct KeyInfo {
    pub hash: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub usage: Option<f64>,
}

/// The runtime-key name we give a machine, so keys are findable for rotate/revoke: the
/// machine's canonical pod name. Normally `<prefix>-<machine>` (prefix added once), but an
/// absolute (`@name`) list entry keeps its bare name — so the key label matches the pod,
/// not a phantom `<prefix>-james-gpu`. Idempotent on already-qualified names.
pub fn key_name(prefix: &str, candidates: &[String], machine: &str) -> String {
    crate::naming::canonical_name(prefix, candidates, machine)
}

impl OpenRouter {
    pub fn new(provisioning_key: impl Into<String>) -> Self {
        Self { provisioning_key: provisioning_key.into(), client: reqwest::Client::new() }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.provisioning_key)
    }

    /// Create a runtime key named `name` with an optional USD credit `limit`.
    pub async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey> {
        let mut body = serde_json::json!({ "name": name });
        if let Some(l) = limit {
            body["limit"] = serde_json::json!(l);
        }
        let resp = self.auth(self.client.post(BASE)).json(&body).send().await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &v, "openrouter create key"));
        }
        // The secret is the top-level `key`; metadata is under `data`.
        let secret = v
            .get("key")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::provider("openrouter create: response had no `key`"))?
            .to_string();
        let hash = v
            .get("data")
            .and_then(|d| d.get("hash"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(CreatedKey { secret, hash, name: name.to_string() })
    }

    /// List provisioned keys (single page — fine for a cohort; the API defaults to ~100).
    pub async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        let resp = self.auth(self.client.get(BASE)).send().await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(Error::provider_http(status, &v, "openrouter list keys"));
        }
        let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        Ok(arr.iter().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect())
    }

    /// Delete the key with `hash` (irreversible — the runtime key stops working).
    pub async fn delete_key(&self, hash: &str) -> Result<()> {
        let resp = self.auth(self.client.delete(format!("{BASE}/{hash}"))).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::provider_http(status, &body, "openrouter delete key"));
        }
        Ok(())
    }

    /// Find an existing key by exact `name` (latest match), or `None`.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<KeyInfo>> {
        Ok(self.list_keys().await?.into_iter().find(|k| k.name.as_deref() == Some(name)))
    }

    /// Rename the key with `hash` in place (`PATCH /keys/{hash}` with `{"name": …}` — the
    /// provisioning API's documented update; secret, limit and usage are untouched). How a
    /// machine's key follows a `pods rename`: rotate/revoke find keys by name.
    pub async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
        let resp = self.auth(self.client.patch(format!("{BASE}/{hash}"))).json(&rename_body(new_name)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::provider_http(status, &body, "openrouter rename key"));
        }
        Ok(())
    }
}

/// The `PATCH /keys/{hash}` body for a rename: only `name`, so nothing else about the key
/// (limit, disabled, reset) changes. Pure, for testing.
pub fn rename_body(new_name: &str) -> serde_json::Value {
    serde_json::json!({ "name": new_name })
}

/// The slice of the provisioning API that `pods rename` / `terminate --revoke-key` use, as
/// a trait so those flows run against a fake in tests (no network, no real keys).
#[async_trait]
pub trait KeyApi: Send + Sync {
    async fn list_keys(&self) -> Result<Vec<KeyInfo>>;
    async fn delete_key(&self, hash: &str) -> Result<()>;
    async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()>;
}

#[async_trait]
impl KeyApi for OpenRouter {
    async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        OpenRouter::list_keys(self).await
    }
    async fn delete_key(&self, hash: &str) -> Result<()> {
        OpenRouter::delete_key(self, hash).await
    }
    async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
        OpenRouter::rename_key(self, hash, new_name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_name_adds_prefix_once() {
        let cands = vec!["nova".to_string(), "@james-gpu".into()];
        assert_eq!(key_name("arena8", &cands, "nova"), "arena8-nova");
        assert_eq!(key_name("arena8", &cands, "arena8-nova"), "arena8-nova");
        // absolute machine keeps its bare label (matches the pod name)
        assert_eq!(key_name("arena8", &cands, "james-gpu"), "james-gpu");
    }

    #[test]
    fn rename_body_sets_only_the_name() {
        assert_eq!(rename_body("arena9-apple"), serde_json::json!({ "name": "arena9-apple" }));
    }

    #[test]
    fn key_info_parses_partial_json() {
        let v = serde_json::json!({ "hash": "abc", "name": "arena8-nova", "limit": 5.0 });
        let k: KeyInfo = serde_json::from_value(v).unwrap();
        assert_eq!(k.hash, "abc");
        assert_eq!(k.name.as_deref(), Some("arena8-nova"));
        assert_eq!(k.limit, Some(5.0));
        assert!(!k.disabled); // defaulted
    }
}
