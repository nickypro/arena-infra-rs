//! Round-trip time to an endpoint, timed as a TCP connect. The handshake is one round trip, so
//! how long a connect takes is a fair RTT — without logging in, and from wherever arena runs,
//! which on prod is where every participant's SSH comes from (the proxy box). Read by
//! Hetzner's nearest-location walk and the dashboard's RTT column.
//!
//! A connect that is closed before any SSH is spoken counts, on OpenSSH ≥ 9.8, as a client
//! that "never completed authentication" (`PerSourcePenalties noauth`: 1 s each, enforced
//! from 15 s accrued). Whoever times an SSH port keeps to about one connect a minute per
//! endpoint, so even several dashboards on one box never come near getting it refused — the
//! box every participant's SSH comes through.

use std::time::Duration;

use tokio::net::{lookup_host, TcpStream, ToSocketAddrs};
use tokio::time::{timeout, Instant};

/// The fastest of `tries` TCP connects to `addr` (`"host:port"`, or `(host, port)`), to the
/// first of its addresses that answers; `None`: it didn't resolve, or nothing answered within
/// `wait` (which bounds the name lookup and each connect alike).
pub async fn connect_rtt<A: ToSocketAddrs>(addr: A, tries: u32, wait: Duration) -> Option<Duration> {
    let connect = |a: std::net::SocketAddr| async move {
        let t = Instant::now();
        matches!(timeout(wait, TcpStream::connect(a)).await, Ok(Ok(_))).then(|| t.elapsed())
    };
    let mut addrs = timeout(wait, lookup_host(addr)).await.ok()?.ok()?;
    let (a, mut best) = loop {
        let a = addrs.next()?;
        if let Some(d) = connect(a).await {
            break (a, d);
        }
    };
    for _ in 1..tries {
        if let Some(d) = connect(a).await {
            best = best.min(d);
        }
    }
    Some(best)
}

/// One TCP connect to `addr`'s first resolved address, timed — name lookup and connect
/// together within `wait`. Exactly one connect, whatever the name resolves to: what a probe
/// that must keep to one unauthenticated connect per endpoint uses (the dashboard's RTT).
/// `None`: it didn't resolve, or didn't answer within `wait`.
pub async fn connect_once<A: ToSocketAddrs>(addr: A, wait: Duration) -> Option<Duration> {
    within(wait, async {
        let a = lookup_host(addr).await.ok()?.next()?;
        let t = Instant::now();
        TcpStream::connect(a).await.ok().map(|_| t.elapsed())
    })
    .await
}

/// `timing`'s answer if it comes within `wait`, else `None` — the one bound on everything a
/// timing does.
async fn within(wait: Duration, timing: impl std::future::Future<Output = Option<Duration>>) -> Option<Duration> {
    timeout(wait, timing).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn times_a_listening_port_and_gives_up_on_one_that_isnt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let got = connect_rtt(("127.0.0.1", port), 2, Duration::from_secs(1)).await;
        assert!(got.is_some_and(|d| d < Duration::from_secs(1)), "{got:?}");
        assert!(connect_rtt(format!("127.0.0.1:{port}"), 1, Duration::from_secs(1)).await.is_some());
        // Not an address at all: unmeasured, not "0 ms".
        assert_eq!(connect_rtt("not an address", 1, Duration::from_secs(1)).await, None);
        assert!(connect_once(("127.0.0.1", port), Duration::from_secs(1)).await.is_some_and(|d| d < Duration::from_secs(1)));
        assert_eq!(connect_once("not an address", Duration::from_secs(1)).await, None);
    }

    /// One bound on the whole timing (review: a test that dialled TEST-NET-1 could pass on an
    /// unreachable route without ever reaching the timeout): a timing that never answers gives
    /// up at exactly `wait`, and one that answers in time is kept.
    #[tokio::test(start_paused = true)]
    async fn a_timing_gives_up_at_its_bound() {
        let wait = Duration::from_secs(1);
        let t = Instant::now();
        assert_eq!(within(wait, std::future::pending()).await, None);
        assert_eq!(t.elapsed(), wait);
        let quick = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Some(Duration::from_millis(30))
        };
        assert_eq!(within(wait, quick).await, Some(Duration::from_millis(30)));
    }
}
