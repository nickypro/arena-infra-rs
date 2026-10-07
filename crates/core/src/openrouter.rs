//! OpenRouter **provisioning** API: mint / list / delete the runtime API keys we hand
//! out to the cohort (one per machine), so keys can be generated, rotated, and revoked
//! programmatically — e.g. to recover from a leak.
//!
//! This needs a *provisioning* key (made at openrouter.ai → Settings → Provisioning API
//! Keys), which is distinct from the runtime keys it creates. Auth is `Bearer
//! <provisioning_key>`. The created runtime key's secret is returned **once**, at
//! creation — afterwards only its `hash` (used for delete/patch) and metadata are
//! visible, so we capture the secret immediately and persist it locally.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::{send_json, send_ok, status_error};

const BASE: &str = "https://openrouter.ai/api/v1/keys";

/// A client for the provisioning API.
pub struct OpenRouter {
    provisioning_key: String,
    client: reqwest::Client,
    /// The keys endpoint: [`BASE`], or a loopback test server.
    base: String,
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
        Self { provisioning_key: provisioning_key.into(), client: reqwest::Client::new(), base: BASE.to_string() }
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
        // Status first (crate::http). Not `send_json`: its "isn't JSON" error quotes the
        // start of the body, and a 2xx body here carries the new key's secret.
        let resp = self.auth(self.client.post(&self.base)).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(status_error(status, &text, "openrouter create key"));
        }
        let unreadable = || {
            Error::provider(format!(
                "openrouter create key: HTTP {status} but the response couldn't be read (not shown: \
                 it carries the secret) — the key may have been created: check `arena keys list` / the \
                 OpenRouter dashboard before retrying"
            ))
        };
        let text = resp.text().await.map_err(|_| unreadable())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| unreadable())?;
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
        let v = send_json(self.auth(self.client.get(&self.base)), "openrouter list keys").await?;
        let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        Ok(arr.iter().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect())
    }

    /// Delete the key with `hash` (irreversible — the runtime key stops working).
    pub async fn delete_key(&self, hash: &str) -> Result<()> {
        send_ok(self.auth(self.client.delete(format!("{}/{hash}", self.base))), "openrouter delete key").await
    }

    /// Find an existing key by exact `name` (latest match), or `None`.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<KeyInfo>> {
        Ok(self.list_keys().await?.into_iter().find(|k| k.name.as_deref() == Some(name)))
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

    /// Status first, then decode: a bad provisioning key's 401 is `Auth` whatever its body;
    /// a 2xx create whose body can't be parsed says the key may exist — without quoting the
    /// body, which would carry the secret.
    #[tokio::test]
    async fn error_statuses_are_classified_and_a_bad_create_body_is_never_echoed() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, client, serve};
        let srv = serve(vec![
            canned(401, "text/html", "<html>Unauthorized</html>"),
            canned(200, "application/json", r#"{"key":"sk-or-v1-SECRET","data":{"hash":"h"#), // truncated JSON
            canned(200, "application/json", r#"{"key":"sk-or-v1-SECRET2","data":{"hash":"h2"}}"#),
            canned(404, "application/json", r#"{"error":{"message":"not found"}}"#),
        ]);
        let or = OpenRouter { provisioning_key: "BOGUS".into(), client: client(), base: srv.base.clone() };
        let e = or.list_keys().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        let e = or.create_key("devtest-apple", None).await.unwrap_err().to_string();
        assert!(e.contains("may have been created") && !e.contains("SECRET"), "{e}");
        let k = or.create_key("devtest-apple", Some(5.0)).await.unwrap();
        assert_eq!((k.secret.as_str(), k.hash.as_str()), ("sk-or-v1-SECRET2", "h2"));
        let e = or.delete_key("h2").await.unwrap_err();
        assert!(e.to_string().contains("openrouter delete key HTTP 404"), "{e}");
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
