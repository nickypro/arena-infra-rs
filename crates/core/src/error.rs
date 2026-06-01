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
pub fn looks_like_capacity(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "no instances",
        "no longer any instances",
        "no rentable offer",
        "capacity",
        "unavailable",
        "out of stock",
        "no offers",
        "insufficient",
        "no resources",
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
    }

    #[test]
    fn constructors_set_kind() {
        assert_eq!(Error::capacity("no rentable offer").kind(), Some(ProviderErrorKind::Capacity));
        assert_eq!(Error::provider("ssh failed").kind(), Some(ProviderErrorKind::Other));
        assert_eq!(Error::Config("x".into()).kind(), None);
    }
}
