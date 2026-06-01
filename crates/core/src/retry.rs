//! Retry-with-backoff for transient provider failures.
//!
//! Wraps an async provider operation and retries it on failures that are plausibly
//! self-correcting — throttling (429), server-side 5xx, and connect/timeout transport
//! errors — with exponential backoff. Failures that retrying can't fix (auth, bad
//! request, capacity) return immediately, so we never hammer the API pointlessly.
//!
//! It's deliberately a small generic combinator rather than something baked into each
//! provider: any `async fn -> Result<T>` can be wrapped, and the *caller* decides the
//! policy and which calls are worth retrying.

use std::time::Duration;

use crate::error::{Error, ProviderErrorKind, Result};

/// How hard to retry. `Default` is gentle: up to 4 retries, 1s doubling to a 30s cap.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff for the given attempt number (1-based), capped at `max_delay`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let mult = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.base_delay.saturating_mul(mult).min(self.max_delay)
    }
}

/// Whether a failure is worth retrying: throttling, 5xx, or a connect/timeout transport
/// error. Auth, capacity, bad-request, and decode errors are *not* retried.
pub fn is_retryable(e: &Error) -> bool {
    match e {
        Error::Provider { kind, .. } => {
            matches!(kind, ProviderErrorKind::RateLimited | ProviderErrorKind::Transient)
        }
        // reqwest transport-level errors: only connect/timeout are worth a retry; a
        // body/JSON decode error would just fail again.
        Error::Http(re) => re.is_timeout() || re.is_connect(),
        _ => false,
    }
}

/// Run `op`, retrying retryable failures with exponential backoff per `policy`.
/// Returns the first success, or the last error once retries are exhausted / the error
/// isn't retryable.
pub async fn retrying<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                if attempt > policy.max_retries || !is_retryable(&e) {
                    return Err(e);
                }
                tokio::time::sleep(policy.backoff(attempt)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn instant_policy() -> RetryPolicy {
        RetryPolicy { max_retries: 5, base_delay: Duration::ZERO, max_delay: Duration::ZERO }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(p.backoff(1), Duration::from_secs(1));
        assert_eq!(p.backoff(2), Duration::from_secs(2));
        assert_eq!(p.backoff(3), Duration::from_secs(4));
        assert_eq!(p.backoff(4), Duration::from_secs(8));
        assert_eq!(p.backoff(5), Duration::from_secs(10)); // capped
    }

    #[test]
    fn classifies_what_is_retryable() {
        assert!(is_retryable(&Error::Provider {
            kind: ProviderErrorKind::RateLimited,
            message: String::new()
        }));
        assert!(is_retryable(&Error::Provider {
            kind: ProviderErrorKind::Transient,
            message: String::new()
        }));
        assert!(!is_retryable(&Error::capacity("none")));
        assert!(!is_retryable(&Error::Provider {
            kind: ProviderErrorKind::Auth,
            message: String::new()
        }));
        assert!(!is_retryable(&Error::Config("x".into())));
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = AtomicU32::new(0);
        let out: Result<u32> = retrying(&instant_policy(), || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(Error::Provider { kind: ProviderErrorKind::Transient, message: "5xx".into() })
            } else {
                Ok(42)
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3); // failed twice, third ok
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let calls = AtomicU32::new(0);
        let out: Result<u32> = retrying(&instant_policy(), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Provider { kind: ProviderErrorKind::Transient, message: "5xx".into() })
        })
        .await;
        assert!(out.is_err());
        // initial try + max_retries
        assert_eq!(calls.load(Ordering::SeqCst), 1 + 5);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable() {
        let calls = AtomicU32::new(0);
        let out: Result<u32> = retrying(&instant_policy(), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Provider { kind: ProviderErrorKind::Auth, message: "401".into() })
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1); // no retries on auth
    }
}
