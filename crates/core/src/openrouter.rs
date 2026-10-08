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

    /// List **every** provisioned key on the account, disabled ones included. The API
    /// returns one page (the most recent 100) per call, so this walks `offset` until a page
    /// comes back empty ([`collect_pages`]). A single page used to be read: on an account
    /// holding more keys (several cohorts + admin keys) a machine's key could sit past it,
    /// and "no key" then meant revoke dropped the CSV row of a key that still worked.
    pub async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        collect_pages(|offset| self.list_page(offset)).await
    }

    /// One page of `GET /keys` from `offset` (disabled keys included: a disabled key still
    /// exists, still carries its machine's name, and revoke/rename must see it). Status
    /// first ([`send_json`]): a bad provisioning key's 401 is `Auth` whatever its body.
    async fn list_page(&self, offset: usize) -> Result<Vec<KeyInfo>> {
        let query = [("include_disabled", "true".to_string()), ("offset", offset.to_string())];
        let v = send_json(self.auth(self.client.get(&self.base)).query(&query), "openrouter list keys").await?;
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

    /// Rename the key with `hash` in place (`PATCH /keys/{hash}` with `{"name": …}` — the
    /// provisioning API's documented update; secret, limit and usage are untouched). How a
    /// machine's key follows a `pods rename`: rotate/revoke find keys by name.
    pub async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
        // Status only ([`send_ok`]): a rename that worked is never reported as failed
        // because of its (unused) body.
        send_ok(
            self.auth(self.client.patch(format!("{}/{hash}", self.base))).json(&rename_body(new_name)),
            "openrouter rename key",
        )
        .await
    }
}

/// Most pages [`collect_pages`] will fetch (100 keys each → 50 000 keys) before giving up:
/// a bound, so an API that ignored `offset` in some new way could never loop forever.
pub const MAX_KEY_PAGES: usize = 500;

/// Gather a paginated key listing: `fetch(offset)` for offset 0, then the running count,
/// until a page is empty. Stops early (successfully) if a page holds nothing new — an API
/// that ignores `offset` would otherwise hand back page 1 forever — and errors after
/// [`MAX_KEY_PAGES`] rather than return a listing that might be missing a key. Any page
/// failing fails the whole listing: a partial list is how a key goes unseen.
pub async fn collect_pages<F, Fut>(mut fetch: F) -> Result<Vec<KeyInfo>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<KeyInfo>>>,
{
    let mut all: Vec<KeyInfo> = Vec::new();
    for _ in 0..MAX_KEY_PAGES {
        let page = fetch(all.len()).await?;
        let fresh: Vec<KeyInfo> = page.into_iter().filter(|k| !all.iter().any(|a| a.hash == k.hash)).collect();
        if fresh.is_empty() {
            return Ok(all);
        }
        all.extend(fresh);
    }
    Err(Error::provider(format!("openrouter list keys: more than {MAX_KEY_PAGES} pages — not a complete listing")))
}

/// The `PATCH /keys/{hash}` body for a rename: only `name`, so nothing else about the key
/// (limit, disabled, reset) changes. Pure, for testing.
pub fn rename_body(new_name: &str) -> serde_json::Value {
    serde_json::json!({ "name": new_name })
}

/// The slice of the provisioning API that `arena keys`, `pods rename` and `terminate
/// --revoke-key` use, as a trait so those flows run against a fake in tests (no network,
/// no real keys).
#[async_trait]
pub trait KeyApi: Send + Sync {
    /// Every key on the account (all pages, disabled ones included).
    async fn list_keys(&self) -> Result<Vec<KeyInfo>>;
    async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey>;
    async fn delete_key(&self, hash: &str) -> Result<()>;
    async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()>;
}

#[async_trait]
impl KeyApi for OpenRouter {
    async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        OpenRouter::list_keys(self).await
    }
    async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey> {
        OpenRouter::create_key(self, name, limit).await
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

    /// The merged listing and rename on the wire: every page is read status-first (lane F)
    /// with `include_disabled` and the running `offset` (lane E's pagination); a failing
    /// later page fails the whole listing, classified by its status; a rename is a
    /// status-only `PATCH` (a 2xx is success whatever its body).
    #[tokio::test]
    async fn paged_listing_and_rename_read_the_status_first() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, client, serve};
        let json = "application/json";
        let srv = serve(vec![
            canned(200, json, r#"{"data":[{"hash":"a","name":"devtest-apple"},{"hash":"b","name":"devtest-bloom","disabled":true}]}"#),
            canned(200, json, r#"{"data":[]}"#),
            canned(200, json, r#"{"data":[{"hash":"a"}]}"#),
            canned(401, "text/html", "<html>Unauthorized</html>"),
            canned(200, "text/plain", "ok (not json)"),
            canned(403, "text/plain", "Forbidden"),
        ]);
        let or = OpenRouter { provisioning_key: "K".into(), client: client(), base: srv.base.clone() };
        let keys = or.list_keys().await.unwrap();
        assert_eq!(keys.iter().map(|k| (k.hash.as_str(), k.disabled)).collect::<Vec<_>>(), [("a", false), ("b", true)]);
        let e = or.list_keys().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "a failed later page fails the listing: {e}");
        or.rename_key("a", "devtest-cloud").await.unwrap();
        let e = or.rename_key("a", "devtest-cloud").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("openrouter rename key HTTP 403"), "{e}");
        assert_eq!(
            *srv.requests.lock().unwrap(),
            [
                "GET /?include_disabled=true&offset=0 HTTP/1.1",
                "GET /?include_disabled=true&offset=2 HTTP/1.1",
                "GET /?include_disabled=true&offset=0 HTTP/1.1",
                "GET /?include_disabled=true&offset=1 HTTP/1.1",
                "PATCH /a HTTP/1.1",
                "PATCH /a HTTP/1.1",
            ]
        );
    }

    #[test]
    fn rename_body_sets_only_the_name() {
        assert_eq!(rename_body("arena9-apple"), serde_json::json!({ "name": "arena9-apple" }));
    }

    fn key(hash: &str) -> KeyInfo {
        KeyInfo { hash: hash.into(), name: Some(format!("n-{hash}")), label: None, disabled: false, limit: None, usage: None }
    }

    /// Run [`collect_pages`] over `pages` (the n-th call gets the n-th page; past the end,
    /// the last page again if `repeat_last`, else empty). The hashes, and the offsets asked.
    async fn paged(pages: Vec<Vec<String>>, repeat_last: bool) -> (Result<Vec<String>>, Vec<usize>) {
        let asked = std::sync::Mutex::new(Vec::new());
        let got = collect_pages(|offset| {
            let n = {
                let mut a = asked.lock().unwrap();
                a.push(offset);
                a.len() - 1
            };
            let page = match pages.get(n) {
                Some(p) => p.clone(),
                None if repeat_last => pages.last().cloned().unwrap_or_default(),
                None => vec![],
            };
            async move { Ok(page.iter().map(|h| key(h)).collect()) }
        })
        .await;
        (got.map(|v| v.into_iter().map(|k| k.hash).collect()), asked.into_inner().unwrap())
    }

    fn page(tag: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{tag}{i}")).collect()
    }

    #[tokio::test]
    async fn listing_walks_every_page_until_an_empty_one() {
        // Two full pages and a short one: all of them, each asked for at the running offset.
        let (got, asked) = paged(vec![page("a", 100), page("b", 100), page("c", 2)], false).await;
        let got = got.unwrap();
        assert_eq!(got.len(), 202);
        assert_eq!((got[0].as_str(), got[201].as_str()), ("a0", "c1"));
        assert_eq!(asked, [0, 100, 200, 202]);
        // An empty account: one call.
        let (got, asked) = paged(vec![], false).await;
        assert_eq!((got.unwrap().len(), asked), (0, vec![0]));
        // An API that ignores offset (page 1 again): stops, without duplicates.
        let (got, asked) = paged(vec![page("x", 2)], true).await;
        assert_eq!((got.unwrap(), asked), (vec!["x0".to_string(), "x1".into()], vec![0, 2]));
    }

    #[tokio::test]
    async fn a_failed_page_fails_the_listing() {
        let calls = std::sync::Mutex::new(0);
        let got = collect_pages(|_| {
            let n = {
                let mut c = calls.lock().unwrap();
                *c += 1;
                *c
            };
            async move {
                if n == 1 {
                    Ok(vec![key("a")])
                } else {
                    Err(Error::provider("openrouter list keys: HTTP 500"))
                }
            }
        })
        .await;
        assert!(got.is_err(), "a partial listing must not pass for a complete one");
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
