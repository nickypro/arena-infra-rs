use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// What kind of provider failure occurred, so callers can react appropriately:
/// stop gracefully on `Capacity`, abort on `Auth`, back off on `RateLimited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The provider has no machine to give right now (pool exhausted / no offer).
    /// Not really an error for a batch create — it just means "that's all there is".
    Capacity,
    /// We're being throttled (HTTP 429). Caller may back off and retry.
    RateLimited,
    /// A transient server-side failure (HTTP 5xx). Worth retrying with backoff.
    Transient,
    /// Bad/again credentials (HTTP 401/403). Retrying is pointless — abort.
    Auth,
    /// This one request is refused for this account, though the key works and other
    /// requests may not be: RunPod v2's `POST /v2/pods` documents its 403 as "your account
    /// cannot access the requested pool — skip this candidate, keep going". Not `Auth`
    /// (aborting would throw away the fallback options), not `Capacity` (waiting won't
    /// change it). Never produced by [`ProviderErrorKind::classify`] — only a backend that
    /// knows an endpoint means this re-tags its error.
    Denied,
    /// The pod refuses lifecycle changes because it is locked: RunPod v2 answers stop,
    /// restart and terminate on a pod with `locked: true` with a 400 "Pod is locked"
    /// (live-verified 2026-10-08), leaving the container untouched. Kept apart from `Other`
    /// so a caller can say "unlock it first" instead of "failed" — and never retried: the
    /// lock is deliberate, so waiting won't lift it. Like `Denied`, never produced by
    /// [`ProviderErrorKind::classify`]; only the backend that knows the wording tags it.
    Locked,
    /// The API refused this request as invalid — schema validation, a GPU type its create
    /// doesn't take — so the same request again won't work, but another option (another GPU
    /// type, tier) may: placement skips the option for the rest of the run instead of stopping
    /// (finding #22: one v1 `400 … does not meet the schema requirements` on the first option
    /// left 4 good options untried). Never retried. [`ProviderErrorKind::classify`] tags RunPod
    /// v1's schema wording; RunPod v2's `422` is an [`Error::Config`], read the same way.
    Rejected,
    /// Anything else.
    Other,
}

impl ProviderErrorKind {
    /// Classify from an HTTP status plus the response message. Status pins down auth
    /// and throttling unambiguously; capacity has no standard code, so we sniff the
    /// body for the phrases providers actually use.
    pub fn classify(status: reqwest::StatusCode, message: &str) -> Self {
        // Status wins for auth/rate-limit. But capacity is checked *before* the generic
        // 5xx→Transient rule, because providers (e.g. RunPod) report "no instances
        // available" as an HTTP 500 — that's exhaustion to wait out, not a blip to retry.
        match status.as_u16() {
            401 | 403 => return Self::Auth,
            429 => return Self::RateLimited,
            _ => {}
        }
        // Vast answers a bad API key with a 404 `{"error": "auth_error", "msg": "Invalid user
        // key"}` (finding #33): an auth failure whatever the status, so callers abort and
        // point at the key instead of saying "re-run".
        if status.is_client_error() && looks_like_auth_error(message) {
            return Self::Auth;
        }
        if looks_like_capacity(message) {
            return Self::Capacity;
        }
        if matches!(status.as_u16(), 400 | 422) && message.contains("does not meet the schema requirements") {
            return Self::Rejected;
        }
        match status.as_u16() {
            500..=599 => Self::Transient,
            _ => Self::Other,
        }
    }
}

/// Whether an error body is Vast's "your API key is wrong" (`"error": "auth_error"`, which it
/// sends with a 404). Matched on the error code, not on words like "invalid key" — Vast's
/// SSH-key attach says that about an SSH key, which is no auth failure.
pub fn looks_like_auth_error(message: &str) -> bool {
    let compact: String = message.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains(r#""error":"auth_error""#)
}

/// Heuristic: does this message read like "no machines available"? Providers don't
/// share a status code for this, so we match the phrasings RunPod/Vast/Hetzner use.
///
/// Needles are kept SPECIFIC to pool-exhaustion on purpose. Broad words like
/// "insufficient" or bare "unavailable" were removed: "insufficient funds/credit" is a
/// billing failure, not capacity, and misclassifying it as capacity would make
/// `--keep-trying` wait for a GPU that will never come because the account is out of
/// money. When unsure we return false (→ `Other`), which fails fast rather than looping.
///
/// RunPod REST v2 answers capacity exhaustion with an HTTP 400 that carries "no
/// machine-readable code of its own — only a human-readable `detail`" (its OpenAPI doc),
/// describing it as the GPU/data-center combination that "could not be placed"; those
/// phrasings are matched too (the exact live wording is unverified — v1's "no instances"
/// is still covered if v2 passes it through).
pub fn looks_like_capacity(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "no instances",
        "no longer any instances",
        "no rentable offer",
        "no capacity",
        "insufficient capacity",
        "no availability",
        "not available in",
        "currently unavailable",
        "out of stock",
        "no offers",
        "no gpus available",
        "no machines available",
        "could not be placed",
        "no resources available",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {message}")]
    Provider {
        kind: ProviderErrorKind,
        message: String,
    },

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// An operation ran out of its time budget (e.g. an SSH step on a wedged pod). Kept
    /// distinct from `Provider` so callers can tell "took too long" from "failed": a
    /// timeout must never be mistaken for a retryable connection error (the message says
    /// "timed out", which a string-sniffing classifier would happily match).
    #[error("{what} timed out after {}", human_duration(.after))]
    Timeout { what: String, after: Duration },
}

/// How much of an error body a message keeps.
const MAX_BODY: usize = 300;

/// What an error message shows of a response body: the first [`MAX_BODY`] characters — except
/// for a validation error that lists its `problems` (RunPod REST v1's schema middleware:
/// `[{"error": "At #/paths/pods/post … However, it does not meet the schema requirements …
/// Suggestion: …", "problems": ["At /pods/properties/…: …"]}]`). Cut at 300 that kept only
/// the boilerplate and lost the part naming the bad field (finding #22), so there the
/// error's first sentence and the problems themselves are kept (each clipped, at most four).
/// Pure.
pub fn body_excerpt(raw: &str) -> String {
    let clip = |s: &str, n: usize| -> String {
        if s.chars().count() > n {
            format!("{}…", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let problems = serde_json::from_str::<serde_json::Value>(raw).ok().and_then(|v| {
        let obj = if v.is_array() { v.get(0).cloned() } else { Some(v) }?;
        let list: Vec<String> = obj.get("problems")?.as_array()?.iter().map(|p| match p.as_str() {
            Some(s) => s.to_string(),
            None => p.to_string(),
        }).collect();
        let head = obj.get("error").and_then(|e| e.as_str()).map(|e| {
            let rest = e.split_once("However, ").map_or(e, |(_, r)| r);
            clip(rest.split(". ").next().unwrap_or(rest).trim(), 120)
        });
        (!list.is_empty()).then_some((head, list))
    });
    match problems {
        Some((head, list)) => {
            let shown: Vec<String> = list.iter().take(4).map(|p| clip(p, 240)).collect();
            let more = if list.len() > 4 { format!(" (+{} more)", list.len() - 4) } else { String::new() };
            let head = head.map(|h| format!("{h} — ")).unwrap_or_default();
            format!("{head}problems: {}{more}", shown.join("; "))
        }
        None => clip(raw, MAX_BODY),
    }
}

/// Render a duration for humans: whole seconds as `300s`, sub-second as `50ms`, else
/// one decimal (`1.5s`). Used in timeout messages, where the budget is usually whole
/// seconds but tests use short ones.
pub fn human_duration(d: &Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else if d.as_secs() == 0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

impl Error {
    /// Build a classified provider error from an HTTP status + body and a short
    /// context label (e.g. "create pod"). The kind is inferred from status/body.
    ///
    /// The body is truncated (on a char boundary) before it goes into the message:
    /// these errors get logged/printed, and an unbounded response body is both noisy
    /// and a needless place for sensitive echoed request context to land.
    pub fn provider_http(status: reqwest::StatusCode, body: &dyn std::fmt::Display, ctx: &str) -> Self {
        let raw = body.to_string();
        // Classified on the body as received (the excerpt may drop the words that tell).
        let kind = ProviderErrorKind::classify(status, &format!("{ctx} HTTP {status}: {raw}"));
        let message = format!("{ctx} HTTP {status}: {}", body_excerpt(&raw));
        Error::Provider { kind, message }
    }

    /// A non-HTTP provider error of unspecified kind (e.g. SSH spawn failure).
    pub fn provider(message: impl Into<String>) -> Self {
        Error::Provider { kind: ProviderErrorKind::Other, message: message.into() }
    }

    /// A provider error explicitly tagged as capacity (e.g. "no rentable offer").
    pub fn capacity(message: impl Into<String>) -> Self {
        Error::Provider { kind: ProviderErrorKind::Capacity, message: message.into() }
    }

    /// The provider-error kind, if this is a provider error.
    pub fn kind(&self) -> Option<ProviderErrorKind> {
        match self {
            Error::Provider { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Whether the provider refused because the pod is locked ([`ProviderErrorKind::Locked`]).
    pub fn is_locked(&self) -> bool {
        self.kind() == Some(ProviderErrorKind::Locked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn classifies_status_codes() {
        assert_eq!(ProviderErrorKind::classify(StatusCode::UNAUTHORIZED, ""), ProviderErrorKind::Auth);
        assert_eq!(ProviderErrorKind::classify(StatusCode::FORBIDDEN, ""), ProviderErrorKind::Auth);
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::TOO_MANY_REQUESTS, ""),
            ProviderErrorKind::RateLimited
        );
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::BAD_GATEWAY, ""),
            ProviderErrorKind::Transient
        );
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::SERVICE_UNAVAILABLE, ""),
            ProviderErrorKind::Transient
        );
    }

    #[test]
    fn sniffs_capacity_from_body() {
        let s = StatusCode::BAD_REQUEST;
        assert_eq!(
            ProviderErrorKind::classify(s, "create pod HTTP 400: no instances available"),
            ProviderErrorKind::Capacity
        );
        assert_eq!(
            ProviderErrorKind::classify(s, "create pod HTTP 400: bad image name"),
            ProviderErrorKind::Other
        );
        // Billing/credit failures must NOT be read as capacity — otherwise
        // --keep-trying would wait forever for a GPU the account can't pay for.
        assert_eq!(
            ProviderErrorKind::classify(s, "create pod HTTP 402: insufficient funds"),
            ProviderErrorKind::Other
        );
        assert!(!looks_like_capacity("insufficient credit balance"));
        assert!(looks_like_capacity("no instances available for this gpu type"));
    }

    /// RunPod v2 errors are problem+json; capacity is a 400 known only by its `detail`.
    /// Its 402 "Insufficient balance" must stay `Other` (stop, don't wait for a GPU).
    #[test]
    fn sniffs_runpod_v2_problem_details() {
        let s = StatusCode::BAD_REQUEST;
        let msg = |detail: &str| {
            format!(r#"create pod HTTP 400 Bad Request: {{"detail":"{detail}","status":400,"title":"Bad Request"}}"#)
        };
        for detail in [
            "this GPU and data center combination could not be placed",
            "Insufficient capacity for the requested GPU type",
            "There are no instances currently available",
            "no machines available with the requested CUDA version",
        ] {
            assert_eq!(ProviderErrorKind::classify(s, &msg(detail)), ProviderErrorKind::Capacity, "{detail}");
        }
        for detail in ["allowedCudaVersions and minCudaVersion are mutually exclusive", "request could not be processed"] {
            assert_eq!(ProviderErrorKind::classify(s, &msg(detail)), ProviderErrorKind::Other, "{detail}");
        }
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::PAYMENT_REQUIRED, "create pod HTTP 402: Insufficient balance"),
            ProviderErrorKind::Other
        );
    }

    #[test]
    fn capacity_500_beats_transient() {
        // RunPod reports capacity as a 500 — it must classify as Capacity (wait it out),
        // not Transient (retry a few times then give up).
        let msg = r#"create pod HTTP 500 Internal Server Error: {"error":"create pod: There are no instances currently available","status":500}"#;
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::INTERNAL_SERVER_ERROR, msg),
            ProviderErrorKind::Capacity
        );
        // A 500 without a capacity message is still Transient.
        assert_eq!(
            ProviderErrorKind::classify(StatusCode::INTERNAL_SERVER_ERROR, "boom"),
            ProviderErrorKind::Transient
        );
    }

    /// Finding #22: v1's schema 400 was cut at 300 characters, inside the boilerplate, before
    /// the part naming the bad property — and it stopped placement. The excerpt keeps the
    /// problems; the kind is `Rejected` (skip that option), from the full body.
    #[test]
    fn a_schema_400_keeps_its_problems_and_reads_as_rejected() {
        // The live body's shape (mig/v1-create-noncreatable.txt, 2026-10-08); the problems
        // text past the recorded cut-off is illustrative.
        let body = serde_json::json!([{
            "error": "At #/paths/pods/post for POST https://rest.runpod.io/v1/pods, The request body is defined as an object. \
                      However, it does not meet the schema requirements of the specification. Suggestion: Ensure that the \
                      object being submitted, matches the schema correctly",
            "problems": ["At /pods/properties/gpuTypeIds/items: value must be one of 'NVIDIA A40', 'NVIDIA RTX A4000'"]
        }]);
        let e = Error::provider_http(StatusCode::BAD_REQUEST, &body, "create pod");
        assert_eq!(e.kind(), Some(ProviderErrorKind::Rejected));
        let msg = e.to_string();
        assert!(msg.contains("problems: At /pods/properties/gpuTypeIds/items: value must be one of"), "{msg}");
        assert!(msg.contains("it does not meet the schema requirements of the specification —"), "{msg}");
        assert!(!msg.contains("Suggestion"), "the boilerplate goes: {msg}");
        // Many long problems stay bounded.
        let many: Vec<String> = (0..9).map(|i| format!("At /pods/properties/p{i}: {}", "x".repeat(400))).collect();
        let e = Error::provider_http(StatusCode::BAD_REQUEST, &serde_json::json!({"error": "bad", "problems": many}), "create pod");
        let msg = e.to_string();
        assert!(msg.contains("(+5 more)") && msg.chars().count() < 1300, "{}", msg.chars().count());
        // Anything else keeps the plain 300-character cut.
        let long = "y".repeat(500);
        let msg = Error::provider_http(StatusCode::BAD_REQUEST, &long, "create pod").to_string();
        assert!(msg.ends_with('…') && msg.matches('y').count() == 300, "{msg}");
        // A 400 that isn't schema validation stays Other (placement still stops on it).
        assert_eq!(ProviderErrorKind::classify(StatusCode::BAD_REQUEST, "create pod HTTP 400: bad image"), ProviderErrorKind::Other);
    }

    /// Finding #33: Vast's bogus-key answer is a 404 `auth_error` — Auth, so callers point at
    /// VAST_API_KEY instead of "re-run". An SSH key called "invalid" is not an auth failure.
    #[test]
    fn vast_auth_error_is_auth_whatever_the_status() {
        let body = serde_json::json!({"success": false, "error": "auth_error", "msg": "Invalid user key"});
        let e = Error::provider_http(StatusCode::NOT_FOUND, &body, "list instances");
        assert_eq!(e.kind(), Some(ProviderErrorKind::Auth), "{e}");
        assert!(!crate::retry::is_retryable(&e));
        let other = serde_json::json!({"success": false, "msg": "invalid key"});
        assert_eq!(Error::provider_http(StatusCode::BAD_REQUEST, &other, "attach").kind(), Some(ProviderErrorKind::Other));
        assert_eq!(
            Error::provider_http(StatusCode::NOT_FOUND, &serde_json::json!({"error": "no_such_instance"}), "x").kind(),
            Some(ProviderErrorKind::Other)
        );
    }

    #[test]
    fn timeout_is_its_own_variant_with_a_readable_message() {
        let e = Error::Timeout { what: "ssh 1.2.3.4:22".into(), after: Duration::from_secs(300) };
        assert_eq!(e.to_string(), "ssh 1.2.3.4:22 timed out after 300s");
        assert_eq!(e.kind(), None); // not a provider error — never classified/retried as one
        assert_eq!(human_duration(&Duration::from_millis(50)), "50ms");
        assert_eq!(human_duration(&Duration::from_millis(1500)), "1.5s");
    }

    #[test]
    fn constructors_set_kind() {
        assert_eq!(Error::capacity("no rentable offer").kind(), Some(ProviderErrorKind::Capacity));
        assert_eq!(Error::provider("ssh failed").kind(), Some(ProviderErrorKind::Other));
        assert_eq!(Error::Config("x".into()).kind(), None);
        let locked = Error::Provider { kind: ProviderErrorKind::Locked, message: "stop pod HTTP 400: Pod is locked".into() };
        assert!(locked.is_locked() && !crate::retry::is_retryable(&locked));
        // Only the tag counts, never the wording: classify() doesn't produce Locked.
        assert!(!Error::provider("Pod is locked").is_locked());
        assert_ne!(
            ProviderErrorKind::classify(reqwest::StatusCode::BAD_REQUEST, r#"{"detail":"Pod is locked"}"#),
            ProviderErrorKind::Locked
        );
    }
}
