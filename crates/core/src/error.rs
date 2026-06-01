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
        match status.as_u16() {
            401 | 403 => return Self::Auth,
            429 => return Self::RateLimited,
            500..=599 => return Self::Transient,
            _ => {}
        }
        if looks_like_capacity(message) {
            Self::Capacity
        } else {
            Self::Other
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
pub fn looks_like_capacity(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "no instances",
        "no longer any instances",
        "no rentable offer",
        "no capacity",
        "no availability",
        "not available in",
        "currently unavailable",
        "out of stock",
        "no offers",
        "no gpus available",
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
}

impl Error {
    /// Build a classified provider error from an HTTP status + body and a short
    /// context label (e.g. "create pod"). The kind is inferred from status/body.
    pub fn provider_http(status: reqwest::StatusCode, body: &dyn std::fmt::Display, ctx: &str) -> Self {
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

    #[test]
    fn constructors_set_kind() {
        assert_eq!(Error::capacity("no rentable offer").kind(), Some(ProviderErrorKind::Capacity));
        assert_eq!(Error::provider("ssh failed").kind(), Some(ProviderErrorKind::Other));
        assert_eq!(Error::Config("x".into()).kind(), None);
    }
}
