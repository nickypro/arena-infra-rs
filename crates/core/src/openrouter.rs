//! OpenRouter **provisioning** API: mint / list / delete the runtime API keys we hand
//! out to the cohort (one per machine), so keys can be generated, rotated, and revoked
//! programmatically — e.g. to recover from a leak — read what each has spent, and raise a
//! key's cap or pause it in place ([`OpenRouter::update_key`], planned by [`plan_cap`]).
//!
//! This needs a *provisioning* key — OpenRouter now calls it a **management** key
//! (openrouter.ai → Settings → Management Keys) — which is distinct from the runtime keys
//! it creates. Auth is `Bearer <provisioning_key>`. The created runtime key's secret is
//! returned **once**, at creation — afterwards only its `hash` (used for delete/patch) and
//! metadata are visible, so we capture the secret immediately and persist it locally.
//!
//! **Workspaces.** A management key is the whole account's — in an organization, every
//! workspace's — never one workspace's. Without `workspace_id` the API lists and creates keys
//! in the Default workspace only. `OPENROUTER_WORKSPACE` (a workspace's id or slug) scopes
//! every call: the listing asks for that workspace and keeps only keys that say they're in
//! it, a create puts the key there, and a delete or rename first reads the key and refuses
//! one from elsewhere. So a cohort (or a sandbox) holding the organization's key never
//! touches another workspace's keys. A workspace that can't be resolved fails the call; it
//! never falls back to the Default one.
//!
//! **Usage.** Each listed key carries what it has spent (all told, and this UTC day, week
//! and month) and its cap; `GET /credits` the account's credit, bought and used. The pure
//! helpers at the end ([`KeyUsage`], [`UsageView`]) turn those into the dashboard's `AI$`
//! column, its detail line and its part of the summary bar.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::{send_json, send_ok, status_error};

const ROOT: &str = "https://openrouter.ai/api/v1";

/// A client for the provisioning API.
pub struct OpenRouter {
    provisioning_key: String,
    client: reqwest::Client,
    /// The API root: [`ROOT`], or a loopback test server.
    root: String,
    /// `OPENROUTER_WORKSPACE` as configured (an id or a slug); `None`: the Default
    /// workspace, as before workspaces.
    workspace: Option<String>,
    /// The workspace's id once a slug was looked up ([`OpenRouter::workspace_id`]).
    resolved: Mutex<Option<String>>,
}

/// A freshly created runtime key — the only time the `secret` is available.
#[derive(Debug, Clone)]
pub struct CreatedKey {
    pub secret: String,
    pub hash: String,
    pub name: String,
}

/// Metadata for an existing provisioned key (no secret).
#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Spent over the key's whole life, USD.
    #[serde(default)]
    pub usage: Option<f64>,
    /// What's left of `limit` in the current reset period, as OpenRouter counts it.
    #[serde(default)]
    pub limit_remaining: Option<f64>,
    /// `daily`, `weekly` or `monthly`: when the cap starts over (none: never).
    #[serde(default)]
    pub limit_reset: Option<String>,
    /// Spent this UTC day, week and month.
    #[serde(default)]
    pub usage_daily: Option<f64>,
    #[serde(default)]
    pub usage_weekly: Option<f64>,
    #[serde(default)]
    pub usage_monthly: Option<f64>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The workspace the key is in.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// The account's credit (`GET /credits`): an organization's whole, whatever the workspace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Credits {
    pub total: f64,
    pub used: f64,
}

impl Credits {
    pub fn left(&self) -> f64 {
        self.total - self.used
    }
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
        Self {
            provisioning_key: provisioning_key.into(),
            client: reqwest::Client::new(),
            root: ROOT.to_string(),
            workspace: None,
            resolved: Mutex::new(None),
        }
    }

    /// Scope every call to `workspace`: an id or a slug (blank: none, the Default workspace).
    pub fn with_workspace(mut self, workspace: Option<&str>) -> Self {
        self.workspace = workspace.map(str::trim).filter(|w| !w.is_empty()).map(String::from);
        self
    }

    /// The client `cfg` asks for: `OPENROUTER_PROVISIONING_KEY` (unset or empty: `None`),
    /// scoped to `OPENROUTER_WORKSPACE`.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        cfg.get("OPENROUTER_PROVISIONING_KEY")
            .filter(|s| !s.is_empty())
            .map(|k| Self::new(k).with_workspace(cfg.get("OPENROUTER_WORKSPACE")))
    }

    /// `OPENROUTER_WORKSPACE` as configured, for messages.
    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.provisioning_key)
    }

    fn keys_url(&self) -> String {
        format!("{}/keys", self.root)
    }

    /// The configured workspace's id: `None` when none is configured (the Default
    /// workspace). Otherwise looked up once (`GET /workspaces/{ref}`), and only accepted if the
    /// answer is the workspace asked for ([`resolved_workspace`]): a mistyped id fails here,
    /// rather than every create landing in the Default workspace. An error, never `None`,
    /// when it can't be resolved: an unscoped call is what the setting is there to prevent.
    pub async fn workspace_id(&self) -> Result<Option<String>> {
        let Some(ws) = &self.workspace else { return Ok(None) };
        let wanted = workspace_ref(ws)?;
        if let Some(id) = self.resolved.lock().unwrap().clone() {
            return Ok(Some(id));
        }
        let url = format!("{}/workspaces/{ws}", self.root);
        let v = send_json(self.auth(self.client.get(url)), &format!("openrouter workspace `{ws}`")).await?;
        let id = resolved_workspace(&v, ws, &wanted)?;
        *self.resolved.lock().unwrap() = Some(id.clone());
        Ok(Some(id))
    }

    /// Create a runtime key named `name` with an optional USD credit `limit` (in the
    /// configured workspace, if any). With a workspace, the answer must say the key is in it:
    /// a key the API put elsewhere is one the scoped listing never shows again (no rotate, no
    /// revoke), so the create fails, naming the key's full hash to delete by hand. Nothing is
    /// deleted here: the answer is the only evidence of which key was made, and a key named
    /// the same could be another workspace's (review round 2, codex #1).
    pub async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey> {
        let ws = self.workspace_id().await?;
        let body = create_body(name, limit, ws.as_deref());
        // Status first (crate::http). Not `send_json`: its "isn't JSON" error quotes the
        // start of the body, and a 2xx body here carries the new key's secret.
        let resp = self.auth(self.client.post(self.keys_url())).json(&body).send().await?;
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
        let data = v.get("data");
        let field = |k: &str| data.and_then(|d| d.get(k)).and_then(|s| s.as_str());
        let hash = field("hash").unwrap_or_default().to_string();
        if let Some(ws) = ws.as_deref() {
            let mut landed = field("workspace_id").map(String::from);
            // An answer that doesn't say: ask where the key is (as a revoke would) before
            // calling it misplaced — otherwise every create would fail if the API stopped
            // echoing workspace_id (review round 3).
            if landed.is_none() && valid_hash(&hash).is_ok() {
                if let Ok(v) = send_json(self.auth(self.client.get(format!("{}/{hash}", self.keys_url()))), "openrouter get key").await {
                    let d = v.get("data");
                    if d.and_then(|d| d.get("hash")).and_then(|s| s.as_str()) == Some(hash.as_str()) {
                        landed = d.and_then(|d| d.get("workspace_id")).and_then(|s| s.as_str()).map(String::from);
                    }
                }
            }
            let landed = landed.as_deref();
            if !landed.is_some_and(|l| l.eq_ignore_ascii_case(ws)) {
                return Err(Error::provider(format!(
                    "openrouter create key: the new key `{name}` landed in workspace {}, not OPENROUTER_WORKSPACE ({}) — \
                     not used; delete it on openrouter.ai (hash {})",
                    landed.unwrap_or("(not said)"),
                    self.workspace.as_deref().unwrap_or_default(),
                    if hash.is_empty() { "not given" } else { hash.as_str() },
                )));
            }
        }
        Ok(CreatedKey { secret, hash, name: name.to_string() })
    }

    /// List **every** provisioned key in the workspace (the Default one when none is
    /// configured), disabled ones included. The API returns one page (the most recent 100)
    /// per call, so this walks `offset` until a page comes back empty ([`collect_pages`]).
    /// A single page used to be read: on an account holding more keys (several cohorts +
    /// admin keys) a machine's key could sit past it, and "no key" then meant revoke dropped
    /// the CSV row of a key that still worked. With a workspace, keys that say they're in
    /// another one are dropped, and a key that doesn't say fails the listing ([`in_workspace`]).
    pub async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        let ws = self.workspace_id().await?;
        let all = collect_pages(|offset| self.list_page(offset, ws.as_deref())).await?;
        in_workspace(all, ws.as_deref())
    }

    /// One page of `GET /keys` from `offset` (disabled keys included: a disabled key still
    /// exists, still carries its machine's name, and revoke/rename must see it), in
    /// workspace `ws` if given. Status first ([`send_json`]): a bad provisioning key's 401 is
    /// `Auth` whatever its body. Only a `data` array counts as a page — [`collect_pages`]
    /// reads an empty page as "end of listing", so an empty 2xx body (`send_json` gives
    /// `Null` for 200/204 with nothing) or a JSON body without `data` must fail, not pass a
    /// truncated listing off as whole.
    async fn list_page(&self, offset: usize, ws: Option<&str>) -> Result<Vec<KeyInfo>> {
        let mut query = vec![("include_disabled", "true".to_string()), ("offset", offset.to_string())];
        if let Some(id) = ws {
            query.push(("workspace_id", id.to_string()));
        }
        let v = send_json(self.auth(self.client.get(self.keys_url())).query(&query), "openrouter list keys").await?;
        let arr = page_data(&v)?;
        // Scoped, a key that can't be read fails the page: dropped, it would read as "no key"
        // (unscoped, as before: skipped).
        let mut keys = Vec::with_capacity(arr.len());
        for k in arr {
            match serde_json::from_value(k.clone()) {
                Ok(info) => keys.push(info),
                Err(_) if ws.is_none() => {}
                Err(e) => return Err(Error::provider(format!("openrouter list keys: a key couldn't be read ({e})"))),
            }
        }
        Ok(keys)
    }

    /// Refuse to change a key outside the configured workspace: read it (`GET /keys/{hash}`),
    /// check the answer is that key, and compare its workspace. Nothing to check without a
    /// workspace.
    async fn ensure_in_workspace(&self, hash: &str, what: &str) -> Result<()> {
        let Some(ws) = self.workspace_id().await? else { return Ok(()) };
        let v = send_json(self.auth(self.client.get(format!("{}/{hash}", self.keys_url()))), &format!("openrouter {what}"))
            .await?;
        let data = v.get("data");
        let theirs = data.and_then(|d| d.get("workspace_id")).and_then(|s| s.as_str());
        let same_key = data.and_then(|d| d.get("hash")).and_then(|s| s.as_str()) == Some(hash);
        if same_key && theirs.is_some_and(|t| t.eq_ignore_ascii_case(&ws)) {
            return Ok(());
        }
        Err(Error::provider(format!(
            "openrouter {what}: refused — key {} is in workspace {}, not OPENROUTER_WORKSPACE ({})",
            short_hash(hash),
            if same_key { theirs.unwrap_or("(not said)") } else { "(the answer was for another key)" },
            self.workspace.as_deref().unwrap_or_default(),
        )))
    }

    /// Delete the key with `hash` (irreversible — the runtime key stops working). With a
    /// workspace, only a key in it.
    pub async fn delete_key(&self, hash: &str) -> Result<()> {
        valid_hash(hash)?;
        self.ensure_in_workspace(hash, "delete key").await?;
        send_ok(self.auth(self.client.delete(format!("{}/{hash}", self.keys_url()))), "openrouter delete key").await
    }

    /// Find an existing key by exact `name` (latest match), or `None`.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<KeyInfo>> {
        Ok(self.list_keys().await?.into_iter().find(|k| k.name.as_deref() == Some(name)))
    }

    /// Rename the key with `hash` in place (`PATCH /keys/{hash}` with `{"name": …}` — the
    /// provisioning API's documented update; secret, limit and usage are untouched). How a
    /// machine's key follows a `pods rename`: rotate/revoke find keys by name. With a
    /// workspace, only a key in it.
    pub async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
        valid_hash(hash)?;
        self.ensure_in_workspace(hash, "rename key").await?;
        // Status only ([`send_ok`]): a rename that worked is never reported as failed
        // because of its (unused) body.
        send_ok(
            self.auth(self.client.patch(format!("{}/{hash}", self.keys_url()))).json(&rename_body(new_name)),
            "openrouter rename key",
        )
        .await
    }

    /// Change the key with `hash` in place (`PATCH /keys/{hash}`): its cap, when the cap starts
    /// over, whether it's disabled — only the fields `update` gives ([`update_body`]; never the
    /// name, never `workspace_id`, which on this call *moves* a key). How `keys limit|pause|
    /// resume` and the dashboard raise a cap or pause a key; the secret and the usage are
    /// untouched. With a workspace, only a key in it (the read-first guard of rename/delete).
    /// Status only, as a rename: the caller reads the keys again to see what they say now.
    pub async fn update_key(&self, hash: &str, update: &KeyUpdate) -> Result<()> {
        let body = update_body(update)?;
        valid_hash(hash)?;
        self.ensure_in_workspace(hash, "update key").await?;
        send_ok(self.auth(self.client.patch(format!("{}/{hash}", self.keys_url()))).json(&body), "openrouter update key").await
    }

    /// The account's credit, bought and used (`GET /credits`).
    pub async fn credits(&self) -> Result<Credits> {
        let v = send_json(self.auth(self.client.get(format!("{}/credits", self.root))), "openrouter credits").await?;
        parse_credits(&v)
    }
}

/// What `OPENROUTER_WORKSPACE` names: an id (a UUID, lower-cased), or a slug to look up.
#[derive(Debug, PartialEq)]
pub enum WorkspaceRef {
    Id(String),
    Slug,
}

/// Read `OPENROUTER_WORKSPACE`: a UUID is an id; letters, digits, `-` and `_` a slug;
/// anything else (it would go into a URL path) is refused. Pure, for testing.
pub fn workspace_ref(ws: &str) -> Result<WorkspaceRef> {
    let b = ws.as_bytes();
    let uuid = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    if uuid {
        return Ok(WorkspaceRef::Id(ws.to_ascii_lowercase()));
    }
    if !ws.is_empty() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_') {
        return Ok(WorkspaceRef::Slug);
    }
    Err(Error::Config(format!("OPENROUTER_WORKSPACE `{ws}` is neither a workspace id nor a slug")))
}

/// The id in a `GET /workspaces/{ref}` answer, if it's the workspace asked for: an id must
/// come back as itself, a slug with that slug. Anything else (an answer for another
/// workspace, one without an id) is refused — caching it would scope every call to the
/// wrong workspace. Pure.
pub fn resolved_workspace(v: &serde_json::Value, asked: &str, wanted: &WorkspaceRef) -> Result<String> {
    let d = v.get("data");
    let id = d.and_then(|d| d.get("id")).and_then(|s| s.as_str()).and_then(|s| match workspace_ref(s) {
        Ok(WorkspaceRef::Id(id)) => Some(id),
        _ => None,
    });
    let slug = d.and_then(|d| d.get("slug")).and_then(|s| s.as_str());
    let ok = match (wanted, &id) {
        (WorkspaceRef::Id(w), Some(id)) => w == id,
        (WorkspaceRef::Slug, Some(_)) => slug.is_some_and(|s| s.eq_ignore_ascii_case(asked)),
        _ => false,
    };
    match id {
        Some(id) if ok => Ok(id),
        _ => Err(Error::provider(format!(
            "openrouter workspace `{asked}`: the answer is for another workspace (id {}, slug {}) — refused",
            id.as_deref().unwrap_or("?"),
            slug.unwrap_or("?")
        ))),
    }
}

/// The keys of `keys` in workspace `ws` (all of them without one). A key that names another
/// workspace is dropped; one that doesn't say fails the listing — "not ours" would make
/// rotate re-mint beside a leaked key and revoke drop the row of a key that still works.
pub fn in_workspace(keys: Vec<KeyInfo>, ws: Option<&str>) -> Result<Vec<KeyInfo>> {
    let Some(ws) = ws else { return Ok(keys) };
    // Only a real workspace id says "another workspace": empty or malformed says nothing.
    let says = |k: &KeyInfo| k.workspace_id.as_deref().is_some_and(|w| matches!(workspace_ref(w), Ok(WorkspaceRef::Id(_))));
    if let Some(k) = keys.iter().find(|k| !says(k)) {
        return Err(Error::provider(format!(
            "openrouter list keys: key {} didn't say which workspace it's in — with OPENROUTER_WORKSPACE \
             set, a listing that can't be checked isn't used",
            short_hash(&k.hash)
        )));
    }
    Ok(keys.into_iter().filter(|k| k.workspace_id.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(ws))).collect())
}

/// A key hash goes into a URL path: letters and digits only (OpenRouter's are hex), so one
/// from a misbehaving answer can't point a delete at another endpoint.
pub fn valid_hash(hash: &str) -> Result<()> {
    if !hash.is_empty() && hash.bytes().all(|c| c.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(Error::provider(format!("openrouter: `{}` isn't a key hash — refused", short_hash(hash))))
    }
}

/// A hash's first 12 characters, for messages.
fn short_hash(hash: &str) -> &str {
    let end = hash.char_indices().nth(12).map(|(i, _)| i).unwrap_or(hash.len());
    &hash[..end]
}

/// The `POST /keys` body: the name, the cap if any, the workspace if any. Pure, for testing.
pub fn create_body(name: &str, limit: Option<f64>, ws: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "name": name });
    if let Some(l) = limit {
        body["limit"] = serde_json::json!(l);
    }
    if let Some(ws) = ws {
        body["workspace_id"] = serde_json::json!(ws);
    }
    body
}

/// `GET /credits`' `data`: `total_credits` and `total_usage`, both required. Pure.
pub fn parse_credits(v: &serde_json::Value) -> Result<Credits> {
    let d = v.get("data").unwrap_or(v);
    let num = |k: &str| d.get(k).and_then(|x| x.as_f64());
    match (num("total_credits"), num("total_usage")) {
        (Some(total), Some(used)) => Ok(Credits { total, used }),
        _ => Err(Error::provider("openrouter credits: the answer had no total_credits/total_usage")),
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

/// The `data` array of one `GET /keys` page, or an error when there isn't one (an empty
/// 2xx body arrives as `Null`, or JSON of another shape): never an empty page, which
/// [`collect_pages`] would take for the end of the listing. Pure, for testing.
pub fn page_data(v: &serde_json::Value) -> Result<&Vec<serde_json::Value>> {
    v.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
        Error::provider("openrouter list keys: a page had no `data` array — not a complete listing")
    })
}

/// The `PATCH /keys/{hash}` body for a rename: only `name`, so nothing else about the key
/// (limit, disabled, reset) changes. Pure, for testing.
pub fn rename_body(new_name: &str) -> serde_json::Value {
    serde_json::json!({ "name": new_name })
}

// ---------------------------------------------------------------------------------------
// Caps: what `keys limit|pause|resume` and the dashboard's raise-cap box change (pure)
// ---------------------------------------------------------------------------------------

/// When a key's cap starts over (`limit_reset`): at 00:00 UTC each day, each week (from Monday)
/// or each month. A key without one counts everything it ever spent against its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitReset {
    Daily,
    Weekly,
    Monthly,
}

impl LimitReset {
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitReset::Daily => "daily",
            LimitReset::Weekly => "weekly",
            LimitReset::Monthly => "monthly",
        }
    }

    /// `--reset`'s value: `daily`, `weekly`, `monthly`, or `never` (`None`: no reset). Pure.
    pub fn parse(s: &str) -> Result<Option<LimitReset>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "daily" => Ok(Some(LimitReset::Daily)),
            "weekly" => Ok(Some(LimitReset::Weekly)),
            "monthly" => Ok(Some(LimitReset::Monthly)),
            "never" | "none" => Ok(None),
            other => Err(Error::Config(format!("`{other}` isn't a cap reset — daily, weekly, monthly or never"))),
        }
    }

    /// A listed key's: anything but the three is none, as the dashboard reads it.
    pub fn of(k: &KeyInfo) -> Option<LimitReset> {
        match k.limit_reset.as_deref() {
            Some("daily") => Some(LimitReset::Daily),
            Some("weekly") => Some(LimitReset::Weekly),
            Some("monthly") => Some(LimitReset::Monthly),
            _ => None,
        }
    }
}

/// The period a cap that starts over `reset` counts, after an amount: `today`, `this week`,
/// `this month`, `in all` (no reset).
pub fn period_words(reset: Option<LimitReset>) -> &'static str {
    period(reset.map(|r| r.as_str())).unwrap_or("in all")
}

/// What a `PATCH /keys/{hash}` changes ([`OpenRouter::update_key`]): only the fields given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyUpdate {
    /// `Some(Some(usd))` caps the key at `usd`; `Some(None)` takes its cap away; `None` leaves it.
    pub limit: Option<Option<f64>>,
    /// `Some(Some(r))` starts the cap over each period `r`; `Some(None)` never; `None` leaves it.
    pub limit_reset: Option<Option<LimitReset>>,
    /// `Some(true)` pauses the key (its calls fail), `Some(false)` resumes it; `None` leaves it.
    pub disabled: Option<bool>,
}

/// OpenRouter's ceiling for a key's cap: under 1 000 000 USD.
pub const MAX_LIMIT: f64 = 1_000_000.0;

/// The `PATCH /keys/{hash}` body for `u`: `limit` (a number, or `null` = no cap), `limit_reset`
/// (`daily`/`weekly`/`monthly`, or `null` = never), `disabled` — each only when given, nothing
/// else (`name` stays; `workspace_id` on this call would *move* the key). A cap outside
/// OpenRouter's range (0 up to [`MAX_LIMIT`], a number) or an update that changes nothing
/// (OpenRouter answers 400 "No update fields provided") is refused here, before any call. Pure.
pub fn update_body(u: &KeyUpdate) -> Result<serde_json::Value> {
    let mut body = serde_json::Map::new();
    if let Some(limit) = u.limit {
        if let Some(x) = limit.filter(|x| !(x.is_finite() && (0.0..MAX_LIMIT).contains(x))) {
            return Err(Error::Config(format!("a key's cap is USD 0 up to (not including) {MAX_LIMIT:.0} — not {x}")));
        }
        body.insert("limit".into(), serde_json::json!(limit));
    }
    if let Some(reset) = u.limit_reset {
        body.insert("limit_reset".into(), serde_json::json!(reset.map(|r| r.as_str())));
    }
    if let Some(d) = u.disabled {
        body.insert("disabled".into(), serde_json::json!(d));
    }
    if body.is_empty() {
        return Err(Error::Config("openrouter update key: nothing to change".into()));
    }
    Ok(serde_json::Value::Object(body))
}

/// What `keys limit` (and the dashboard's raise-cap box) asks of a key's cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapChange {
    /// Cap it at this many USD (`10`, `2.50`).
    To(f64),
    /// Raise the cap it has by this many USD (`+5`).
    Raise(f64),
    /// No cap at all: only from `--no-cap`, never from an amount typed.
    NoCap,
}

/// An amount as typed: `10` or `2.50` (USD, a `$` allowed) sets the cap, `+5` raises the one the
/// key has by $5 (`+0` keeps it: with `--reset`, only when it starts over changes). Digits and
/// one `.` only — no `1e3`, `inf` or `nan` — and under OpenRouter's ceiling. Words for "no cap"
/// are refused with the flag that does it: a cap is never lifted by an amount. Pure.
pub fn parse_cap(s: &str) -> std::result::Result<CapChange, String> {
    let t = s.trim();
    let (raise, rest) = match t.strip_prefix('+') {
        Some(r) => (true, r.trim_start()),
        None => (false, t),
    };
    let num = rest.strip_prefix('$').unwrap_or(rest);
    let plain = num.bytes().any(|c| c.is_ascii_digit())
        && num.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && num.bytes().filter(|&c| c == b'.').count() <= 1;
    match plain.then(|| num.parse::<f64>().ok()).flatten() {
        Some(x) if x < MAX_LIMIT => Ok(if raise { CapChange::Raise(x) } else { CapChange::To(x) }),
        Some(_) => Err(format!("`{t}`: OpenRouter caps a key under ${MAX_LIMIT:.0}")),
        None if ["none", "no", "off", "unlimited", "nocap", "no-cap", "inf", "infinite", "∞"].contains(&t.to_ascii_lowercase().as_str()) => {
            Err("no cap is `--no-cap` (the CLI's), never an amount".into())
        }
        None if t.starts_with('-') => Err(format!("`{t}`: to lower a cap, give the new cap itself (`2`), not a negative raise")),
        None => Err(format!("`{t}` isn't an amount — USD as `10` or `2.50` sets the cap, `+5` raises it by $5")),
    }
}

/// What `k` has spent against a cap that starts over `reset`: that period's own usage (today's
/// for a daily cap, …, the life's without a reset) — else, for the reset the key has, OpenRouter's
/// `limit − limit_remaining`, which stops at the cap (a key over its cap reads as at it). Not
/// [`KeyUsage::spent`], which prefers the latter: a plan must see a $2.60 spend against a $2.50
/// cap as more than $2.50. Pure.
pub fn spent_against(k: &KeyInfo, reset: Option<LimitReset>) -> Option<f64> {
    let own = match reset {
        Some(LimitReset::Daily) => k.usage_daily,
        Some(LimitReset::Weekly) => k.usage_weekly,
        Some(LimitReset::Monthly) => k.usage_monthly,
        None => k.usage,
    };
    own.or_else(|| match (k.limit, k.limit_remaining) {
        (Some(l), Some(r)) if reset == LimitReset::of(k) => Some((l - r).max(0.0)),
        _ => None,
    })
}

/// One key's cap change, planned against the cap it has and what it has spent.
#[derive(Debug, Clone, PartialEq)]
pub struct CapPlan {
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub reset_before: Option<LimitReset>,
    pub reset_after: Option<LimitReset>,
    /// What it has spent against the new cap ([`spent_against`] for `reset_after`), if known.
    pub spent: Option<f64>,
}

impl CapPlan {
    fn cap_changes(&self) -> bool {
        match (self.before, self.after) {
            (Some(b), Some(a)) => (a - b).abs() > 1e-9,
            (None, None) => false,
            _ => true,
        }
    }

    /// Whether it changes the key at all.
    pub fn changes(&self) -> bool {
        self.cap_changes() || self.reset_after != self.reset_before
    }

    /// The key's calls still fail after it: the new cap is at or under what was spent against it.
    pub fn still_at_cap(&self) -> bool {
        matches!((self.after, self.spent), (Some(a), Some(s)) if s >= a - 0.005)
    }

    /// The `PATCH` it takes: the cap and the reset, each only when it changes.
    pub fn update(&self) -> KeyUpdate {
        KeyUpdate {
            limit: self.cap_changes().then_some(self.after),
            limit_reset: (self.reset_after != self.reset_before).then_some(self.reset_after),
            disabled: None,
        }
    }

    /// `cap $0.10 → $0.20 · spent $0.00 in all`, `cap $5.00 → $10.00 (monthly) · spent $0.42 this
    /// month`, `cap $5.00, daily → never · spent $3.00 in all — still at its cap`.
    pub fn words(&self) -> String {
        let cap = |x: Option<f64>| x.map(|x| crate::fleet::fmt_money("$", x)).unwrap_or_else(|| "no cap".into());
        let reset = |r: Option<LimitReset>| r.map_or("never", |r| r.as_str());
        let mut out = if self.cap_changes() { format!("cap {} → {}", cap(self.before), cap(self.after)) } else { format!("cap {}", cap(self.after)) };
        if self.reset_after != self.reset_before {
            out.push_str(&format!(", resets {} → {}", reset(self.reset_before), reset(self.reset_after)));
        } else if let (Some(r), Some(_)) = (self.reset_after, self.after) {
            out.push_str(&format!(" ({})", r.as_str()));
        }
        let spent = self.spent.map(|s| crate::fleet::fmt_money("$", s)).unwrap_or_else(|| "?".into());
        out.push_str(&format!(" · spent {spent} {}", period_words(self.reset_after)));
        if self.still_at_cap() {
            out.push_str(" — still at its cap");
        }
        out
    }
}

/// Plan `change` (none: the cap stays) and `reset` (none: as it is; `Some(None)`: never) for
/// `k`. Refused — the why, for the caller to put after the key's name — when a raise meets a key
/// with no cap, the cap would reach OpenRouter's ceiling, or (unless `force`) the change
/// tightens the cap (lower, a first cap, or another reset period) below what the key has spent
/// against it, or what it spent isn't known: its calls would fail at once, which is a pause, not
/// a cap. A raise that leaves it under what it spent goes ahead ([`CapPlan::still_at_cap`] says
/// so). The new cap is rounded to the millionth (no `0.25000000000000006`). Pure.
pub fn plan_cap(k: &KeyInfo, change: Option<CapChange>, reset: Option<Option<LimitReset>>, force: bool) -> std::result::Result<CapPlan, String> {
    let money = |x: f64| crate::fleet::fmt_money("$", x);
    let before = k.limit;
    let reset_before = LimitReset::of(k);
    let reset_after = reset.unwrap_or(reset_before);
    let after = match change {
        None => before,
        Some(CapChange::To(x)) => Some(x),
        Some(CapChange::Raise(x)) => match before {
            Some(b) => Some(b + x),
            None => return Err("it has no cap to raise — give the cap itself (`10`)".into()),
        },
        Some(CapChange::NoCap) => None,
    }
    .map(|x| (x * 1e6).round() / 1e6);
    if let Some(a) = after.filter(|a| !(a.is_finite() && *a >= 0.0 && *a < MAX_LIMIT)) {
        return Err(format!("{} is past OpenRouter's ceiling for a cap (under ${MAX_LIMIT:.0})", money(a)));
    }
    let plan = CapPlan { before, after, reset_before, reset_after, spent: spent_against(k, reset_after) };
    let tighter = match (before, after) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(b), Some(a)) => a < b - 1e-9 || reset_after != reset_before,
    };
    if let (true, false, Some(a)) = (tighter, force, after) {
        let when = period_words(reset_after);
        match plan.spent {
            Some(s) if a < s - 0.005 => {
                return Err(format!(
                    "a {} cap is under what it has spent {when} ({}) — its calls would fail at once; --force sets it \
                     anyway (`arena keys pause` stops a key outright)",
                    money(a),
                    money(s)
                ))
            }
            None => return Err(format!("what it has spent {when} isn't known — --force to tighten its cap anyway")),
            _ => {}
        }
    }
    Ok(plan)
}

/// Of the keys that go by one name, the one it shows: the enabled one, then the newest (the
/// first of equals). The one the dashboard's `AI$` cell is about, and the one `keys limit` and
/// `keys resume` change.
pub fn shown_key<'a>(keys: impl IntoIterator<Item = &'a KeyInfo>) -> Option<&'a KeyInfo> {
    keys.into_iter().fold(None, |best, k| match best {
        Some(b) if shown_rank(b) >= shown_rank(k) => Some(b),
        _ => Some(k),
    })
}

/// How [`shown_key`] ranks keys: enabled first, then by `created_at`.
fn shown_rank(k: &KeyInfo) -> (bool, Option<&str>) {
    (!k.disabled, k.created_at.as_deref())
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
    /// Change a key's cap, reset or disabled flag ([`OpenRouter::update_key`]). Default: refused
    /// — a fake for flows that never change a key in place need not say what it would do.
    async fn update_key(&self, hash: &str, _update: &KeyUpdate) -> Result<()> {
        Err(Error::provider(format!("openrouter update key {}: not supported here", short_hash(hash))))
    }
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
    async fn update_key(&self, hash: &str, update: &KeyUpdate) -> Result<()> {
        OpenRouter::update_key(self, hash, update).await
    }
}

// ---------------------------------------------------------------------------------------
// Usage: what the dashboard shows of the keys' spend and the account's credit (pure)
// ---------------------------------------------------------------------------------------

/// How close a key is to its cap — or the credit to running out — for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fill {
    Ok,
    /// 80% of the cap or more; a credit that the caps could spend.
    Near,
    /// At the cap, or disabled: the key's calls fail. A credit at $0: every key's do.
    Full,
}

/// One key's spend against its cap, as the dashboard shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyUsage {
    pub disabled: bool,
    /// Spent in the cap's current period: OpenRouter's own `limit − limit_remaining`, else
    /// the period's own usage (today's for a daily cap, …), else — for a cap that never
    /// starts over — the life's `usage`. Unknown rather than the life's spend for a cap that
    /// resets: a key that spent $30 in all could have $0.20 of today's $5.
    pub spent: Option<f64>,
    pub limit: Option<f64>,
    pub reset: Option<String>,
    pub today: Option<f64>,
    pub week: Option<f64>,
    pub month: Option<f64>,
    pub total: Option<f64>,
}

/// What the cap's period is called after "of $5": the period it starts over, if it does.
fn period(reset: Option<&str>) -> Option<&'static str> {
    match reset {
        Some("daily") => Some("today"),
        Some("weekly") => Some("this week"),
        Some("monthly") => Some("this month"),
        _ => None,
    }
}

/// An amount in as few characters as still say it: `0.42`, `12.3`, `123`.
fn short_amount(x: f64) -> String {
    let x = if x.abs() < 0.005 { 0.0 } else { x };
    if x < 10.0 {
        format!("{x:.2}")
    } else if x < 100.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.0}")
    }
}

/// A cap as it was set, without trailing zeros after a decimal point: `5`, `2.5`, `0.02`,
/// `150` (for 150.25: three digits are all the cell has room for).
fn cap_amount(x: f64) -> String {
    let s = if x.fract() == 0.0 && x.abs() < 1e9 { format!("{x:.0}") } else { short_amount(x) };
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

impl KeyUsage {
    pub fn of(k: &KeyInfo) -> Self {
        let reset = k.limit_reset.clone().filter(|r| period(Some(r)).is_some());
        let spent = match (k.limit, k.limit_remaining, reset.as_deref()) {
            (Some(l), Some(r), _) => Some((l - r).max(0.0)),
            (_, _, Some("daily")) => k.usage_daily,
            (_, _, Some("weekly")) => k.usage_weekly,
            (_, _, Some("monthly")) => k.usage_monthly,
            _ => k.usage,
        };
        KeyUsage {
            disabled: k.disabled,
            spent,
            limit: k.limit,
            reset,
            today: k.usage_daily,
            week: k.usage_weekly,
            month: k.usage_monthly,
            total: k.usage,
        }
    }

    /// What's left under the cap (`None`: no cap, or the spend isn't known).
    pub fn left(&self) -> Option<f64> {
        Some((self.limit? - self.spent?).max(0.0))
    }

    /// What OpenRouter's own counters say it spent in its cap's period — today's for a daily
    /// cap, …, the life's for a cap that never starts over. Not [`Self::spent`]: `limit −
    /// limit_remaining` reads $0 for every $0 cap, whatever was spent before the cap went to 0.
    fn period_usage(&self) -> Option<f64> {
        match self.reset.as_deref() {
            Some("daily") => self.today,
            Some("weekly") => self.week,
            Some("monthly") => self.month,
            _ => self.total,
        }
    }

    /// Paused rather than at its cap: an enabled key whose cap is $0 and that has spent
    /// nothing in the cap's period (or nothing it says). Its calls fail as an exhausted key's
    /// do, but by an operator's choice — a key handed out ahead of use, a participant held
    /// back — not because someone ran it dry: the summary bar's "at cap" warning leaves it
    /// out (the human, 2026-10-29: "why does it say 1 ai key at cap?" with every AI$ cell on
    /// screen under its cap), and its cell says `paused`.
    pub fn paused(&self) -> bool {
        !self.disabled && self.limit.is_some_and(|l| l <= 0.0) && self.period_usage().or(self.spent).is_none_or(|s| s < 0.005)
    }

    pub fn fill(&self) -> Fill {
        if self.disabled {
            return Fill::Full;
        }
        if self.paused() {
            return Fill::Ok;
        }
        match (self.spent, self.limit) {
            (_, Some(l)) if l <= 0.0 => Fill::Full,
            (Some(s), Some(l)) if s >= l - 0.005 => Fill::Full,
            (Some(s), Some(l)) if s >= 0.8 * l => Fill::Near,
            _ => Fill::Ok,
        }
    }

    /// The `AI$` cell (at most 9 wide): `1.23/5` against a cap, `$0.42` without one, `off` when disabled,
    /// `paused` for a $0 cap nothing was spent against ([`Self::paused`]), `?` when the listing
    /// gave no spend.
    pub fn cell(&self) -> String {
        if self.disabled {
            return "off".into();
        }
        if self.paused() {
            return "paused".into();
        }
        match (self.spent, self.limit) {
            (None, _) => "?".into(),
            (Some(s), Some(l)) => format!("{}/{}", short_amount(s), cap_amount(l)),
            (Some(s), None) => format!("${}", short_amount(s)),
        }
    }

    /// The detail pane's line: `$1.23 of $5 this month ($3.77 left) · today $0.20 · week
    /// $1.00 · month $1.23 · all $4.56`.
    pub fn detail(&self) -> String {
        let money = |x: Option<f64>| x.map(|x| crate::fleet::fmt_money("$", x)).unwrap_or_else(|| "?".into());
        let mut out = String::new();
        if self.disabled {
            out.push_str("disabled · ");
        }
        if self.paused() {
            out.push_str("paused (a $0 cap, nothing spent against it) · ");
        }
        match self.limit {
            Some(l) => {
                out.push_str(&format!("{} of {}", money(self.spent), crate::fleet::fmt_money("$", l)));
                if let Some(p) = period(self.reset.as_deref()) {
                    out.push_str(&format!(" {p}"));
                }
                if let Some(left) = self.left() {
                    out.push_str(&format!(" ({} left)", crate::fleet::fmt_money("$", left)));
                }
            }
            None => out.push_str(&format!("{} spent, no cap", money(self.spent))),
        }
        out.push_str(&format!(
            " · today {} · week {} · month {} · all {}",
            money(self.today),
            money(self.week),
            money(self.month),
            money(self.total)
        ));
        out
    }
}

/// What the dashboard knows of OpenRouter after its last reads: each listed key's usage
/// (`all`), the one shown for each name (a key is named after its pod: [`key_name`]) and the
/// account's credit. The keys and the credit are read and kept apart: one failing keeps its
/// own last answer, never the other's.
#[derive(Debug, Clone, Default)]
pub struct UsageView {
    /// The key shown for each name.
    pub keys: HashMap<String, KeyUsage>,
    /// That key as listed, for planning a change to it ([`plan_cap`]: the dashboard's raise-cap box).
    pub infos: HashMap<String, KeyInfo>,
    /// Every listed key, by name (a name can be held twice): what the totals add up.
    pub all: Vec<(String, KeyUsage)>,
    pub credits: Option<Credits>,
    /// Why the last listing failed, if it did (`keys`/`all` are then the last good ones).
    pub keys_error: Option<String>,
    /// Why the last credit read failed, if it did (`credits` is then the last good one).
    pub credits_error: Option<String>,
    /// A read has landed (before the first, an empty view means "not read yet", not "no keys").
    pub read: bool,
}

impl UsageView {
    /// From a listing. A name held by more than one key (an old one disabled, a rotation's
    /// leftover) shows the enabled one, then the newest; the totals count them all.
    pub fn from_keys(keys: &[KeyInfo], credits: Option<Credits>) -> Self {
        let mut v = UsageView { credits, read: true, ..Default::default() };
        v.set_keys(keys);
        v
    }

    fn set_keys(&mut self, keys: &[KeyInfo]) {
        let mut best: HashMap<String, &KeyInfo> = HashMap::new();
        let mut all = Vec::new();
        for k in keys {
            let Some(name) = k.name.as_deref().filter(|n| !n.is_empty()) else { continue };
            all.push((name.to_string(), KeyUsage::of(k)));
            match best.get(name) {
                Some(b) if shown_rank(b) >= shown_rank(k) => {}
                _ => {
                    best.insert(name.to_string(), k);
                }
            }
        }
        self.keys = best.iter().map(|(n, k)| (n.clone(), KeyUsage::of(k))).collect();
        self.infos = best.into_iter().map(|(n, k)| (n, k.clone())).collect();
        self.all = all;
    }

    /// Take one round's reads: each answer replaces its own part, each failure keeps that
    /// part's last good answer and says why.
    pub fn apply(&mut self, keys: std::result::Result<Vec<KeyInfo>, String>, credits: std::result::Result<Credits, String>) {
        self.read = true;
        match keys {
            Ok(k) => {
                self.set_keys(&k);
                self.keys_error = None;
            }
            Err(e) => self.keys_error = Some(e),
        }
        match credits {
            Ok(c) => {
                self.credits = Some(c);
                self.credits_error = None;
            }
            Err(e) => self.credits_error = Some(e),
        }
    }

    /// This cohort's share: every key named `<prefix>-…` or after one of `pods` (an `@name`
    /// pod's key carries its bare name), summed for the summary bar — a disabled key's spend
    /// today counts, its cap doesn't; a paused one ([`KeyUsage::paused`]) isn't at its cap.
    /// The keys at their cap are named, with whether a pod in `pods` carries each: a key of
    /// the cohort's can outlive its pod (a terminated machine, a renamed one), and then no
    /// row shows it.
    pub fn cohort(&self, prefix: &str, pods: &[String]) -> CohortUsage {
        let needle = format!("{prefix}-");
        let mut c = CohortUsage {
            credit_left: self.credits.map(|c| c.left()),
            failed: self.keys_error.is_some() || self.credits_error.is_some(),
            ..Default::default()
        };
        for (name, k) in &self.all {
            if !(name.starts_with(&needle) || pods.iter().any(|p| p == name)) {
                continue;
            }
            c.keys += 1;
            if let Some(t) = k.today {
                *c.today.get_or_insert(0.0) += t;
            }
            if k.disabled {
                continue;
            }
            if k.fill() == Fill::Full {
                c.capped.push(CappedKey { name: name.clone(), pod: pods.iter().any(|p| p == name) });
            }
            match (k.limit, k.left()) {
                (None, _) => c.uncapped += 1,
                (Some(_), Some(left)) => c.headroom += left,
                (Some(l), None) => c.headroom += l.max(0.0), // spend unknown: the whole cap may go
            }
        }
        // The ones no row shows first: they're what the bar is for.
        c.capped.sort_by(|a, b| a.pod.cmp(&b.pod).then_with(|| a.name.cmp(&b.name)));
        c
    }
}

/// A cohort key at its cap: its name, and whether a listed pod is named after it (else no
/// row in the dashboard shows it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedKey {
    pub name: String,
    pub pod: bool,
}

/// How many keys at their cap the summary bar names; the rest are `+N more`.
pub const CAPPED_NAMED: usize = 2;

/// The cohort's keys added up, and the account's credit, for the summary bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CohortUsage {
    pub keys: usize,
    /// Spent today by the cohort's keys (`None`: no key said).
    pub today: Option<f64>,
    /// Enabled keys at their cap — those participants' AI calls fail — the ones no listed pod
    /// carries first, then by name. Not the paused ones ($0 cap, nothing spent).
    pub capped: Vec<CappedKey>,
    /// What the enabled keys may still spend under their caps.
    pub headroom: f64,
    /// Enabled keys with no cap: nothing bounds what they spend.
    pub uncapped: usize,
    pub credit_left: Option<f64>,
    /// The last read failed (the numbers are the last good ones).
    pub failed: bool,
}

impl CohortUsage {
    /// The summary bar's warning for keys at their cap, naming them — `⚠ AI key at cap:
    /// arena9-foo (no pod)`, `⚠ 3 AI keys at cap: arena9-foo (no pod), arena9-bar +1 more` —
    /// up to [`CAPPED_NAMED`]; `(no pod)`: no row shows it (`arena keys list` lists every key).
    /// `None`: none at cap.
    pub fn capped_names(&self) -> Option<String> {
        let n = self.capped.len();
        if n == 0 {
            return None;
        }
        let named: Vec<String> = self.capped.iter().take(CAPPED_NAMED).map(|k| if k.pod { k.name.clone() } else { format!("{} (no pod)", k.name) }).collect();
        let more = if n > CAPPED_NAMED { format!(" +{} more", n - CAPPED_NAMED) } else { String::new() };
        let head = if n == 1 { "⚠ AI key at cap".to_string() } else { format!("⚠ {n} AI keys at cap") };
        Some(format!("{head}: {}{more}", named.join(", ")))
    }

    /// The summary bar's parts: a warning to lead it, if any (` ⚠ openrouter $12.00 left,
    /// the keys' caps allow $40.00 more ·`; `‼` when the credit is gone), the tail (`openrouter
    /// $422.09 left · AI today $3.20`), and how bad the warning is.
    pub fn tui_parts(&self) -> (Option<String>, Option<String>, Fill) {
        let money = |x: f64| crate::fleet::fmt_money("$", x);
        let mut lead = Vec::new();
        let mut worst = Fill::Ok;
        match self.credit_left {
            Some(left) if left <= 0.005 => {
                lead.push(format!("‼ openrouter {} left — every key's calls fail", money(left)));
                worst = Fill::Full;
            }
            Some(left) if left < self.headroom => {
                lead.push(format!("⚠ openrouter {} left, the keys' caps allow {} more", money(left), money(self.headroom)));
                worst = Fill::Near;
            }
            _ => {}
        }
        if let Some(names) = self.capped_names() {
            lead.push(names);
            worst = worst.max(Fill::Near);
        }
        let lead = (!lead.is_empty()).then(|| format!(" {} ·", lead.join(" · ")));
        let mut tail = Vec::new();
        if let Some(left) = self.credit_left {
            tail.push(format!("openrouter {} left", money(left)));
        }
        if self.keys > 0 {
            tail.push(format!("AI today {}", self.today.map(money).unwrap_or_else(|| "?".into())));
        }
        if self.uncapped > 0 {
            tail.push(format!("{} uncapped", self.uncapped));
        }
        if tail.is_empty() && self.failed {
            tail.push("openrouter n/a".into());
        } else if self.failed {
            tail.push("(n/a)".into());
        }
        let tail = (!tail.is_empty()).then(|| tail.join(" · "));
        (lead, tail, worst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client for a loopback test server at `base`, with no workspace.
    fn test_client(key: &str, base: &str) -> OpenRouter {
        OpenRouter { root: base.to_string(), client: crate::http::test_server::client(), ..OpenRouter::new(key) }
    }

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
        use crate::http::test_server::{canned, serve};
        let srv = serve(vec![
            canned(401, "text/html", "<html>Unauthorized</html>"),
            canned(200, "application/json", r#"{"key":"sk-or-v1-SECRET","data":{"hash":"h"#), // truncated JSON
            canned(200, "application/json", r#"{"key":"sk-or-v1-SECRET2","data":{"hash":"h2"}}"#),
            canned(404, "application/json", r#"{"error":{"message":"not found"}}"#),
        ]);
        let or = test_client("BOGUS", &srv.base);
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
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let srv = serve(vec![
            canned(200, json, r#"{"data":[{"hash":"a","name":"devtest-apple"},{"hash":"b","name":"devtest-bloom","disabled":true}]}"#),
            canned(200, json, r#"{"data":[]}"#),
            canned(200, json, r#"{"data":[{"hash":"a"}]}"#),
            canned(401, "text/html", "<html>Unauthorized</html>"),
            canned(200, "text/plain", "ok (not json)"),
            canned(403, "text/plain", "Forbidden"),
        ]);
        let or = test_client("K", &srv.base);
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
                "GET /keys?include_disabled=true&offset=0 HTTP/1.1",
                "GET /keys?include_disabled=true&offset=2 HTTP/1.1",
                "GET /keys?include_disabled=true&offset=0 HTTP/1.1",
                "GET /keys?include_disabled=true&offset=1 HTTP/1.1",
                "PATCH /keys/a HTTP/1.1",
                "PATCH /keys/a HTTP/1.1",
            ]
        );
    }

    /// A 2xx page with no `data` array — an empty 200, a 204, JSON of another shape — fails
    /// the listing. `send_json` reads an empty 2xx body as `Null`; were that an empty page,
    /// [`collect_pages`] would stop there and return page 1 as the whole listing, and revoke
    /// would drop the CSV row of a key past it that still works.
    #[tokio::test]
    async fn a_page_without_data_fails_the_listing_instead_of_ending_it() {
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let page1 = r#"{"data":[{"hash":"a","name":"devtest-apple"}]}"#;
        for (case, second) in [
            ("empty 200", canned(200, json, "")),
            ("204", canned(204, json, "")),
            ("json without data", canned(200, json, r#"{"keys":[]}"#)),
            ("data not an array", canned(200, json, r#"{"data":null}"#)),
        ] {
            let srv = serve(vec![canned(200, json, page1), second]);
            let or = test_client("K", &srv.base);
            let e = or.list_keys().await.expect_err(case);
            assert!(e.to_string().contains("no `data` array"), "{case}: {e}");
            assert_eq!(srv.requests.lock().unwrap().len(), 2, "{case}: page 2 was asked for");
        }
    }

    #[test]
    fn page_data_needs_a_data_array() {
        use serde_json::json;
        assert_eq!(page_data(&json!({"data": [{"hash": "a"}]})).unwrap().len(), 1);
        assert!(page_data(&json!({"data": []})).unwrap().is_empty(), "an explicit empty page is the end");
        for bad in [json!(null), json!({}), json!({"data": null}), json!({"data": {}}), json!([])] {
            assert!(page_data(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rename_body_sets_only_the_name() {
        assert_eq!(rename_body("arena9-apple"), serde_json::json!({ "name": "arena9-apple" }));
    }

    fn key(hash: &str) -> KeyInfo {
        KeyInfo { hash: hash.into(), name: Some(format!("n-{hash}")), ..Default::default() }
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

    /// A workspace is an id (a UUID, lower-cased) or a slug; anything that could escape the
    /// URL path is refused.
    #[test]
    fn workspace_ref_reads_ids_and_slugs_and_refuses_the_rest() {
        let id = "C9F89F20-08E3-4CFF-BAD6-70C478E2C2C4";
        assert_eq!(workspace_ref(id).unwrap(), WorkspaceRef::Id(id.to_ascii_lowercase()));
        for slug in ["arena-dev-test", "default", "ws_2"] {
            assert_eq!(workspace_ref(slug).unwrap(), WorkspaceRef::Slug, "{slug}");
        }
        for bad in ["", "a/b", "../keys", "x?y", "sp ace", "ws%2F", "a.b"] {
            assert!(workspace_ref(bad).is_err(), "{bad}");
        }
    }

    /// Keys naming another workspace are dropped; a key that doesn't say fails the listing
    /// (an empty "not ours" would make rotate leave a leaked key live).
    #[test]
    fn in_workspace_drops_other_workspaces_and_refuses_keys_that_dont_say() {
        const W1: &str = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        const W2: &str = "a3339b09-7feb-5818-be3e-1b9484142ece";
        let k = |h: &str, ws: Option<&str>| KeyInfo { hash: h.into(), workspace_id: ws.map(String::from), ..Default::default() };
        let keys = vec![k("a", Some(W1)), k("b", Some(&W1.to_uppercase())), k("c", Some(W2))];
        let hashes = |v: Vec<KeyInfo>| v.into_iter().map(|k| k.hash).collect::<Vec<_>>();
        assert_eq!(hashes(in_workspace(keys.clone(), Some(W1)).unwrap()), ["a", "b"]);
        let mut silent = keys.clone();
        silent.push(k("d", None));
        let e = in_workspace(silent.clone(), Some(W1)).unwrap_err().to_string();
        assert!(e.contains("key d didn't say which workspace"), "{e}");
        for junk in ["", "w2", "not-a-uuid"] {
            let mut odd = keys.clone();
            odd.push(k("e", Some(junk)));
            assert!(in_workspace(odd, Some(W1)).is_err(), "`{junk}` says nothing about the workspace");
        }
        assert_eq!(hashes(in_workspace(silent, None).unwrap()), ["a", "b", "c", "d"], "no workspace: as before");
    }

    /// A workspace lookup is only taken when the answer is the workspace asked for.
    #[test]
    fn resolved_workspace_takes_only_the_workspace_asked_for() {
        use serde_json::json;
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let theirs = "a3339b09-7feb-5818-be3e-1b9484142ece";
        let ans = |id: &str, slug: &str| json!({"data": {"id": id, "slug": slug}});
        let slug = WorkspaceRef::Slug;
        let id = WorkspaceRef::Id(ours.into());
        assert_eq!(resolved_workspace(&ans(ours, "arena-dev-test"), "arena-dev-test", &slug).unwrap(), ours);
        assert_eq!(resolved_workspace(&ans(&ours.to_uppercase(), "x"), ours, &id).unwrap(), ours);
        assert_eq!(resolved_workspace(&ans(ours, "arena-dev-test"), "Arena-Dev-Test", &slug).unwrap(), ours);
        for (v, asked, wanted) in [
            (ans(theirs, "default"), "arena-dev-test", &slug),
            (ans(theirs, "arena-dev-test"), ours, &id),
            (json!({"data": {"slug": "arena-dev-test"}}), "arena-dev-test", &slug),
            (ans("not-a-uuid", "arena-dev-test"), "arena-dev-test", &slug),
            (json!(null), ours, &id),
        ] {
            let e = resolved_workspace(&v, asked, wanted).unwrap_err().to_string();
            assert!(e.contains("refused"), "{v}: {e}");
        }
    }

    #[test]
    fn valid_hash_is_letters_and_digits() {
        assert!(valid_hash("f01d52606dc8f0a8303a7b5cc3fa07109c2e346cec7c0a16b40de462992ce943").is_ok());
        for bad in ["", "a/../../workspaces/x", "a?x=1", "a b", "a%2F"] {
            assert!(valid_hash(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_body_carries_the_workspace_and_cap_when_given() {
        use serde_json::json;
        assert_eq!(create_body("devtest-a", None, None), json!({"name": "devtest-a"}));
        assert_eq!(
            create_body("devtest-a", Some(1.0), Some("w1")),
            json!({"name": "devtest-a", "limit": 1.0, "workspace_id": "w1"})
        );
    }

    #[test]
    fn credits_need_both_totals() {
        use serde_json::json;
        let c = parse_credits(&json!({"data": {"total_credits": 2335, "total_usage": 1912.91}})).unwrap();
        assert_eq!(c, Credits { total: 2335.0, used: 1912.91 });
        assert!((c.left() - 422.09).abs() < 1e-9);
        assert!(parse_credits(&json!({"data": {"total_credits": 5}})).is_err());
        assert!(parse_credits(&json!(null)).is_err());
    }

    /// With a workspace on the wire: a slug is looked up once, every listing page asks for
    /// the id and a key from elsewhere in the answer is dropped; a create sends it; a delete
    /// or rename of a key in another workspace is refused before anything is sent to change
    /// it; credits are one GET.
    #[tokio::test]
    async fn a_workspace_scopes_every_call_and_guards_deletes_and_renames() {
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let srv = serve(vec![
            canned(200, json, &format!(r#"{{"data":{{"id":"{ours}","slug":"arena-dev-test"}}}}"#)),
            canned(200, json, &format!(r#"{{"data":[{{"hash":"a","name":"devtest-apple","workspace_id":"{ours}"}},{{"hash":"x","name":"arena9-apple","workspace_id":"a3339b09-7feb-5818-be3e-1b9484142ece"}}]}}"#)),
            canned(200, json, r#"{"data":[]}"#),
            canned(200, json, &format!(r#"{{"key":"sk-or-v1-S","data":{{"hash":"n","name":"devtest-bloom","workspace_id":"{ours}"}}}}"#)),
            canned(200, json, r#"{"data":{"hash":"x","workspace_id":"other"}}"#),
            canned(200, json, r#"{"data":{"hash":"y"}}"#),
            canned(200, json, &format!(r#"{{"data":{{"hash":"a","workspace_id":"{ours}"}}}}"#)),
            canned(200, json, "{}"),
            canned(200, json, r#"{"data":{"total_credits":10,"total_usage":4}}"#),
        ]);
        let or = test_client("K", &srv.base).with_workspace(Some(" arena-dev-test "));
        let keys = or.list_keys().await.unwrap();
        assert_eq!(keys.iter().map(|k| k.hash.as_str()).collect::<Vec<_>>(), ["a"], "the other workspace's key is dropped");
        or.create_key("devtest-bloom", Some(1.0)).await.unwrap();
        let e = or.delete_key("x").await.unwrap_err().to_string();
        assert!(e.contains("refused") && e.contains("other"), "{e}");
        let e = or.rename_key("y", "devtest-z").await.unwrap_err().to_string();
        assert!(e.contains("refused") && e.contains("(not said)"), "a key that doesn't say is refused too: {e}");
        or.delete_key("a").await.unwrap();
        assert_eq!(or.credits().await.unwrap().left(), 6.0);
        assert_eq!(
            *srv.requests.lock().unwrap(),
            [
                "GET /workspaces/arena-dev-test HTTP/1.1".to_string(),
                format!("GET /keys?include_disabled=true&offset=0&workspace_id={ours} HTTP/1.1"),
                format!("GET /keys?include_disabled=true&offset=2&workspace_id={ours} HTTP/1.1"),
                "POST /keys HTTP/1.1".into(),
                "GET /keys/x HTTP/1.1".into(),
                "GET /keys/y HTTP/1.1".into(),
                "GET /keys/a HTTP/1.1".into(),
                "DELETE /keys/a HTTP/1.1".into(),
                "GET /credits HTTP/1.1".into(),
            ],
            "the slug looked up once; no DELETE/PATCH for x or y"
        );
        let create = srv.bodies.lock().unwrap().iter().find(|b| b.contains("devtest-bloom")).cloned().unwrap();
        let create: serde_json::Value = serde_json::from_str(&create).unwrap();
        assert_eq!(create["workspace_id"], ours);
    }

    /// A workspace that can't be resolved fails the call — it never falls back to an
    /// unscoped listing (the Default workspace's keys).
    #[tokio::test]
    async fn an_unresolvable_workspace_fails_instead_of_listing_the_default_one() {
        use crate::http::test_server::{canned, serve};
        let srv = serve(vec![canned(404, "application/json", r#"{"error":{"message":"not found"}}"#)]);
        let or = test_client("K", &srv.base).with_workspace(Some("nope"));
        assert!(or.list_keys().await.is_err());
        let or = test_client("K", &srv.base).with_workspace(Some("a/b"));
        assert!(or.delete_key("h").await.is_err());
        assert_eq!(srv.requests.lock().unwrap().len(), 1, "only the lookup; nothing listed or deleted");
    }

    /// A create the API put in another workspace (or didn't say where) fails, naming the key
    /// to delete by hand; nothing is deleted (the answer can't prove which key it made) and
    /// the secret isn't returned or quoted.
    #[tokio::test]
    async fn a_key_created_outside_the_workspace_fails_the_create_and_deletes_nothing() {
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let ws = format!(r#"{{"data":{{"id":"{ours}","slug":"arena-dev-test"}}}}"#);
        let srv = serve(vec![
            canned(200, json, &ws),
            canned(200, json, r#"{"key":"sk-or-v1-S1","data":{"hash":"d1","name":"devtest-a","workspace_id":"other"}}"#),
            canned(200, json, r#"{"key":"sk-or-v1-S2","data":{"hash":"d2","name":"devtest-a"}}"#),
            canned(200, json, r#"{"data":{"hash":"d2"}}"#),
            canned(200, json, r#"{"key":"sk-or-v1-S3","data":{"hash":"d3","name":"devtest-a"}}"#),
            canned(200, json, &format!(r#"{{"data":{{"hash":"d3","workspace_id":"{ours}"}}}}"#)),
        ]);
        let or = test_client("K", &srv.base).with_workspace(Some("arena-dev-test"));
        let e = or.create_key("devtest-a", Some(1.0)).await.unwrap_err().to_string();
        assert!(e.contains("landed in workspace other") && e.contains("hash d1") && !e.contains("sk-or"), "{e}");
        let e = or.create_key("devtest-a", Some(1.0)).await.unwrap_err().to_string();
        assert!(e.contains("(not said)") && e.contains("hash d2"), "{e}");
        // An answer that doesn't say, but the key itself is in the workspace: fine.
        let k = or.create_key("devtest-a", Some(1.0)).await.unwrap();
        assert_eq!((k.hash.as_str(), k.secret.as_str()), ("d3", "sk-or-v1-S3"));
        assert_eq!(
            *srv.requests.lock().unwrap(),
            [
                "GET /workspaces/arena-dev-test HTTP/1.1",
                "POST /keys HTTP/1.1",
                "POST /keys HTTP/1.1",
                "GET /keys/d2 HTTP/1.1",
                "POST /keys HTTP/1.1",
                "GET /keys/d3 HTTP/1.1",
            ],
            "no DELETE"
        );
    }

    /// Scoped, a listed key that can't be read fails the listing (dropped, it would read as
    /// "no key"); unscoped it's skipped, as before.
    #[tokio::test]
    async fn a_scoped_listing_with_an_unreadable_key_fails() {
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let page = format!(r#"{{"data":[{{"hash":"a","workspace_id":"{ours}"}},{{"name":"no-hash"}}]}}"#);
        let srv = serve(vec![
            canned(200, json, &format!(r#"{{"data":{{"id":"{ours}","slug":"arena-dev-test"}}}}"#)),
            canned(200, json, &page),
            canned(200, json, &page),
            canned(200, json, r#"{"data":[]}"#),
        ]);
        let or = test_client("K", &srv.base).with_workspace(Some(ours));
        assert!(or.list_keys().await.unwrap_err().to_string().contains("couldn't be read"));
        let unscoped = test_client("K", &srv.base);
        assert_eq!(unscoped.list_keys().await.unwrap().len(), 1);
    }

    /// A workspace given as an id is looked up too: a mistyped id fails before anything is
    /// listed or created.
    #[tokio::test]
    async fn a_workspace_id_is_checked_once_before_use() {
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let srv = serve(vec![
            canned(404, json, r#"{"error":{"message":"Workspace not found"}}"#),
            canned(200, json, &format!(r#"{{"data":{{"id":"{ours}","slug":"arena-dev-test"}}}}"#)),
            canned(200, json, r#"{"data":[{"hash":"a","name":"devtest-a"}]}"#),
            canned(200, json, r#"{"data":[]}"#),
        ]);
        let typo = test_client("K", &srv.base).with_workspace(Some("c9f89f20-08e3-4cff-bad6-70c478e2c2c5"));
        assert!(typo.list_keys().await.is_err());
        let or = test_client("K", &srv.base).with_workspace(Some(ours));
        let e = or.list_keys().await.unwrap_err().to_string();
        assert!(e.contains("didn't say which workspace"), "a listing without workspace ids isn't used: {e}");
        assert_eq!(srv.requests.lock().unwrap().len(), 4, "the typo's lookup, ours, two pages");
    }

    fn usage(limit: Option<f64>, remaining: Option<f64>, total: Option<f64>, reset: Option<&str>) -> KeyUsage {
        KeyUsage::of(&KeyInfo {
            hash: "h".into(),
            limit,
            limit_remaining: remaining,
            usage: total,
            limit_reset: reset.map(String::from),
            usage_daily: Some(0.2),
            usage_weekly: Some(1.0),
            usage_monthly: Some(1.23),
            ..Default::default()
        })
    }

    /// The cell, the colour and the detail line: spend is the cap's period's (limit −
    /// remaining) when both are known, else the life's usage.
    #[test]
    fn key_usage_cells_colours_and_detail() {
        let k = usage(Some(5.0), Some(3.77), Some(4.56), Some("monthly"));
        assert_eq!((k.cell(), k.fill()), ("1.23/5".to_string(), Fill::Ok));
        assert_eq!(
            k.detail(),
            "$1.23 of $5.00 this month ($3.77 left) · today $0.20 · week $1.00 · month $1.23 · all $4.56"
        );
        for (limit, rem, total, cell, fill) in [
            (Some(5.0), Some(1.0), None, "4.00/5", Fill::Near),
            (Some(5.0), Some(0.0), None, "5.00/5", Fill::Full),
            (Some(50.0), None, Some(12.34), "12.3/50", Fill::Ok),
            (Some(2.5), None, Some(2.6), "2.60/2.5", Fill::Full),
            (Some(0.02), Some(0.0), None, "0.02/0.02", Fill::Full),
            (None, None, Some(123.4), "$123", Fill::Ok),
            // A $0 cap: paused while nothing was spent against it, else at its cap.
            (Some(0.0), None, Some(0.0), "paused", Fill::Ok),
            (Some(0.0), Some(0.0), None, "paused", Fill::Ok),
            (Some(0.0), Some(0.0), Some(2.0), "0.00/0", Fill::Full),
            (Some(5.0), None, None, "?", Fill::Ok),
        ] {
            let k = usage(limit, rem, total, None);
            assert_eq!((k.cell().as_str(), k.fill()), (cell, fill), "{limit:?} {rem:?} {total:?}");
        }
        // A cap that starts over, without limit_remaining: the period's own spend, not the life's.
        let daily = usage(Some(5.0), None, Some(30.0), Some("daily"));
        assert_eq!((daily.cell().as_str(), daily.fill()), ("0.20/5", Fill::Ok));
        let weekly = usage(Some(5.0), None, Some(30.0), Some("weekly"));
        assert_eq!(weekly.spent, Some(1.0));
        let mut no_month = KeyInfo { limit: Some(5.0), limit_reset: Some("monthly".into()), usage: Some(30.0), ..Default::default() };
        assert_eq!(KeyUsage::of(&no_month).spent, None, "unknown, never the life's $30");
        no_month.limit_reset = None;
        assert_eq!(KeyUsage::of(&no_month).spent, Some(30.0), "a cap that never resets: the life's");
        // Caps keep their integer digits.
        assert_eq!(usage(Some(150.25), Some(150.25), None, None).cell(), "0.00/150");
        assert_eq!(usage(Some(100.4), Some(100.4), None, None).cell(), "0.00/100");
        assert_eq!(usage(Some(10.5), Some(10.5), None, None).cell(), "0.00/10.5");
        let none = usage(None, None, Some(0.5), None);
        assert!(none.detail().starts_with("$0.50 spent, no cap · today $0.20"), "{}", none.detail());
        let mut off = usage(Some(5.0), Some(5.0), Some(0.0), Some("bogus"));
        off.disabled = true;
        assert_eq!((off.cell().as_str(), off.fill()), ("off", Fill::Full));
        assert!(off.detail().starts_with("disabled · $0.00 of $5.00 ($5.00 left)"), "an unknown reset says no period: {}", off.detail());
        off.limit = Some(0.0);
        assert_eq!((off.cell().as_str(), off.fill(), off.paused()), ("off", Fill::Full, false), "disabled wins over paused");
        let paused = usage(Some(0.0), Some(0.0), Some(4.0), Some("monthly"));
        assert_eq!((paused.cell().as_str(), paused.fill()), ("0.00/0", Fill::Full), "$1.23 spent this month against a $0 cap: at it");
        let paused = KeyUsage { month: Some(0.0), ..paused };
        assert_eq!((paused.cell().as_str(), paused.fill()), ("paused", Fill::Ok), "nothing this month, though $4 in its life");
        assert!(paused.detail().starts_with("paused (a $0 cap, nothing spent against it) · $0.00 of $0.00 this month"), "{}", paused.detail());
    }

    /// The summary bar names the cohort's keys at their cap — the ones no listed pod carries
    /// first (`(no pod)`: no row shows them), then by name — up to two, then `+N more`. A paused
    /// key ($0 cap, nothing spent in its period) and a disabled one aren't counted; a $0 cap
    /// that was spent against is; a key outside the cohort never is.
    #[test]
    #[allow(clippy::type_complexity)]
    fn cohort_names_its_keys_at_cap() {
        let k = |name: &str, limit: f64, remaining: Option<f64>, today: Option<f64>, total: Option<f64>, reset: Option<&str>, disabled: bool| KeyInfo {
            hash: format!("h-{name}"),
            name: Some(name.into()),
            disabled,
            limit: Some(limit),
            limit_remaining: remaining,
            usage_daily: today,
            usage: total,
            limit_reset: reset.map(String::from),
            ..Default::default()
        };
        let full = |n: &str| k(n, 5.0, Some(0.0), Some(5.0), Some(9.0), Some("daily"), false);
        let under = |n: &str| k(n, 5.0, Some(4.0), Some(1.0), Some(9.0), Some("daily"), false);
        let off = |n: &str| k(n, 5.0, Some(0.0), Some(5.0), Some(9.0), Some("daily"), true);
        // $0 daily caps: $3 in its life but nothing today (paused); $0.40 today (at cap).
        let paused_daily = |n: &str| k(n, 0.0, Some(0.0), Some(0.0), Some(3.0), Some("daily"), false);
        let spent_daily = |n: &str| k(n, 0.0, Some(0.0), Some(0.4), Some(3.0), Some("daily"), false);
        // $0 caps that never start over: nothing in its life (paused), nothing said (paused),
        // $2 in its life (at cap).
        let paused_new = |n: &str| k(n, 0.0, None, None, Some(0.0), None, false);
        let paused_unsaid = |n: &str| k(n, 0.0, None, None, None, None, false);
        let spent_life = |n: &str| k(n, 0.0, None, None, Some(2.0), None, false);
        let pods = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let cases: Vec<(Vec<KeyInfo>, Vec<String>, Vec<(&str, bool)>, Option<&str>)> = vec![
            (vec![under("devtest-a")], pods(&["devtest-a"]), vec![], None),
            (vec![full("devtest-a")], pods(&["devtest-a"]), vec![("devtest-a", true)], Some("⚠ AI key at cap: devtest-a")),
            (vec![full("devtest-foo")], pods(&["devtest-a"]), vec![("devtest-foo", false)], Some("⚠ AI key at cap: devtest-foo (no pod)")),
            (vec![paused_daily("devtest-a"), paused_new("devtest-b"), paused_unsaid("devtest-c"), off("devtest-d")], pods(&["devtest-a"]), vec![], None),
            (
                vec![spent_daily("devtest-a"), spent_life("devtest-b")],
                pods(&["devtest-a", "devtest-b"]),
                vec![("devtest-a", true), ("devtest-b", true)],
                Some("⚠ 2 AI keys at cap: devtest-a, devtest-b"),
            ),
            (
                vec![full("devtest-z"), full("devtest-y"), full("devtest-x"), full("devtest-w"), full("james-gpu"), off("devtest-v"), full("other-a")],
                pods(&["devtest-z", "devtest-y", "james-gpu"]),
                vec![("devtest-w", false), ("devtest-x", false), ("devtest-y", true), ("devtest-z", true), ("james-gpu", true)],
                Some("⚠ 5 AI keys at cap: devtest-w (no pod), devtest-x (no pod) +3 more"),
            ),
        ];
        for (keys, pods, want, lead) in cases {
            let c = UsageView::from_keys(&keys, None).cohort("devtest", &pods);
            let got: Vec<(&str, bool)> = c.capped.iter().map(|k| (k.name.as_str(), k.pod)).collect();
            assert_eq!(got, want, "{keys:?}");
            assert_eq!(c.capped_names().as_deref(), lead);
            assert_eq!(c.tui_parts().0, lead.map(|l| format!(" {l} ·")));
            assert_eq!(c.tui_parts().2, if lead.is_some() { Fill::Near } else { Fill::Ok });
        }
    }

    /// A name held twice shows the enabled key, then the newest; the cohort is the keys named
    /// `<prefix>-…` or after a listed pod; the bar warns when the credit is below what the
    /// enabled keys' caps allow, or gone, and names keys at their cap.
    #[test]
    fn usage_view_picks_one_key_per_name_and_sums_the_cohort() {
        let key = |hash: &str, name: &str, disabled: bool, created: &str, limit: f64, rem: f64, today: f64| KeyInfo {
            hash: hash.into(),
            name: Some(name.into()),
            disabled,
            created_at: Some(created.into()),
            limit: Some(limit),
            limit_remaining: Some(rem),
            usage_daily: Some(today),
            ..Default::default()
        };
        let keys = vec![
            key("old", "devtest-apple", true, "2026-10-01T00:00:00Z", 5.0, 5.0, 9.0),
            key("new", "devtest-apple", false, "2026-10-02T00:00:00Z", 5.0, 4.0, 1.0),
            key("newer-off", "devtest-apple", true, "2026-10-03T00:00:00Z", 5.0, 5.0, 9.0),
            key("b", "devtest-bloom", false, "2026-10-02T00:00:00Z", 5.0, 0.0, 2.0),
            key("j", "james-gpu", false, "2026-10-02T00:00:00Z", 10.0, 10.0, 0.5),
            key("p", "arena9-apple", false, "2026-10-02T00:00:00Z", 5.0, 5.0, 7.0),
        ];
        let v = UsageView::from_keys(&keys, Some(Credits { total: 20.0, used: 10.0 }));
        assert_eq!(v.keys["devtest-apple"].spent, Some(1.0), "the enabled key wins over newer disabled ones");
        assert_eq!(v.infos["devtest-apple"].hash, "new", "and its listing is kept, for planning a change to it");
        let c = v.cohort("devtest", &["james-gpu".to_string()]);
        // Every key counts in the totals (the disabled ones' spend today too); caps only the enabled.
        assert_eq!((c.keys, c.capped.len(), c.uncapped), (5, 1, 0));
        assert!((c.today.unwrap() - 21.5).abs() < 1e-9 && (c.headroom - 14.0).abs() < 1e-9, "{c:?}");
        let (lead, tail, fill) = c.tui_parts();
        assert_eq!(lead.as_deref(), Some(" ⚠ openrouter $10.00 left, the keys' caps allow $14.00 more · ⚠ AI key at cap: devtest-bloom (no pod) ·"));
        assert_eq!(tail.as_deref(), Some("openrouter $10.00 left · AI today $21.50"));
        assert_eq!(fill, Fill::Near);
        let gone = CohortUsage { credit_left: Some(0.0), ..c.clone() };
        assert_eq!(gone.tui_parts().2, Fill::Full);
        assert!(gone.tui_parts().0.unwrap().starts_with(" ‼ openrouter $0.00 left — every key's calls fail"));
        let calm = CohortUsage { credit_left: Some(100.0), capped: Vec::new(), ..c.clone() };
        assert_eq!(calm.tui_parts().0, None);
        let failed = CohortUsage { failed: true, uncapped: 2, ..calm };
        assert_eq!(failed.tui_parts().1.as_deref(), Some("openrouter $100.00 left · AI today $21.50 · 2 uncapped · (n/a)"));
        let nothing = UsageView { keys_error: Some("x".into()), ..Default::default() }.cohort("devtest", &[]);
        assert_eq!(nothing.tui_parts(), (None, Some("openrouter n/a".into()), Fill::Ok));
        assert_eq!(UsageView::default().cohort("devtest", &[]).tui_parts(), (None, None, Fill::Ok));
    }

    /// The keys and the credit are applied apart: a failure keeps its own part's last good
    /// answer, and the other part's fresh one still lands.
    #[test]
    fn usage_view_applies_keys_and_credit_apart() {
        let key = |name: &str, today: f64| KeyInfo { hash: name.into(), name: Some(name.into()), usage_daily: Some(today), ..Default::default() };
        let mut fresh = UsageView::default();
        assert!(!fresh.read);
        fresh.apply(Err("x".into()), Err("y".into()));
        assert!(fresh.read, "a failed read is still a read: `-`, not `…`");
        let mut v = UsageView::from_keys(&[key("devtest-a", 1.0)], Some(Credits { total: 100.0, used: 0.0 }));
        v.apply(Err("list timed out".into()), Ok(Credits { total: 100.0, used: 100.0 }));
        assert_eq!(v.credits.unwrap().left(), 0.0, "the fresh credit lands though the listing failed");
        assert_eq!((v.keys["devtest-a"].today, v.keys_error.as_deref()), (Some(1.0), Some("list timed out")));
        let c = v.cohort("devtest", &[]);
        assert!(c.failed && c.tui_parts().2 == Fill::Full, "{c:?}");
        v.apply(Ok(vec![key("devtest-a", 2.0)]), Err("credits 500".into()));
        assert_eq!((v.keys["devtest-a"].today, v.keys_error.as_deref()), (Some(2.0), None));
        assert_eq!((v.credits.unwrap().left(), v.credits_error.as_deref()), (0.0, Some("credits 500")));
    }

    /// The update body carries exactly the fields given — `null` for "no cap" and "never" —
    /// never `name` or `workspace_id`; a cap outside OpenRouter's range, or nothing to change, is
    /// refused before any call.
    #[test]
    fn update_body_sends_only_what_is_given() {
        use serde_json::json;
        let u = |limit, limit_reset, disabled| KeyUpdate { limit, limit_reset, disabled };
        for (update, want) in [
            (u(Some(Some(5.0)), None, None), json!({"limit": 5.0})),
            (u(Some(None), None, None), json!({"limit": null})),
            (u(None, Some(Some(LimitReset::Daily)), None), json!({"limit_reset": "daily"})),
            (u(None, Some(None), None), json!({"limit_reset": null})),
            (u(None, None, Some(true)), json!({"disabled": true})),
            (u(None, None, Some(false)), json!({"disabled": false})),
            (u(Some(Some(0.0)), Some(Some(LimitReset::Monthly)), None), json!({"limit": 0.0, "limit_reset": "monthly"})),
        ] {
            assert_eq!(update_body(&update).unwrap(), want, "{update:?}");
        }
        for bad in [-0.01, f64::NAN, f64::INFINITY, MAX_LIMIT, 2e6] {
            let e = update_body(&u(Some(Some(bad)), None, None)).unwrap_err().to_string();
            assert!(e.contains("a key's cap is USD 0 up to"), "{bad}: {e}");
        }
        assert!(update_body(&KeyUpdate::default()).unwrap_err().to_string().contains("nothing to change"));
    }

    #[test]
    fn limit_reset_reads_the_flag_and_the_listing() {
        assert_eq!(LimitReset::parse("Daily").unwrap(), Some(LimitReset::Daily));
        assert_eq!(LimitReset::parse("weekly").unwrap(), Some(LimitReset::Weekly));
        assert_eq!(LimitReset::parse(" monthly ").unwrap(), Some(LimitReset::Monthly));
        assert_eq!(LimitReset::parse("never").unwrap(), None);
        assert!(LimitReset::parse("hourly").is_err());
        let k = |r: Option<&str>| KeyInfo { limit_reset: r.map(String::from), ..Default::default() };
        assert_eq!(LimitReset::of(&k(Some("monthly"))), Some(LimitReset::Monthly));
        assert_eq!(LimitReset::of(&k(Some("yearly"))), None, "an unknown reset reads as none, as in the dashboard");
        assert_eq!(LimitReset::of(&k(None)), None);
    }

    /// `10`, `2.50`, `$3` set the cap, `+5` raises it; words for "no cap", negatives, exponents
    /// and the ceiling are refused, each saying what to give instead.
    #[test]
    fn parse_cap_takes_amounts_and_raises_only() {
        for (typed, want) in [
            ("10", CapChange::To(10.0)),
            (" 2.50 ", CapChange::To(2.5)),
            ("$3", CapChange::To(3.0)),
            ("0", CapChange::To(0.0)),
            (".5", CapChange::To(0.5)),
            ("+5", CapChange::Raise(5.0)),
            ("+0.05", CapChange::Raise(0.05)),
            ("+ 5", CapChange::Raise(5.0)),
            ("+$2", CapChange::Raise(2.0)),
            ("+0", CapChange::Raise(0.0)),
        ] {
            assert_eq!(parse_cap(typed), Ok(want), "{typed}");
        }
        for (typed, says) in [
            ("none", "--no-cap"),
            ("unlimited", "--no-cap"),
            ("-1", "not a negative raise"),
            ("1e3", "isn't an amount"),
            ("inf", "--no-cap"),
            ("nan", "isn't an amount"),
            ("", "isn't an amount"),
            ("+", "isn't an amount"),
            (".", "isn't an amount"),
            ("1.2.3", "isn't an amount"),
            ("5usd", "isn't an amount"),
            ("1000000", "under $1000000"),
        ] {
            let e = parse_cap(typed).unwrap_err();
            assert!(e.contains(says), "{typed}: {e}");
        }
    }

    /// A key on a `reset` cap, with the period counters the listing gives.
    fn capped(limit: Option<f64>, remaining: Option<f64>, reset: Option<&str>) -> KeyInfo {
        KeyInfo {
            hash: "h".into(),
            name: Some("devtest-a".into()),
            limit,
            limit_remaining: remaining,
            limit_reset: reset.map(String::from),
            usage: Some(30.0),
            usage_daily: Some(0.2),
            usage_weekly: Some(1.0),
            usage_monthly: Some(2.6),
            ..Default::default()
        }
    }

    /// What a key has spent against a cap: the cap's period's own counter, else (same reset only)
    /// OpenRouter's limit − remaining, which stops at the cap.
    #[test]
    fn spent_against_reads_the_new_periods_counter() {
        let k = capped(Some(2.5), Some(0.0), Some("monthly"));
        assert_eq!(spent_against(&k, Some(LimitReset::Monthly)), Some(2.6), "the month's own $2.60, not limit − remaining's $2.50");
        assert_eq!(spent_against(&k, Some(LimitReset::Daily)), Some(0.2));
        assert_eq!(spent_against(&k, Some(LimitReset::Weekly)), Some(1.0));
        assert_eq!(spent_against(&k, None), Some(30.0));
        let bare = KeyInfo { usage_monthly: None, usage: None, ..k.clone() };
        assert_eq!(spent_against(&bare, Some(LimitReset::Monthly)), Some(2.5), "no counter: limit − remaining, same reset");
        assert_eq!(spent_against(&bare, None), None, "another reset: unknown, never the old period's");
    }

    /// Absolute, relative, no cap and a new reset, against what the key spent: a tightening
    /// below the spend (or past an unknown one) is refused unless forced; a raise that stays under
    /// it goes ahead and says so; float noise is rounded off.
    #[test]
    fn plan_cap_raises_sets_and_refuses_tightening_below_the_spend() {
        let monthly = capped(Some(5.0), Some(2.4), Some("monthly")); // $2.60 this month
        let ok = |k: &KeyInfo, c: Option<CapChange>, r: Option<Option<LimitReset>>| plan_cap(k, c, r, false).unwrap();
        let p = ok(&monthly, Some(CapChange::Raise(5.0)), None);
        assert_eq!((p.before, p.after, p.spent, p.changes(), p.still_at_cap()), (Some(5.0), Some(10.0), Some(2.6), true, false));
        assert_eq!(p.words(), "cap $5.00 → $10.00 (monthly) · spent $2.60 this month");
        assert_eq!(p.update(), KeyUpdate { limit: Some(Some(10.0)), ..Default::default() });
        // Lower, still above the spend: fine. Below it: refused, forced: done.
        assert_eq!(ok(&monthly, Some(CapChange::To(3.0)), None).after, Some(3.0));
        let e = plan_cap(&monthly, Some(CapChange::To(2.0)), None, false).unwrap_err();
        assert!(e.contains("a $2.00 cap is under what it has spent this month ($2.60)") && e.contains("--force"), "{e}");
        let forced = plan_cap(&monthly, Some(CapChange::To(2.0)), None, true).unwrap();
        assert!(forced.still_at_cap() && forced.words().ends_with("— still at its cap"), "{}", forced.words());
        // The same cap: nothing to do.
        let same = ok(&monthly, Some(CapChange::To(5.0)), None);
        assert!(!same.changes());
        assert_eq!(same.words(), "cap $5.00 (monthly) · spent $2.60 this month");
        // A raise that stays under the spend isn't refused; it says the key is still at its cap.
        let over = capped(Some(2.5), Some(0.0), Some("monthly"));
        let p = ok(&over, Some(CapChange::Raise(0.05)), None);
        assert_eq!(p.after, Some(2.55));
        assert!(p.still_at_cap(), "{p:?}");
        // Float noise: 0.2 + 0.05 is 0.25, not 0.25000000000000006.
        let tiny = capped(Some(0.2), Some(0.2), None);
        assert_eq!(ok(&tiny, Some(CapChange::Raise(0.05)), None).after, Some(0.25));
        // No cap: only by asking; a raise of a key without one is refused.
        let p = ok(&monthly, Some(CapChange::NoCap), None);
        assert_eq!((p.after, p.update()), (None, KeyUpdate { limit: Some(None), ..Default::default() }));
        assert!(p.words().starts_with("cap $5.00 → no cap"), "{}", p.words());
        let open = capped(None, None, None);
        assert!(plan_cap(&open, Some(CapChange::Raise(5.0)), None, false).unwrap_err().contains("no cap to raise"));
        // A first cap on an uncapped key is a tightening: under its life's $30 refused.
        assert!(plan_cap(&open, Some(CapChange::To(10.0)), None, false).unwrap_err().contains("spent in all ($30.00)"));
        assert_eq!(ok(&open, Some(CapChange::To(10.0)), Some(Some(LimitReset::Daily))).after, Some(10.0), "today's $0.20 is under it");
        // Another reset, the cap kept (`+0`): the new period's spend decides.
        let daily = capped(Some(5.0), Some(4.8), Some("daily"));
        let e = plan_cap(&daily, Some(CapChange::Raise(0.0)), Some(None), false).unwrap_err();
        assert!(e.contains("spent in all ($30.00)"), "never resetting, its whole life counts: {e}");
        let p = ok(&daily, Some(CapChange::Raise(0.0)), Some(Some(LimitReset::Weekly)));
        assert_eq!(p.update(), KeyUpdate { limit_reset: Some(Some(LimitReset::Weekly)), ..Default::default() });
        assert_eq!(p.words(), "cap $5.00, resets daily → weekly · spent $1.00 this week");
        assert_eq!(ok(&daily, None, Some(Some(LimitReset::Weekly))), p, "no amount: the cap stays");
        // Spend unknown: a tightening is refused unless forced; a raise isn't.
        let unknown = KeyInfo { usage_monthly: None, limit_remaining: None, ..monthly.clone() };
        assert!(plan_cap(&unknown, Some(CapChange::To(4.0)), None, false).unwrap_err().contains("isn't known"));
        assert!(plan_cap(&unknown, Some(CapChange::To(4.0)), None, true).is_ok());
        assert_eq!(ok(&unknown, Some(CapChange::Raise(1.0)), None).words(), "cap $5.00 → $6.00 (monthly) · spent ? this month");
        // The ceiling.
        assert!(plan_cap(&capped(Some(999_999.0), Some(999_999.0), None), Some(CapChange::Raise(5.0)), None, false).unwrap_err().contains("ceiling"));
    }

    /// One key per name: the enabled one, then the newest, the first of equals — as the
    /// dashboard's cell shows.
    #[test]
    fn shown_key_prefers_enabled_then_newest() {
        let k = |h: &str, disabled: bool, created: Option<&str>| KeyInfo { hash: h.into(), disabled, created_at: created.map(String::from), ..Default::default() };
        let keys = [k("old-off", true, Some("2026-10-01")), k("on", false, Some("2026-10-02")), k("new-off", true, Some("2026-10-03"))];
        assert_eq!(shown_key(&keys).unwrap().hash, "on");
        let off = [k("a", true, Some("2026-10-01")), k("b", true, Some("2026-10-03")), k("c", true, Some("2026-10-02"))];
        assert_eq!(shown_key(&off).unwrap().hash, "b");
        let twins = [k("first", false, None), k("second", false, None)];
        assert_eq!(shown_key(&twins).unwrap().hash, "first");
        assert!(shown_key(&[] as &[KeyInfo]).is_none());
    }

    /// An update on the wire: the key read first (the workspace guard), then one `PATCH` with
    /// only the given fields; a key in another workspace is refused with no `PATCH`; an error
    /// status is classified; an update that changes nothing, or a bad hash, sends nothing.
    #[tokio::test]
    async fn update_key_reads_first_and_patches_only_the_given_fields() {
        use crate::error::ProviderErrorKind as K;
        use crate::http::test_server::{canned, serve};
        let json = "application/json";
        let ours = "c9f89f20-08e3-4cff-bad6-70c478e2c2c4";
        let mine = format!(r#"{{"data":{{"hash":"a","workspace_id":"{ours}"}}}}"#);
        let srv = serve(vec![
            canned(200, json, &format!(r#"{{"data":{{"id":"{ours}","slug":"arena-dev-test"}}}}"#)),
            canned(200, json, &mine),
            canned(200, json, r#"{"data":{"hash":"a","limit":0.2}}"#),
            canned(200, json, r#"{"data":{"hash":"x","workspace_id":"a3339b09-7feb-5818-be3e-1b9484142ece"}}"#),
            canned(200, json, &mine),
            canned(200, "text/plain", "ok (not json)"),
            canned(200, json, &mine),
            canned(401, "text/html", "<html>Unauthorized</html>"),
        ]);
        let or = test_client("K", &srv.base).with_workspace(Some("arena-dev-test"));
        let cap = KeyUpdate { limit: Some(Some(0.2)), ..Default::default() };
        or.update_key("a", &cap).await.unwrap();
        let e = or.update_key("x", &cap).await.unwrap_err().to_string();
        assert!(e.contains("refused") && e.contains("a3339b09"), "{e}");
        let pause = KeyUpdate { disabled: Some(true), ..Default::default() };
        or.update_key("a", &pause).await.unwrap();
        let e = or.update_key("a", &KeyUpdate { limit: Some(None), limit_reset: Some(None), ..Default::default() }).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("openrouter update key HTTP 401"), "{e}");
        assert!(or.update_key("a", &KeyUpdate::default()).await.is_err());
        assert!(or.update_key("a/../credits", &cap).await.is_err());
        assert_eq!(
            *srv.requests.lock().unwrap(),
            [
                "GET /workspaces/arena-dev-test HTTP/1.1",
                "GET /keys/a HTTP/1.1",
                "PATCH /keys/a HTTP/1.1",
                "GET /keys/x HTTP/1.1",
                "GET /keys/a HTTP/1.1",
                "PATCH /keys/a HTTP/1.1",
                "GET /keys/a HTTP/1.1",
                "PATCH /keys/a HTTP/1.1",
            ],
            "no PATCH for x; nothing for the empty update or the bad hash"
        );
        let patches: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(
            patches,
            [serde_json::json!({"limit": 0.2}), serde_json::json!({"disabled": true}), serde_json::json!({"limit": null, "limit_reset": null})]
        );
    }
}
