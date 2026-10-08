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
pub const PROVIDERS: &str = "runpod (follows RUNPOD_API), runpod-v1, runpod-v2, vast, hetzner";

/// The API base for `provider`: `runpod` is the generation `RUNPOD_API` selects (as every
/// other command), `runpod-v1`/`runpod-v2` name one explicitly. Pure.
pub fn api_base(provider: &str, cfg: &Config) -> Result<ApiBase> {
    use crate::provider::{hetzner, runpod, runpod_v2, vast};
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
        other => return Err(Error::Config(format!("api get: unknown provider `{other}` (known: {PROVIDERS})"))),
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
            Some(v) => scrub(&(if raw { v } else { redact(&v) }).to_string(), api_key),
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
        Some(v) => serde_json::to_string_pretty(&if raw { v } else { redact(&v) })
            .map_err(|e| Error::provider(format!("api get: re-encoding the response: {e}")))?,
        None if raw => body,
        None => redact_text(&body),
    };
    Ok(scrub(&text, api_key))
}

/// The passthrough's HTTP client: redirects are never followed (a 3xx is reported, so the
/// request — and its key — can't be bounced to another host), and the whole request is
/// bounded by [`TIMEOUT`].
fn client_builder() -> reqwest::ClientBuilder {
    Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(TIMEOUT)
}

/// `GET` `url` with the key as a bearer token (see [`client_builder`]). Returns the status
/// and the body text — read best-effort for an error status, so the status still
/// classifies a body that can't be read.
pub async fn get(url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    get_with(&client_builder().build()?, url, api_key).await
}

async fn get_with(client: &Client, url: Url, api_key: &str) -> Result<(StatusCode, String)> {
    let resp = client.get(url).bearer_auth(api_key).send().await?;
    let status = resp.status();
    let body = if status.is_success() { resp.text().await? } else { resp.text().await.unwrap_or_default() };
    Ok((status, body))
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
        let v1 = Config::parse("RUNPOD_API_KEY=k");
        let v2 = Config::parse("RUNPOD_API_KEY=k\nRUNPOD_API=v2");
        assert_eq!(api_base("runpod", &v1).unwrap().url, "https://rest.runpod.io/v1");
        assert_eq!(api_base("runpod", &v2).unwrap().url, V2);
        assert_eq!(api_base("RunPod-V2", &v1).unwrap().provider, "runpod-v2");
        assert_eq!(api_base("runpod-v1", &v2).unwrap().url, "https://rest.runpod.io/v1");
        assert_eq!(api_base("vast", &v1).unwrap(), ApiBase { provider: "vast", url: VAST, key: "VAST_API_KEY" });
        assert_eq!(api_base("hetzner", &v1).unwrap().url, "https://api.hetzner.cloud/v1");
        assert_eq!(api_base("hetzner", &v1).unwrap().key, "HETZNER_API_KEY");
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
}
