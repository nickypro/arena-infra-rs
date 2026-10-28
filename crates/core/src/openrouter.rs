//! OpenRouter **provisioning** API: mint / list / delete the runtime API keys we hand
//! out to the cohort (one per machine), so keys can be generated, rotated, and revoked
//! programmatically — e.g. to recover from a leak — and read what each has spent.
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
    /// workspace); an id as given; a slug looked up once (`GET /workspaces/{slug}`). An
    /// error, never `None`, when it can't be resolved: an unscoped call is what the setting
    /// is there to prevent.
    pub async fn workspace_id(&self) -> Result<Option<String>> {
        let Some(ws) = &self.workspace else { return Ok(None) };
        match workspace_ref(ws)? {
            WorkspaceRef::Id(id) => return Ok(Some(id)),
            WorkspaceRef::Slug => {}
        }
        if let Some(id) = self.resolved.lock().unwrap().clone() {
            return Ok(Some(id));
        }
        let url = format!("{}/workspaces/{ws}", self.root);
        let v = send_json(self.auth(self.client.get(url)), &format!("openrouter workspace `{ws}`")).await?;
        let id = v
            .get("data")
            .and_then(|d| d.get("id"))
            .and_then(|s| s.as_str())
            .and_then(|s| match workspace_ref(s) {
                Ok(WorkspaceRef::Id(id)) => Some(id),
                _ => None,
            })
            .ok_or_else(|| Error::provider(format!("openrouter workspace `{ws}`: the answer had no workspace id")))?;
        *self.resolved.lock().unwrap() = Some(id.clone());
        Ok(Some(id))
    }

    /// Create a runtime key named `name` with an optional USD credit `limit` (in the
    /// configured workspace, if any).
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
        let hash = v
            .get("data")
            .and_then(|d| d.get("hash"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(CreatedKey { secret, hash, name: name.to_string() })
    }

    /// List **every** provisioned key in the workspace (the Default one when none is
    /// configured), disabled ones included. The API returns one page (the most recent 100)
    /// per call, so this walks `offset` until a page comes back empty ([`collect_pages`]).
    /// A single page used to be read: on an account holding more keys (several cohorts +
    /// admin keys) a machine's key could sit past it, and "no key" then meant revoke dropped
    /// the CSV row of a key that still worked. With a workspace, a key that doesn't say it's
    /// in it is dropped ([`in_workspace`]) whatever the API answered.
    pub async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        let ws = self.workspace_id().await?;
        let all = collect_pages(|offset| self.list_page(offset, ws.as_deref())).await?;
        Ok(in_workspace(all, ws.as_deref()))
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
        Ok(arr.iter().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect())
    }

    /// Refuse to change a key outside the configured workspace: read it (`GET /keys/{hash}`)
    /// and compare. Nothing to check without a workspace.
    async fn ensure_in_workspace(&self, hash: &str, what: &str) -> Result<()> {
        let Some(ws) = self.workspace_id().await? else { return Ok(()) };
        let v = send_json(self.auth(self.client.get(format!("{}/{hash}", self.keys_url()))), &format!("openrouter {what}"))
            .await?;
        let theirs = v.get("data").and_then(|d| d.get("workspace_id")).and_then(|s| s.as_str());
        if theirs.is_some_and(|t| t.eq_ignore_ascii_case(&ws)) {
            return Ok(());
        }
        Err(Error::provider(format!(
            "openrouter {what}: refused — key {} is in workspace {}, not OPENROUTER_WORKSPACE ({})",
            &hash[..hash.len().min(12)],
            theirs.unwrap_or("(not said)"),
            self.workspace.as_deref().unwrap_or_default(),
        )))
    }

    /// Delete the key with `hash` (irreversible — the runtime key stops working). With a
    /// workspace, only a key in it.
    pub async fn delete_key(&self, hash: &str) -> Result<()> {
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
        self.ensure_in_workspace(hash, "rename key").await?;
        // Status only ([`send_ok`]): a rename that worked is never reported as failed
        // because of its (unused) body.
        send_ok(
            self.auth(self.client.patch(format!("{}/{hash}", self.keys_url()))).json(&rename_body(new_name)),
            "openrouter rename key",
        )
        .await
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

/// The keys of `keys` in workspace `ws` (all of them without one). A key that doesn't say
/// which workspace it's in isn't kept: the listing already asked for `ws`, so this only
/// matters if the API ignored that — and then the safe reading is "not ours".
pub fn in_workspace(keys: Vec<KeyInfo>, ws: Option<&str>) -> Vec<KeyInfo> {
    match ws {
        None => keys,
        Some(ws) => keys.into_iter().filter(|k| k.workspace_id.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(ws))).collect(),
    }
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
    /// Spent in the cap's current period — OpenRouter's own `limit − limit_remaining`, so a
    /// cap that starts over daily counts today's spend, not the key's life; without both of
    /// those, the life's `usage`.
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

/// A cap as it was set: `5`, `2.50`.
fn cap_amount(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e9 {
        format!("{x:.0}")
    } else {
        short_amount(x)
    }
}

impl KeyUsage {
    pub fn of(k: &KeyInfo) -> Self {
        let spent = match (k.limit, k.limit_remaining) {
            (Some(l), Some(r)) => Some((l - r).max(0.0)),
            _ => k.usage,
        };
        KeyUsage {
            disabled: k.disabled,
            spent,
            limit: k.limit,
            reset: k.limit_reset.clone().filter(|r| period(Some(r)).is_some()),
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

    pub fn fill(&self) -> Fill {
        if self.disabled {
            return Fill::Full;
        }
        match (self.spent, self.limit) {
            (_, Some(l)) if l <= 0.0 => Fill::Full,
            (Some(s), Some(l)) if s >= l - 0.005 => Fill::Full,
            (Some(s), Some(l)) if s >= 0.8 * l => Fill::Near,
            _ => Fill::Ok,
        }
    }

    /// The `AI$` cell: `1.23/5` against a cap, `$0.42` without one, `off` when disabled,
    /// `?` when the listing gave no spend.
    pub fn cell(&self) -> String {
        if self.disabled {
            return "off".into();
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

/// What the dashboard knows of OpenRouter after its last read: every listed key's usage by
/// name (a key is named after its pod: [`key_name`]) and the account's credit.
#[derive(Debug, Clone, Default)]
pub struct UsageView {
    pub keys: HashMap<String, KeyUsage>,
    pub credits: Option<Credits>,
    /// Why the last read failed, if it did; what's kept is then the last good answer.
    pub error: Option<String>,
}

impl UsageView {
    /// From a listing. A name held by more than one key (an old one disabled, a rotation's
    /// leftover) shows the enabled one, then the newest.
    pub fn from_keys(keys: &[KeyInfo], credits: Option<Credits>) -> Self {
        let mut best: HashMap<String, &KeyInfo> = HashMap::new();
        for k in keys {
            let Some(name) = k.name.as_deref().filter(|n| !n.is_empty()) else { continue };
            let rank = |k: &KeyInfo| (!k.disabled, k.created_at.clone());
            match best.get(name) {
                Some(b) if rank(b) >= rank(k) => {}
                _ => {
                    best.insert(name.to_string(), k);
                }
            }
        }
        let keys = best.into_iter().map(|(n, k)| (n, KeyUsage::of(k))).collect();
        UsageView { keys, credits, error: None }
    }

    /// This cohort's share: the keys named `<prefix>-…` or after one of `pods` (an `@name`
    /// pod's key carries its bare name), summed for the summary bar.
    pub fn cohort(&self, prefix: &str, pods: &[String]) -> CohortUsage {
        let needle = format!("{prefix}-");
        let mut c = CohortUsage { credit_left: self.credits.map(|c| c.left()), failed: self.error.is_some(), ..Default::default() };
        for (name, k) in &self.keys {
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
                c.at_cap += 1;
            }
            if let Some(left) = k.left() {
                c.headroom += left;
            }
        }
        c
    }
}

/// The cohort's keys added up, and the account's credit, for the summary bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CohortUsage {
    pub keys: usize,
    /// Spent today by the cohort's keys (`None`: no key said).
    pub today: Option<f64>,
    /// Enabled keys at their cap: those participants' AI calls fail.
    pub at_cap: usize,
    /// What the enabled keys may still spend under their caps.
    pub headroom: f64,
    pub credit_left: Option<f64>,
    /// The last read failed (the numbers are the last good ones).
    pub failed: bool,
}

impl CohortUsage {
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
        if self.at_cap > 0 {
            let keys = if self.at_cap == 1 { "key" } else { "keys" };
            lead.push(format!("⚠ {} AI {keys} at cap", self.at_cap));
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

    #[test]
    fn in_workspace_keeps_only_keys_that_say_they_are_in_it() {
        let k = |h: &str, ws: Option<&str>| KeyInfo { hash: h.into(), workspace_id: ws.map(String::from), ..Default::default() };
        let keys = vec![k("a", Some("w1")), k("b", Some("W1")), k("c", Some("w2")), k("d", None)];
        let hashes = |v: Vec<KeyInfo>| v.into_iter().map(|k| k.hash).collect::<Vec<_>>();
        assert_eq!(hashes(in_workspace(keys.clone(), Some("w1"))), ["a", "b"]);
        assert_eq!(hashes(in_workspace(keys, None)), ["a", "b", "c", "d"], "no workspace: as before");
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
            canned(200, json, &format!(r#"{{"data":[{{"hash":"a","name":"devtest-apple","workspace_id":"{ours}"}},{{"hash":"x","name":"arena9-apple","workspace_id":"other"}}]}}"#)),
            canned(200, json, r#"{"data":[]}"#),
            canned(200, json, r#"{"key":"sk-or-v1-S","data":{"hash":"n"}}"#),
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
            (Some(2.5), None, Some(2.6), "2.60/2.50", Fill::Full),
            (None, None, Some(123.4), "$123", Fill::Ok),
            (Some(0.0), None, Some(0.0), "0.00/0", Fill::Full),
            (Some(5.0), None, None, "?", Fill::Ok),
        ] {
            let k = usage(limit, rem, total, None);
            assert_eq!((k.cell().as_str(), k.fill()), (cell, fill), "{limit:?} {rem:?} {total:?}");
        }
        let none = usage(None, None, Some(0.5), None);
        assert!(none.detail().starts_with("$0.50 spent, no cap · today $0.20"), "{}", none.detail());
        let mut off = usage(Some(5.0), Some(5.0), Some(0.0), Some("bogus"));
        off.disabled = true;
        assert_eq!((off.cell().as_str(), off.fill()), ("off", Fill::Full));
        assert!(off.detail().starts_with("disabled · $0.00 of $5.00 ($5.00 left)"), "an unknown reset says no period: {}", off.detail());
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
        let c = v.cohort("devtest", &["james-gpu".to_string()]);
        assert_eq!((c.keys, c.at_cap), (3, 1));
        assert!((c.today.unwrap() - 3.5).abs() < 1e-9 && (c.headroom - 14.0).abs() < 1e-9, "{c:?}");
        let (lead, tail, fill) = c.tui_parts();
        assert_eq!(lead.as_deref(), Some(" ⚠ openrouter $10.00 left, the keys' caps allow $14.00 more · ⚠ 1 AI key at cap ·"));
        assert_eq!(tail.as_deref(), Some("openrouter $10.00 left · AI today $3.50"));
        assert_eq!(fill, Fill::Near);
        let gone = CohortUsage { credit_left: Some(0.0), ..c.clone() };
        assert_eq!(gone.tui_parts().2, Fill::Full);
        assert!(gone.tui_parts().0.unwrap().starts_with(" ‼ openrouter $0.00 left — every key's calls fail"));
        let calm = CohortUsage { credit_left: Some(100.0), at_cap: 0, ..c.clone() };
        assert_eq!(calm.tui_parts().0, None);
        let failed = CohortUsage { failed: true, ..calm };
        assert_eq!(failed.tui_parts().1.as_deref(), Some("openrouter $100.00 left · AI today $3.50 · (n/a)"));
        let nothing = UsageView { error: Some("x".into()), ..Default::default() }.cohort("devtest", &[]);
        assert_eq!(nothing.tui_parts(), (None, Some("openrouter n/a".into()), Fill::Ok));
        assert_eq!(UsageView::default().cohort("devtest", &[]).tui_parts(), (None, None, Fill::Ok));
    }
}
