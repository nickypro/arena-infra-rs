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
        if looks_like_capacity(message) {
            return Self::Capacity;
        }
        match status.as_u16() {
            500..=599 => Self::Transient,
            _ => Self::Other,
        }
    }
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
        const MAX_BODY: usize = 300;
        let raw = body.to_string();
        let body = if raw.chars().count() > MAX_BODY {
            let mut s: String = raw.chars().take(MAX_BODY).collect();
            s.push('…');
            s
        } else {
            raw
        };
        let message = format!("{ctx} HTTP {status}: {body}");
        let kind = ProviderErrorKind::classify(status, &message);
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
    }
}
