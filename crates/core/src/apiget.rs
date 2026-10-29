//! `arena api get <provider> <path>`: a **read-only** passthrough to a provider's REST API,
//! for the endpoints the tool has no command for (`pods/<id>`, `catalog/datacenters`,
//! `network-volumes`, `billing/pods`, Vast's `users/current/`, Hetzner's `locations`…).
//!
//! The guard rails, each a pure function tested here:
//! - **GET only.** There is no verb option: nothing this command sends can change anything.
//! - **The key goes only to the provider.** The path is relative to the provider's API base
//!   and [`resolve_url`] refuses anything that could point elsewhere — an absolute URL, a
//!   `//host`, `.`/`..` segments — then checks the built URL is still on the base's origin,
//!   under its path. Redirects are not followed (a 3xx is reported, not chased), and the
//!   key travels in the `Authorization` header, never the URL.
//! - **Secrets are redacted** from what's printed ([`redact`]): `env` blocks, values under
//!   key/token/secret/password-like names, and token- or key-shaped strings (`rpa_…`,
//!   `sk-…`, `hf_…`, SSH key material, PEM blocks). `--raw` prints the body as received —
//!   except the configured API key itself, which is never printed ([`scrub`]).

use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde_json::Value;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::status_error;
use crate::provider::RunpodApi;

/// What a redacted value is replaced with.
pub const REDACTED: &str = "<redacted>";

/// What the configured API key is replaced with, `--raw` included.
pub const KEY_SHOWN_AS: &str = "<api key>";

/// The whole request's budget: read-only, so a stalled API just fails the command.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// One provider API the passthrough can read: its base URL and the config key holding its
/// API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiBase {
    pub provider: &'static str,
    pub url: &'static str,
    pub key: &'static str,
}

/// The provider names `api get` takes.
pub const PROVIDERS: &str = "runpod (follows RUNPOD_API), runpod-v1, runpod-v2, vast, hetzner, shadeform";

/// The API base for `provider`: `runpod` is the generation `RUNPOD_API` selects (as every
/// other command), `runpod-v1`/`runpod-v2` name one explicitly. Pure.
pub fn api_base(provider: &str, cfg: &Config) -> Result<ApiBase> {
    use crate::provider::{hetzner, runpod, runpod_v2, shadeform, vast};
    let v1 = ApiBase { provider: "runpod-v1", url: runpod::BASE, key: "RUNPOD_API_KEY" };
    let v2 = ApiBase { provider: "runpod-v2", url: runpod_v2::BASE, key: "RUNPOD_API_KEY" };
    Ok(match provider.trim().to_ascii_lowercase().as_str() {
        "runpod" => match RunpodApi::from_config(cfg)? {
            RunpodApi::V1 => v1,
            RunpodApi::V2 => v2,
        },
        "runpod-v1" => v1,
        "runpod-v2" => v2,
        "vast" => ApiBase { provider: "vast", url: vast::BASE, key: "VAST_API_KEY" },
        "hetzner" => ApiBase { provider: "hetzner", url: hetzner::BASE, key: "HETZNER_API_KEY" },
        "shadeform" => ApiBase { provider: "shadeform", url: shadeform::BASE, key: "SHADEFORM_API_KEY" },
        other => return Err(Error::Config(format!("api get: unknown provider `{other}` (known: {PROVIDERS})"))),
    })
}

impl ApiBase {
    /// The header the key travels in: Shadeform takes it as `X-API-KEY` (`None`: the bearer
    /// `Authorization` header, every other API here).
    pub fn key_header(&self) -> Option<&'static str> {
        (self.provider == "shadeform").then_some("X-API-KEY")
    }
}

/// Refuse `--raw` where the body carries a secret the provider itself holds for us:
/// Shadeform's `/sshkeys` (and `/sshkeys/{id}/info`) return the managed key's **private half**
/// — printed raw it would land in the terminal, shell history and logs. Without `--raw` the
/// passthrough redacts it (`private_key` is a secret name).
///
/// Judged on the path as the API will route it, not as typed (review SF2): the typed path and
/// the resolved URL's path are each percent-decoded (twice: `%2573`), lowercased, split on `/`
/// with empty and `.` segments dropped, and a leading copy of the base's own path (`v1/` —
/// the docs' spelling, which [`resolve_url`] strips too) removed; any that then starts with a
/// `sshkeys…` segment is refused. And [`render_for`] blanks a private key in Shadeform output
/// even with `--raw`, should a spelling ever slip past this. Pure.
pub fn refuse_raw(base: &ApiBase, path: &str, raw: bool) -> Result<()> {
    if raw && base.provider == "shadeform" && reaches_sshkeys(base, path) {
        return Err(Error::Config(
            "api get shadeform sshkeys… --raw is refused: Shadeform returns the managed SSH key's private half there — \
             drop --raw (it's redacted)"
                .into(),
        ));
    }
    Ok(())
}

/// `%XX` decoded (invalid escapes kept as typed). Pure.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path's segments as a router would see them: query/fragment cut, percent-decoded (twice),
/// lowercased, empty and `.` segments dropped. Pure.
fn canonical_segments(path: &str) -> Vec<String> {
    let p = path.trim().split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(&percent_decode(p)).to_ascii_lowercase();
    decoded.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty() && *s != ".").map(String::from).collect()
}

/// Whether `path` (typed, or as [`resolve_url`] resolves it under `base`) reaches a `sshkeys…`
/// route once the base's own path (`v1`) is taken off the front. Pure.
fn reaches_sshkeys(base: &ApiBase, path: &str) -> bool {
    let base_segs = Url::parse(base.url).map(|u| canonical_segments(u.path())).unwrap_or_default();
    let mut candidates = vec![canonical_segments(path)];
    if let Ok(url) = resolve_url(base.url, path) {
        candidates.push(canonical_segments(url.path()));
    }
    candidates.into_iter().any(|mut segs| {
        while !base_segs.is_empty() && segs.starts_with(&base_segs) {
            segs.drain(..base_segs.len());
        }
        segs.first().is_some_and(|s| s.starts_with("sshkeys"))
    })
}

/// Whether `p` starts with a URL scheme (`https:`, `file:`, `javascript:` …): letters, then
/// letters/digits/`+.-`, then `:` — before any `/` or `?`.
fn has_scheme(p: &str) -> bool {
    let head = p.split(['/', '?']).next().unwrap_or("");
    match head.split_once(':') {
        Some((scheme, _)) => {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        }
        None => false,
    }
}

/// A `.`/`..` path segment, spelled out or percent-encoded (`%2e`).
fn dot_segment(seg: &str) -> bool {
    let decoded = seg.to_ascii_lowercase().replace("%2e", ".");
    decoded == "." || decoded == ".."
}

/// The URL for `path` under `base` — or a refusal. `path` is relative to the base (`pods`,
/// `pods/abc`, `catalog/gpus?cloud=COMMUNITY`; one leading `/` is fine). Refused: nothing,
/// whitespace/control characters, a scheme or `//host` (an absolute URL would send the key
/// wherever it points), backslashes, a `#fragment`, `.`/`..` segments. Then the built URL
/// must still be on the base's scheme/host/port, without credentials, under the base path —
/// whatever the parser made of it. Pure.
pub fn resolve_url(base: &str, path: &str) -> Result<Url> {
    let refuse = |why: &str| {
        Error::Config(format!("api get: {why} — give a path relative to {base}, e.g. `pods` or `pods/<id>`"))
    };
    let p = path.trim();
    if p.is_empty() {
        return Err(refuse("no path"));
    }
    if p.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(refuse("the path has whitespace or control characters"));
    }
    if p.starts_with("//") || p.contains("://") || has_scheme(p) {
        return Err(refuse("absolute URLs are refused (the API key would go wherever it points)"));
    }
    if p.contains('\\') {
        return Err(refuse("backslashes are refused"));
    }
    if p.contains('#') {
        return Err(refuse("a #fragment is refused"));
    }
    let rel = p.strip_prefix('/').unwrap_or(p);
    let path_part = rel.split('?').next().unwrap_or("");
    if path_part.split('/').any(dot_segment) {
        return Err(refuse("`.`/`..` path segments are refused"));
    }
    let base_url = Url::parse(base).map_err(|e| Error::Config(format!("api get: bad base {base}: {e}")))?;
    let rel = strip_version_prefix(base_url.path(), rel).map_err(|why| refuse(&why))?;
    let url = Url::parse(&format!("{}/{rel}", base.trim_end_matches('/'))).map_err(|e| refuse(&format!("not a valid path ({e})")))?;
    let under = format!("{}/", base_url.path().trim_end_matches('/'));
    let same_origin = url.scheme() == base_url.scheme()
        && url.host_str() == base_url.host_str()
        && url.port_or_known_default() == base_url.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none();
    if !same_origin || !url.path().starts_with(&under) || url.fragment().is_some() {
        return Err(refuse("the path leaves the API base"));
    }
    Ok(url)
}

/// `rel` without a leading copy of the base's own path (`/v2/pods` on `…/v2` → `pods`;
/// Vast's `api/v0/instances/` on `…/api/v0` → `instances/`): pasted from RunPod's docs, such
/// a path was appended to the base as `…/v2/v2/pods` — a 404 that didn't say why (wave-6
/// live finding). A leading *other* version segment (`v1/…` against v2) is refused, naming
/// the base and the provider name that reaches that version — never silently sent to
/// `…/v2/v1/…`. Pure.
fn strip_version_prefix<'a>(base_path: &str, rel: &'a str) -> std::result::Result<&'a str, String> {
    let base = base_path.trim_matches('/');
    if !base.is_empty() {
        if rel == base {
            return Ok("");
        }
        if let Some(rest) = rel.strip_prefix(base).and_then(|r| r.strip_prefix('/')) {
            return Ok(rest);
        }
    }
    let first = rel.split(['/', '?']).next().unwrap_or("");
    let versionish = |seg: &str| {
        seg.len() >= 2 && seg.starts_with(['v', 'V']) && seg[1..].chars().all(|c| c.is_ascii_digit())
    };
    if versionish(first) && !base.split('/').any(|b| b.eq_ignore_ascii_case(first)) {
        let hint = match first.to_ascii_lowercase().as_str() {
            "v1" if base.ends_with("v2") => " — for RunPod's v1 API use `arena api get runpod-v1 …`",
            "v2" if base.ends_with("v1") => " — for RunPod's v2 API use `arena api get runpod-v2 …`",
            _ => "",
        };
        return Err(format!(
            "the path starts with `{first}/`, but paths are relative to this API's base (it would go to /{base}/{rel}){hint}"
        ));
    }
    Ok(rel)
}

/// Object keys whose value is secret as a whole — an `env` block, keys, tokens, passwords.
/// Matched on the lowercased name with `-` read as `_`; broad on purpose (over-redacting a
/// harmless field costs nothing; `--raw` shows it).
fn secret_key(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace('-', "_");
    matches!(n.as_str(), "env" | "envs" | "extra_env" | "environment" | "key" | "keys")
        || n.ends_with("_key")
        || n.ends_with("apikey")
        || ["token", "secret", "password", "passwd", "credential", "auth", "cookie", "login"].iter().any(|w| n.contains(w))
}

/// Every string (and number) inside a secret value replaced; the structure — an env block's
/// variable names — kept, so it's still visible *which* secrets are there.
fn blank(v: &Value) -> Value {
    match v {
        Value::String(_) | Value::Number(_) => Value::String(REDACTED.into()),
        Value::Array(a) => Value::Array(a.iter().map(blank).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), blank(v))).collect()),
        other => other.clone(),
    }
}

/// Characters a token is made of.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Token prefixes redacted wherever they start a word: RunPod (`rpa_`), OpenAI/Anthropic/
/// OpenRouter (`sk-`), Hugging Face (`hf_`), GitHub (`ghp_`, `gho_`, `github_pat_`).
const TOKEN_PREFIXES: &[&str] = &["rpa_", "sk-", "hf_", "ghp_", "gho_", "github_pat_"];

/// Shortest run after a prefix that counts as a token (so `sk-learn` stays readable).
const MIN_TOKEN_TAIL: usize = 8;

/// Shortest base64 run starting `AAAA` that counts as SSH key material.
const MIN_KEY_BLOB: usize = 40;

/// Free text with token- and key-shaped words redacted: a word starting with one of
/// [`TOKEN_PREFIXES`] (followed by ≥ 8 token characters), and SSH public-key material (a
/// base64 run of ≥ 40 starting `AAAA` — the type word before it stays). A PEM block (`-----BEGIN
/// …`) redacts the whole string. Pure.
pub fn redact_text(s: &str) -> String {
    if s.contains("-----BEGIN") {
        return REDACTED.into();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let boundary = i == 0 || !token_char(chars[i - 1]);
        if boundary {
            let rest: String = chars[i..].iter().take(16).collect();
            let token = TOKEN_PREFIXES.iter().find(|p| rest.starts_with(**p)).and_then(|p| {
                let start = i + p.chars().count();
                let tail = chars[start..].iter().take_while(|c| token_char(**c)).count();
                (tail >= MIN_TOKEN_TAIL).then_some(start + tail)
            });
            let blob = rest.starts_with("AAAA").then(|| {
                let n = chars[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')).count();
                i + n
            });
            let end = token.or(blob.filter(|end| end - i >= MIN_KEY_BLOB));
            if let Some(end) = end {
                out.push_str(REDACTED);
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A response body with its secrets redacted (see the module doc): values under
/// [`secret_key`] names blanked, every other string through [`redact_text`]. Pure.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter().map(|(k, v)| (k.clone(), if secret_key(k) { blank(v) } else { redact(v) })).collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::String(s) => Value::String(redact_text(s)),
        other => other.clone(),
    }
}

/// `text` with every occurrence of the configured API key replaced by [`KEY_SHOWN_AS`] —
/// applied to everything printed, `--raw` included. A key shorter than 8 characters isn't
/// a real one (and replacing it would mangle the output), so it's left alone. Pure.
pub fn scrub(text: &str, api_key: &str) -> String {
    let key = api_key.trim();
    if key.chars().count() < 8 {
        return text.to_string();
    }
    text.replace(key, KEY_SHOWN_AS)
}

/// What to print for one response, or the error to fail with. A 2xx JSON body is printed
/// pretty; any other 2xx body as text; an empty one as nothing. Anything else — a 3xx
/// included: redirects aren't followed — is the classified provider error. Unless `raw`,
/// secrets are redacted; the API key never shows either way. Pure.
///
/// The key is scrubbed from the body **as received**, before anything else touches it, and
/// again from what is printed. Scrubbing only the finished error message wasn't enough: the
/// error clips its body to 300 characters, and a key the clip cut in two no longer matched —
/// its head printed (`--raw`, or any key [`redact_text`] doesn't know by shape, like Vast's
/// and Hetzner's). The second pass catches a key that only reads as one once a JSON body is
/// decoded (`\u0071…` escapes) and re-encoded.
pub fn render(status: StatusCode, body: &str, raw: bool, api_key: &str) -> Result<String> {
    let body = scrub(body, api_key);
    let json = serde_json::from_str::<Value>(&body).ok();
    if !status.is_success() {
        let shown = match json {
            // Compact, as the error prints a JSON body anyway — and scrubbed here, before
            // the clip.
            Some(v) => scrub(&(if raw { v } else { redact_personal(&redact(&v)) }).to_string(), api_key),
            None if raw => body,
            None => redact_text(&body),
        };
        let mut e = status_error(status, &shown, "api get");
        if status.is_redirection() {
            e = Error::provider(format!("{e} — redirects aren't followed (the request stays on the API base)"));
        }
        return Err(match e {
            Error::Provider { kind, message } => Error::Provider { kind, message: scrub(&message, api_key) },
            other => other,
        });
    }
    let text = match json {
        Some(v) => serde_json::to_string_pretty(&if raw { v } else { redact_personal(&redact(&v)) })
            .map_err(|e| Error::provider(format!("api get: re-encoding the response: {e}")))?,
        None if raw => body,
        None => redact_text(&body),
    };
    Ok(scrub(&text, api_key))
}

/// [`render`] for `base`'s API. Shadeform's bodies hold two things for us that are never
/// worth printing, so on Shadeform they are blanked **even with `--raw`** (review SF2/SF9,
/// defence in depth behind [`refuse_raw`]): a private key (`/sshkeys` returns the managed
/// key's private half) and personal data (`/account`'s email, phone, tax id, address,
/// organisation). Everything else is as [`render`] has it. Pure.
pub fn render_for(base: &ApiBase, status: StatusCode, body: &str, raw: bool, api_key: &str) -> Result<String> {
    if base.provider != "shadeform" || !raw {
        return render(status, body, raw, api_key);
    }
    let body = match serde_json::from_str::<Value>(body) {
        Ok(v) => redact_personal(&blank_where(&v, &|k| k.to_ascii_lowercase().contains("private"))).to_string(),
        Err(_) if body.contains("PRIVATE KEY") || body.contains("-----BEGIN") => redact_text(body),
        Err(_) => body.to_string(),
    };
    render(status, &body, raw, api_key)
}

/// Personal data, by key name: blanked in the default rendering like a secret (review SF9 —
/// Shadeform's `GET /account` carries the account's email, phone, tax id, address and
/// organisation; Vast's `users/current/` an email). Broad on purpose, as [`secret_key`] is.
fn personal_key(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace('-', "_");
    ["email", "e_mail", "phone", "mobile", "tax_id", "taxid", "vat_id", "address", "organization", "organisation", "postal", "zip_code"]
        .iter()
        .any(|w| n.contains(w))
}

/// `v` with every value under a personal-data key ([`personal_key`]) blanked. Pure.
fn redact_personal(v: &Value) -> Value {
    blank_where(v, &personal_key)
}

/// `v` with every value under a key `hit` names blanked ([`blank`]), at any depth. Pure.
fn blank_where(v: &Value, hit: &dyn Fn(&str) -> bool) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), if hit(k) { blank(v) } else { blank_where(v, hit) })).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|x| blank_where(x, hit)).collect()),
        other => other.clone(),
    }
}

/// The passthrough's HTTP client: redirects are never followed (a 3xx is reported, so the
/// request — and its key — can't be bounced to another host), and the whole request is
/// bounded by [`TIMEOUT`].
fn client_builder() -> reqwest::ClientBuilder {
    Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(TIMEOUT)
}

/// `GET` `url` the way `base` takes its key ([`ApiBase::key_header`]; see [`get`]).
pub async fn get_for(base: &ApiBase, url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    get_for_with(&client_builder().build()?, base, url, api_key).await
}

async fn get_for_with(client: &Client, base: &ApiBase, url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    match base.key_header() {
        None => get_with(client, url, api_key).await,
        Some(header) => {
            let limits = limits_for(&url);
            crate::http::send_text_with(client.get(url).header(header, api_key).header("Accept", "application/json"), limits).await
        }
    }
}

/// `GET` `url` with the key as a bearer token (see [`client_builder`]). Returns the status
/// and the body text — read best-effort for an error status, so the status still
/// classifies a body that can't be read.
pub async fn get(url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    get_with(&client_builder().build()?, url, api_key).await
}

/// Both GETs go through the rate-limit-aware path ([`crate::http::exchange`]) like every
/// other request: a held quota is waited out (or refused, not sent), a 429 retried, and what
/// the API said about its quota is kept for [`limits_report`].
async fn get_with(client: &Client, url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    let limits = limits_for(&url);
    crate::http::send_text_with(client.get(url).bearer_auth(api_key), limits).await
}

/// RunPod's limits for a RunPod URL (its reads are resent after a 5xx too), the others' else.
fn limits_for(url: &Url) -> &'static crate::http::Limits {
    if crate::ratelimit::api_of(url).0 == "runpod" {
        &crate::http::Limits::RUNPOD
    } else {
        &crate::http::Limits::OTHERS
    }
}

/// `api get … --limits`: what the API `url` belongs to said about its quota on this process's
/// requests to it ([`crate::ratelimit::describe`]) — the `RateLimit-Policy` and `RateLimit`
/// headers as received, each window now, the requests sent. Counts only: no key, no body.
pub fn limits_report(url: &Url) -> Vec<String> {
    let api = crate::ratelimit::api_of(url).1;
    let views: Vec<_> = crate::ratelimit::observed().into_iter().filter(|v| v.api == api).collect();
    if views.is_empty() {
        return vec![format!("{api}: no request was sent")];
    }
    views.iter().flat_map(crate::ratelimit::describe).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const V2: &str = "https://api.runpod.io/v2";
    const VAST: &str = "https://console.vast.ai/api/v0";

    /// (path, the URL it resolves to)
    #[test]
    fn relative_paths_resolve_under_the_base() {
        let cases = [
            (V2, "pods", "https://api.runpod.io/v2/pods"),
            (V2, "/pods", "https://api.runpod.io/v2/pods"),
            (V2, "pods/mqseb0i5acrssr", "https://api.runpod.io/v2/pods/mqseb0i5acrssr"),
            (V2, "catalog/gpus?cloud=COMMUNITY&product=POD", "https://api.runpod.io/v2/catalog/gpus?cloud=COMMUNITY&product=POD"),
            (V2, "  network-volumes ", "https://api.runpod.io/v2/network-volumes"),
            (VAST, "users/current/", "https://console.vast.ai/api/v0/users/current/"),
            (V2, "pods/@evil.com", "https://api.runpod.io/v2/pods/@evil.com"),
            (V2, "pods?next=https%3A%2F%2Fevil.com", "https://api.runpod.io/v2/pods?next=https%3A%2F%2Fevil.com"),
        ];
        for (base, path, want) in cases {
            let url = resolve_url(base, path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(url.as_str(), want, "{path}");
        }
    }

    /// Wave-6 live finding: `arena api get runpod-v2 /v2/pods` went to `…/v2/v2/pods` (404).
    /// A leading copy of the base's own path is dropped; another version is refused with a
    /// pointer, the base named.
    #[test]
    fn a_leading_version_prefix_is_the_bases_own_or_refused() {
        let ok = |base: &str, path: &str| resolve_url(base, path).unwrap_or_else(|e| panic!("{path}: {e}")).to_string();
        assert_eq!(ok(V2, "/v2/pods"), "https://api.runpod.io/v2/pods");
        assert_eq!(ok(V2, "v2/pods/abc?x=1"), "https://api.runpod.io/v2/pods/abc?x=1");
        assert_eq!(ok(V2, "/v2"), "https://api.runpod.io/v2/");
        assert_eq!(ok(VAST, "/api/v0/instances/"), "https://console.vast.ai/api/v0/instances/");
        assert_eq!(ok("https://rest.runpod.io/v1", "/v1/pods"), "https://rest.runpod.io/v1/pods");
        assert_eq!(ok(V2, "v2pods"), "https://api.runpod.io/v2/v2pods", "only a whole segment");
        let e = resolve_url(V2, "/v1/pods").unwrap_err().to_string();
        assert!(e.contains("starts with `v1/`") && e.contains("runpod-v1") && e.contains(V2), "{e}");
        let e = resolve_url("https://rest.runpod.io/v1", "v2/pods").unwrap_err().to_string();
        assert!(e.contains("runpod-v2"), "{e}");
        let e = resolve_url(VAST, "v1/instances").unwrap_err().to_string();
        assert!(e.contains("starts with `v1/`") && e.contains(VAST), "{e}");
    }

    /// Anything that could send the key somewhere else — or leave the API base — is refused
    /// before a request exists.
    #[test]
    fn paths_that_could_leave_the_base_are_refused() {
        let cases = [
            ("", "no path"),
            ("https://evil.com/x", "absolute URLs"),
            ("http://api.runpod.io/v2/pods", "absolute URLs"),
            ("//evil.com/x", "absolute URLs"),
            ("evil.com://x", "absolute URLs"),
            ("javascript:alert(1)", "absolute URLs"),
            ("file:/etc/passwd", "absolute URLs"),
            ("../graphql", "path segments"),
            ("pods/../../graphql", "path segments"),
            ("pods/%2e%2e/%2E%2E/graphql", "path segments"),
            ("./pods", "path segments"),
            ("pods\\..\\x", "backslashes"),
            ("pods#x", "fragment"),
            ("pods x", "whitespace"),
        ];
        for (path, want) in cases {
            let e = resolve_url(V2, path).unwrap_err();
            assert!(matches!(e, Error::Config(_)), "{path}: {e:?}");
            let msg = e.to_string();
            assert!(msg.contains(want), "{path:?}: `{want}` in {msg}");
            assert!(msg.contains(V2), "{path:?}: names the base: {msg}");
        }
        // A trailing newline is trimmed away like any whitespace at the ends; inside, it's refused.
        assert!(resolve_url(V2, "pods\n").is_ok());
        assert!(resolve_url(V2, "po\nds").unwrap_err().to_string().contains("control characters"));
    }

    #[test]
    fn api_base_follows_runpod_api_and_names_the_key() {
        let v1 = Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v1");
        let v2 = Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v2");
        assert_eq!(api_base("runpod", &v1).unwrap().url, "https://rest.runpod.io/v1");
        assert_eq!(api_base("runpod", &v2).unwrap().url, V2);
        let default = Config::parse("RUNPOD_API_KEY=k");
        assert_eq!(api_base("runpod", &default).unwrap(), ApiBase { provider: "runpod-v2", url: V2, key: "RUNPOD_API_KEY" });
        assert_eq!(api_base("RunPod-V2", &v1).unwrap().provider, "runpod-v2");
        assert_eq!(api_base("runpod-v1", &v2).unwrap().url, "https://rest.runpod.io/v1");
        assert_eq!(api_base("vast", &v1).unwrap(), ApiBase { provider: "vast", url: VAST, key: "VAST_API_KEY" });
        assert_eq!(api_base("hetzner", &v1).unwrap().url, "https://api.hetzner.cloud/v1");
        assert_eq!(api_base("hetzner", &v1).unwrap().key, "HETZNER_API_KEY");
        let sf = api_base("shadeform", &v1).unwrap();
        assert_eq!((sf.url, sf.key, sf.key_header()), ("https://api.shadeform.ai/v1", "SHADEFORM_API_KEY", Some("X-API-KEY")));
        assert_eq!(api_base("hetzner", &v1).unwrap().key_header(), None, "bearer everywhere else");
        let e = api_base("openrouter", &v1).unwrap_err().to_string();
        assert!(e.contains("unknown provider `openrouter`") && e.contains("runpod-v2"), "{e}");
        assert!(api_base("runpod", &Config::parse("RUNPOD_API=v3")).is_err());
    }

    /// (case, text, redacted)
    #[test]
    fn redact_text_table() {
        let blob = format!("AAAAC3NzaC1lZDI1NTE5AAAAI{}", "x".repeat(30));
        let cases = [
            ("runpod key", "key rpa_ABCDEFGH1234567890 here".to_string(), "key <redacted> here".to_string()),
            ("openrouter key", "sk-or-v1-0123456789abcdef".into(), "<redacted>".into()),
            ("hf token in a shell line", "export HF_TOKEN=hf_abcdefghijkl; ls".into(), "export HF_TOKEN=<redacted>; ls".into()),
            ("github", "ghp_0123456789abcdefghij".into(), "<redacted>".into()),
            ("ssh public key keeps its type and comment", format!("ssh-ed25519 {blob} devtest"), "ssh-ed25519 <redacted> devtest".into()),
            ("pem", "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END".into(), "<redacted>".into()),
            ("a short prefix-word stays", "sk-learn and hf_x".into(), "sk-learn and hf_x".into()),
            ("not at a word boundary", "disk-12345678901 task_hf_12345678901".into(), "disk-12345678901 task_hf_12345678901".into()),
            ("a short AAAA run stays", "AAAA1234".into(), "AAAA1234".into()),
            ("plain text", "RUNNING on 64.119.209.250:23924".into(), "RUNNING on 64.119.209.250:23924".into()),
            ("non-ascii", "café rpa_ABCDEFGH1234 ü".into(), "café <redacted> ü".into()),
        ];
        for (case, text, want) in cases {
            assert_eq!(redact_text(&text), want, "{case}");
        }
    }

    /// A v2 pod (the recorded shape) and Vast/Hetzner-style secrets: env values, keys,
    /// tokens and passwords go; ids, names, endpoints, prices stay.
    #[test]
    fn redact_blanks_secret_fields_and_keeps_the_rest() {
        let body = json!({
            "pods": [{
                "id": "mqseb0i5acrssr", "name": "devtest-echo", "cost": 0.13, "locked": true,
                "env": {"PUBLIC_KEY": "ssh-ed25519 AAAA… devtest", "HF_TOKEN": "hf_abcdefghijkl", "MACHINE_NAME": "devtest-echo"},
                "ssh": {"direct": {"host": "64.119.209.250", "port": 23924, "command": "ssh root@64.119.209.250 -p 23924"}},
            }],
            "keys": ["ssh-ed25519 AAAAC3 a", "ssh-rsa AAAAB3 b"],
            "user": {"api_key": "abc123", "ssh_key": "ssh-rsa x", "jupyter_token": "t0k3n", "credit": 136.87, "email": "x@y.z"},
            "image_login": "-u me -p hunter2 docker.io",
            "extra_env": [["HF_TOKEN", "hf_abcdefghijkl"]],
            "root_password": "hunter2", "public_key": "ssh-ed25519 x", "Authorization": "Bearer x",
            "note": "token rpa_ABCDEFGH12345678 in text",
        });
        let r = redact(&body);
        let pod = &r["pods"][0];
        assert_eq!((pod["id"].as_str(), pod["name"].as_str(), pod["cost"].as_f64(), pod["locked"].as_bool()), (Some("mqseb0i5acrssr"), Some("devtest-echo"), Some(0.13), Some(true)));
        assert_eq!(pod["env"], json!({"PUBLIC_KEY": REDACTED, "HF_TOKEN": REDACTED, "MACHINE_NAME": REDACTED}), "env names kept, values gone");
        assert_eq!(pod["ssh"]["direct"]["port"], 23924);
        assert_eq!(r["keys"], json!([REDACTED, REDACTED]));
        assert_eq!(
            r["user"],
            json!({"api_key": REDACTED, "ssh_key": REDACTED, "jupyter_token": REDACTED, "credit": 136.87, "email": "x@y.z"})
        );
        assert_eq!(r["extra_env"], json!([[REDACTED, REDACTED]]));
        for k in ["root_password", "public_key", "Authorization", "image_login"] {
            assert_eq!(r[k], REDACTED, "{k}");
        }
        assert_eq!(r["note"], "token <redacted> in text");
    }

    #[test]
    fn render_prints_json_pretty_redacted_and_never_the_key() {
        let key = "rpa_THEREALKEY0123456789";
        let body = json!({"env": {"A": "1"}, "echo": key, "plain": "x"}).to_string();
        let out = render(StatusCode::OK, &body, false, key).unwrap();
        assert!(out.contains("\n  \"plain\": \"x\""), "pretty: {out}");
        assert!(!out.contains("THEREALKEY") && out.contains(REDACTED), "{out}");
        // --raw: the body as received — env and all — but the key itself still never shows.
        let raw = render(StatusCode::OK, &body, true, key).unwrap();
        assert!(raw.contains("\"A\": \"1\"") && raw.contains(KEY_SHOWN_AS) && !raw.contains("THEREALKEY"), "{raw}");
        // Text bodies: redacted unless raw; the key never (scrubbed first, so it reads as
        // the key either way).
        assert_eq!(render(StatusCode::OK, &format!("ok {key} hf_abcdefghijkl"), false, key).unwrap(), format!("ok {KEY_SHOWN_AS} <redacted>"));
        assert_eq!(render(StatusCode::OK, &format!("ok {key}"), true, key).unwrap(), format!("ok {KEY_SHOWN_AS}"));
        assert_eq!(render(StatusCode::NO_CONTENT, "", false, key).unwrap(), "");
        // Error statuses fail, classified, the key scrubbed from the message.
        let e = render(StatusCode::UNAUTHORIZED, &format!("{{\"error\":\"bad key {key}\"}}"), true, key).unwrap_err();
        assert_eq!(e.kind(), Some(crate::ProviderErrorKind::Auth));
        assert!(e.to_string().contains("api get HTTP 401") && !e.to_string().contains("THEREALKEY"), "{e}");
        let e = render(StatusCode::MOVED_PERMANENTLY, "", false, key).unwrap_err().to_string();
        assert!(e.contains("HTTP 301") && e.contains("redirects aren't followed"), "{e}");
        // A short key is no key: nothing replaced.
        assert_eq!(scrub("k in a word", "k"), "k in a word");
    }

    /// An error body is clipped to 300 characters in the message. A key echoed across that
    /// cut must not print its head — `--raw`, or redacted with a key whose shape
    /// `redact_text` doesn't know (Vast's and Hetzner's: 64 alphanumerics, no prefix) — nor
    /// may a key escaped in a JSON body (`\u0071…`), which the error decodes.
    #[test]
    fn an_error_body_never_prints_part_of_the_key_at_the_clip() {
        let runpod = "rpa_THEREALKEY0123456789ABCDEFGHIJ";
        let hetzner = "q8Xv2LmN9pR4tZ7wK1yB6cD3fG5hJ0sQ2uE8iO4aV9nM7xL1kP3rT6yW0zC5bH8j";
        assert_eq!(hetzner.len(), 64);
        for key in [runpod, hetzner] {
            let head = &key[..6];
            // (case, body, raw)
            let cases = [
                ("text across the cut, raw", format!("{}{key} tail", "x".repeat(290)), true),
                ("text across the cut", format!("{}{key} tail", "x".repeat(290)), false),
                ("json across the cut, raw", json!({"error": format!("{}{key}", "x".repeat(280))}).to_string(), true),
                ("json across the cut", json!({"error": format!("{}{key}", "x".repeat(280))}).to_string(), false),
                ("json-escaped key", format!(r#"{{"error":"bad key \u{:04x}{}"}}"#, key.as_bytes()[0], &key[1..]), true),
            ];
            for (case, body, raw) in cases {
                for status in [StatusCode::UNAUTHORIZED, StatusCode::FOUND] {
                    let e = render(status, &body, raw, key).unwrap_err().to_string();
                    assert!(!e.contains(head), "{case} ({status}, key {head}…): {e}");
                    assert!(e.contains("api get HTTP"), "{case}: {e}");
                }
            }
        }
        // A 2xx body the same: whole, escaped or not, never the key.
        let escaped = format!(r#"{{"echo":"\u{:04x}{}"}}"#, hetzner.as_bytes()[0], &hetzner[1..]);
        for raw in [true, false] {
            let out = render(StatusCode::OK, &escaped, raw, hetzner).unwrap();
            assert!(!out.contains(&hetzner[..6]) && out.contains(KEY_SHOWN_AS), "{out}");
        }
    }

    /// End to end over a loopback socket: GET only, the key as a bearer header (never in the
    /// URL), and a redirect — with a `Location` pointing at a second server, as a real one
    /// would — reported, not chased: the second server never sees a request (or the key).
    /// (Without a `Location`, no client follows a 3xx, so a test without one proves nothing.)
    #[tokio::test]
    async fn get_sends_one_get_and_does_not_follow_a_redirect() {
        use crate::http::test_server::{canned, serve};
        let elsewhere = serve(vec![canned(200, "application/json", r#"{"stolen":true}"#)]);
        let srv = serve(vec![
            canned(200, "application/json", r#"{"pods":[]}"#),
            canned(302, "text/plain", "").header("Location", format!("{}/x", elsewhere.base)),
        ]);
        let url = Url::parse(&format!("{}/v2/pods", srv.base)).unwrap();
        // The real builder, minus a system proxy (an HTTP_PROXY in the test env must not
        // route a loopback request elsewhere).
        let client = client_builder().no_proxy().build().unwrap();
        let (status, body) = get_with(&client, url.clone(), "k3y-material").await.unwrap();
        assert_eq!((status, body.as_str()), (StatusCode::OK, r#"{"pods":[]}"#));
        let (status, _) = get_with(&client, url, "k3y-material").await.unwrap();
        assert_eq!(status, StatusCode::FOUND, "reported, not followed");
        assert_eq!(*srv.requests.lock().unwrap(), ["GET /v2/pods HTTP/1.1", "GET /v2/pods HTTP/1.1"]);
        for head in srv.headers.lock().unwrap().iter() {
            let auth: Vec<&String> = head.iter().filter(|h| h.to_ascii_lowercase().starts_with("authorization:")).collect();
            assert_eq!(auth.len(), 1, "{head:?}");
            assert_eq!(auth[0].split_once(':').unwrap().1.trim(), "Bearer k3y-material", "{head:?}");
        }
        // A following client would have made that request inside `send` (and answered 200).
        assert!(elsewhere.requests.lock().unwrap().is_empty(), "the redirect was followed: {:?}", elsewhere.requests.lock().unwrap());
    }

    /// Shadeform: the key in `X-API-KEY` (no `Authorization`), and `--raw` on `sshkeys…`
    /// refused — the body holds the managed key's private half; redacted without it.
    #[tokio::test]
    async fn shadeform_key_header_and_sshkeys_raw_refusal() {
        use crate::http::test_server::{canned, serve};
        let cfg = Config::parse("");
        let sf = api_base("shadeform", &cfg).unwrap();
        let srv = serve(vec![canned(200, "application/json", r#"{"instances":[]}"#)]);
        let client = client_builder().no_proxy().build().unwrap();
        let url = Url::parse(&format!("{}/instances", srv.base)).unwrap();
        let (status, _) = get_for_with(&client, &sf, url, "sf-key-material").await.unwrap();
        assert_eq!(status, StatusCode::OK);
        let head = srv.headers.lock().unwrap()[0].clone();
        assert!(head.iter().any(|h| h == "x-api-key: sf-key-material"), "{head:?}");
        assert!(!head.iter().any(|h| h.to_ascii_lowercase().starts_with("authorization")), "{head:?}");
        for path in ["sshkeys", "/sshkeys/abc/info", "SSHKEYS?x=1"] {
            let e = refuse_raw(&sf, path, true).unwrap_err().to_string();
            assert!(e.contains("private half") && e.contains("drop --raw"), "{path}: {e}");
            refuse_raw(&sf, path, false).unwrap();
        }
        refuse_raw(&sf, "instances", true).unwrap();
        refuse_raw(&api_base("vast", &cfg).unwrap(), "sshkeys", true).unwrap();
        let body = r#"{"ssh_keys":[{"id":"1","name":"Shadeform Managed Key","public_key":"ssh-rsa AAAA x","private_key":"PRIVATE-KEY-MATERIAL-MUST-NEVER-BE-PRINTED"}]}"#;
        let out = render(StatusCode::OK, body, false, "sf-key-material").unwrap();
        assert!(!out.contains("PRIVATE-KEY-MATERIAL") && out.contains("Shadeform Managed Key"), "{out}");
    }

    /// REVIEW sf SF2: `--raw` on Shadeform's `sshkeys` is refused by the path's *first segment*,
    /// but [`resolve_url`] strips a leading copy of the base's own version — the docs' spelling,
    /// `https://api.shadeform.ai/v1/sshkeys`, pasted as `v1/sshkeys` or `/v1/sshkeys`. Such a path
    /// passes the refusal (`v1` isn't `sshkeys`) and then GETs `/v1/sshkeys`: `api get shadeform
    /// v1/sshkeys --raw` prints the managed key's private half, unredacted, to the terminal and
    /// its logs. The refusal must judge the path actually requested (the resolved URL's), which
    /// also covers a percent-encoded segment (`%73shkeys`): Shadeform's router is Go's (its 404
    /// is `404 page not found`), which matches on the decoded path.
    #[test]
    fn review_sf_sf2_raw_refusal_covers_every_path_that_reaches_sshkeys() {
        let sf = api_base("shadeform", &Config::parse("")).unwrap();
        for path in ["v1/sshkeys", "/v1/sshkeys", "v1/sshkeys/abc/info", "/v1/sshkeys?x=1"] {
            let url = resolve_url(sf.url, path).unwrap();
            assert!(url.path().starts_with("/v1/sshkeys"), "{path} → {url}");
            assert!(
                refuse_raw(&sf, path, true).is_err(),
                "`api get shadeform {path} --raw` passes the refusal and GETs {url} — the private key, raw"
            );
        }
        let url = resolve_url(sf.url, "%73shkeys").unwrap();
        assert_eq!(url.path(), "/v1/%73shkeys");
        assert!(refuse_raw(&sf, "%73shkeys", true).is_err(), "`%73shkeys` is `sshkeys` to Go's router — refused only by spelling");
    }

    /// REVIEW sf SF8 (cosmetic): several new messages carry a run of spaces mid-sentence — a
    /// source line continuation lost its `\`, so the next line's indentation became part of the
    /// string: this refusal prints "private half there —              drop --raw". (Also in
    /// the CLI: `warn_no_volume`'s Shadeform line and two `config check` rows.)
    #[test]
    fn review_sf_sf8_refusal_text_has_no_stray_indentation() {
        let sf = api_base("shadeform", &Config::parse("")).unwrap();
        let e = refuse_raw(&sf, "sshkeys", true).unwrap_err().to_string();
        assert!(!e.contains("    "), "a run of spaces inside the message: {e:?}");
    }

    /// REVIEW sf SF9 (hygiene, low): `GET /account` is the one Shadeform body arena reads that
    /// carries the human's personal data — email, phone, tax id, postal address (spec §6: "never
    /// logs the body"; the spike piped it through a redactor). `balance` types it down to the
    /// money, but `api get shadeform account` (the obvious way to eyeball the undocumented
    /// endpoint during the live test) prints every field unredacted without `--raw`, straight
    /// into terminal scrollback and the agent transcripts and reports this sandbox keeps. The
    /// default (redacted) rendering should blank them as it blanks keys; `--raw` stays raw.
    #[test]
    fn review_sf_sf9_account_pii_is_redacted_without_raw() {
        let body = r#"{"id":"x","tier":"free","balance":"12.50","hourly_burn":"0.57","email":"someone@example.invalid","phone_number":"+1 555 0100","tax_id":"T-12345","address":"1 Example Way","organization":"Example Org"}"#;
        let out = render(StatusCode::OK, body, false, "sf-key-material").unwrap();
        assert!(out.contains("12.50"), "{out}");
        for pii in ["someone@example.invalid", "+1 555 0100", "T-12345", "1 Example Way"] {
            assert!(!out.contains(pii), "`api get shadeform account` printed {pii:?}:\n{out}");
        }
    }

    /// SF2, the fix's own table: every spelling that reaches `/v1/sshkeys…` is refused with
    /// `--raw` — the docs' `v1/` prefix, any case, percent-encoding (once or twice), `//` and
    /// `./` — and nothing else is: other routes, or `sshkeys` without `--raw`, or another API.
    #[test]
    fn sf2_raw_refusal_canonicalises_the_path() {
        let cfg = Config::parse("");
        let sf = api_base("shadeform", &cfg).unwrap();
        for path in [
            "sshkeys", "SSHKEYS", "/sshkeys/", "v1/sshkeys", "V1/SshKeys", "/v1/sshkeys/abc/info", "v1//sshkeys", "./sshkeys",
            "v1/./sshkeys", "%73shkeys", "%2573shkeys", "v1/%53%53HKEYS", "sshkeys?x=1", "sshkeys/abc/info#f", "v1/v1/sshkeys",
        ] {
            assert!(refuse_raw(&sf, path, true).is_err(), "{path} should be refused with --raw");
            assert!(refuse_raw(&sf, path, false).is_ok(), "{path} without --raw is redacted, not refused");
        }
        for path in ["instances", "v1/instances", "instances/types", "account", "sshkey-ish/../x", "my-sshkeys"] {
            assert!(refuse_raw(&sf, path, true).is_ok(), "{path}");
        }
        assert!(refuse_raw(&api_base("vast", &cfg).unwrap(), "v1/sshkeys", true).is_ok(), "only Shadeform's");
        assert_eq!(percent_decode("%73sh%4Beys%zz%"), "sshKeys%zz%");
        assert_eq!(canonical_segments("/V1//./%73shkeys/?q=1"), ["v1", "sshkeys"]);
    }

    /// SF2/SF9 defence in depth: on Shadeform a private key and personal fields are blanked
    /// even with `--raw` — whatever path got there; elsewhere `--raw` is as received. Without
    /// `--raw`, personal fields are blanked on every API (Vast's `users/current/` has an email).
    #[test]
    fn render_for_blanks_shadeforms_private_key_and_personal_data_even_raw() {
        let cfg = Config::parse("");
        let sf = api_base("shadeform", &cfg).unwrap();
        let vast = api_base("vast", &cfg).unwrap();
        let keys = r#"{"ssh_keys":[{"id":"1","name":"Shadeform Managed Key","public_key":"ssh-rsa AAAA x","private_key":"PRIVATE-KEY-MATERIAL-MUST-NEVER-BE-PRINTED","is_default":true}]}"#;
        let out = render_for(&sf, StatusCode::OK, keys, true, "sf-key-material").unwrap();
        assert!(!out.contains("PRIVATE-KEY-MATERIAL") && out.contains("ssh-rsa AAAA x") && out.contains(REDACTED), "{out}");
        let account = r#"{"balance":"12.50","email":"someone@example.invalid","phone_number":"+1 555 0100","tax_id":"T-1","address":"1 Way","organization":"Org"}"#;
        let out = render_for(&sf, StatusCode::OK, account, true, "sf-key-material").unwrap();
        for pii in ["someone@example.invalid", "+1 555 0100", "T-1", "1 Way", "\"Org\""] {
            assert!(!out.contains(pii), "{pii} in {out}");
        }
        assert!(out.contains("12.50"));
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----";
        assert!(!render_for(&sf, StatusCode::OK, pem, true, "sf-key-material").unwrap().contains("abc"));
        // Another API with --raw: as received (its documented contract).
        let out = render_for(&vast, StatusCode::OK, r#"{"email":"x@y.z","credit":1}"#, true, "vast-key-material").unwrap();
        assert!(out.contains("x@y.z"), "{out}");
        // Without --raw, personal data is blanked everywhere.
        let out = render_for(&vast, StatusCode::OK, r#"{"email":"x@y.z","credit":1}"#, false, "vast-key-material").unwrap();
        assert!(!out.contains("x@y.z") && out.contains("credit"), "{out}");
    }
}
