//! Reading provider HTTP responses: one place that decides "did it work, and what failed".
//!
//! The rule every backend (RunPod v1/v2, Vast, Hetzner, OpenRouter) follows: read the body
//! as **text** and look at the **status first**, only then parse JSON. Decoding first was a
//! real bug — a bad RunPod v1 key answers `401` with a body that isn't JSON, the decode
//! failed, and the operator saw "http error: error decoding response body": unclassified,
//! so not `Auth` (abort), and with the 401 nowhere in it. With the status checked first an
//! error response always becomes [`Error::provider_http`] (classified by status: 401/403
//! auth, 429 throttled, 5xx transient, capacity sniffed from the body), whatever its body.
//!
//! And every request goes through one rate-limit-aware path, [`exchange`]: RunPod enforces a
//! per-key quota on all its APIs since 2026-10-27, shared by every process on the key (see
//! [`crate::ratelimit`]). It waits out a held quota or a 429 — never past what the caller can
//! wait — retries a 429 for any method (it is refused before processing), and resends only
//! reads after a 5xx or a network error.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::Value;
use tokio::time::Instant;

use crate::error::{Error, ProviderErrorKind, Result};
use crate::ratelimit::{self, wait_words, QuotaKey};

/// How hard one API's requests are retried ([`exchange`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// 429s retried per request, each after the server's wait.
    pub retries: u32,
    /// 5xx answers and network errors sent again per **read** (GET/HEAD), with backoff —
    /// never a write: a POST that timed out or got a 502 may have created something, which
    /// the callers' own "may have been created" handling decides.
    pub resends: u32,
    /// The first backoff step, doubled per retry, when the server names no wait.
    pub backoff: Duration,
    /// The longest one wait may be when the caller set no bound
    /// ([`ratelimit::waits_within`]): a minute window's reset, and a little. Asked for
    /// longer (an hour window spent), the call fails at once, saying how long.
    pub max_wait: Duration,
    /// Up to this much random delay added to each wait, so the processes sharing a key don't
    /// all retry in the same instant.
    pub jitter: Duration,
}

impl Limits {
    /// Every RunPod API (REST v2, REST v1, GraphQL): rate-limited since 2026-10-27 (see
    /// [`ratelimit`]); its reads are also resent after a 5xx or a network error.
    pub(crate) const RUNPOD: Limits = Limits {
        retries: 3,
        resends: 2,
        backoff: Duration::from_secs(1),
        max_wait: Duration::from_secs(65),
        jitter: Duration::from_secs(1),
    };

    /// Every other API (Vast, Hetzner, Shadeform, OpenRouter): a 429 is waited out and retried
    /// as on RunPod, but nothing else is resent here — their callers keep their own retry
    /// rules, which were written for one request per call.
    pub(crate) const OTHERS: Limits = Limits { resends: 0, ..Limits::RUNPOD };

    /// The backoff before the `n`th resend or retry (0-based): `backoff`, doubled each time.
    fn step(&self, n: u32) -> Duration {
        self.backoff.saturating_mul(2u32.saturating_pow(n.min(16))).min(Duration::from_secs(60))
    }
}

/// What [`exchange`] ends with.
pub(crate) enum Answer {
    /// The server's answer — any status but a 429 given up on — its body unread.
    Reply(Response),
    /// A 429 not retried any further: its body, and why not (for the error to say).
    Throttled { status: StatusCode, text: String, note: String },
}

/// How [`exchange`] sleeps: tokio's, or a test's recorder.
pub(crate) type Sleeper<'a> = &'a (dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Sync);

/// Send `rb` the rate-limit-aware way — the one path for every provider request:
/// - **Before sending**, a quota that is held (a 429's wait, or a window with `r` ≤ 1 until
///   its reset: [`ratelimit::Quota::block`]) is waited out — or, when that is longer than
///   the caller can wait ([`ratelimit::budget`]), the request fails at once as rate-limited,
///   not sent, with the wait in the message.
/// - **Every answer's** `RateLimit-Policy` and `RateLimit` are recorded for that quota
///   (process-wide, for the next request and for display).
/// - **A 429** — whatever the method: it is refused before any processing, so even a create
///   is safe to send again — holds the quota for the server's wait
///   ([`ratelimit::wait_for_429`]: `Retry-After`, else exponential backoff) and is retried
///   at most `limits.retries` times, never past the caller's bound; then it is
///   [`Answer::Throttled`].
/// - **A 5xx or a network error** is sent again only for a read (GET/HEAD), with exponential
///   backoff, at most `limits.resends` times; for anything else it is the caller's, as is.
///
/// A request that can't be built (a bad URL) or cloned (a streamed body) is sent once, as is.
pub(crate) async fn exchange(rb: RequestBuilder, ctx: &str, limits: &Limits) -> Result<Answer> {
    exchange_with(rb, ctx, limits, &|d| Box::pin(tokio::time::sleep(d))).await
}

pub(crate) async fn exchange_with(rb: RequestBuilder, ctx: &str, limits: &Limits, sleep: Sleeper<'_>) -> Result<Answer> {
    let Some(req) = rb.try_clone().and_then(|b| b.build().ok()) else {
        return Ok(Answer::Reply(rb.send().await?));
    };
    let key = QuotaKey::of(&req);
    let read = matches!(*req.method(), Method::GET | Method::HEAD);
    let (mut retried, mut resent) = (0u32, 0u32);
    loop {
        let sent_at = admit(&key, ctx, limits, sleep).await?;
        let Some(attempt) = rb.try_clone() else {
            return Ok(Answer::Reply(rb.send().await?));
        };
        let resp = match attempt.send().await {
            Ok(resp) => resp,
            Err(e) => {
                if read && resent < limits.resends && resendable(&e) {
                    if let Some(pause) = backoff(limits, resent) {
                        resent += 1;
                        sleep(pause).await;
                        continue;
                    }
                }
                return Err(e.into());
            }
        };
        let now = Instant::now();
        let status = resp.status();
        let state = header_text(&resp, "ratelimit");
        ratelimit::with(&key, |q| q.record(header_text(&resp, "ratelimit-policy").as_deref(), state.as_deref(), now));
        if status != StatusCode::TOO_MANY_REQUESTS {
            ratelimit::with(&key, |q| q.note_answered(sent_at));
            if status.is_server_error() && read && resent < limits.resends {
                if let Some(pause) = backoff(limits, resent) {
                    resent += 1;
                    sleep(pause).await;
                    continue;
                }
            }
            return Ok(Answer::Reply(resp));
        }
        let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(ratelimit::parse_retry_after);
        let text = resp.text().await.unwrap_or_default();
        let windows = state.as_deref().map(ratelimit::parse_state).unwrap_or_default();
        let body_after = ratelimit::body_retry_after(&text);
        let wait = ratelimit::with(&key, |q| {
            q.note_throttled(sent_at, retry_after, body_after, &windows, limits.backoff, refusal_words(&text), now)
        });
        if retried < limits.retries && wait <= ratelimit::budget(limits.max_wait, now) {
            // The hold just set is waited out by `admit`.
            retried += 1;
            continue;
        }
        let note = if retried >= limits.retries {
            format!("{} rate limit: still refused after {retried} retries; next request in {}", key.api(), wait_words(wait))
        } else {
            format!("{} rate limit: next request in {}, longer than this call waits", key.api(), wait_words(wait))
        };
        return Ok(Answer::Throttled { status, text, note });
    }
}

/// Let one request on `key`'s quota through, and say when: at once, or once its hold has
/// passed — re-checked after every wake, so a hold another request set or extended meanwhile
/// (or a low quota's spacing behind a request let through first) is honoured too. A hold
/// longer than the caller can wait refuses the request, not sent. A hold already waited out
/// isn't waited twice (a sleep that ended early).
async fn admit(key: &QuotaKey, ctx: &str, limits: &Limits, sleep: Sleeper<'_>) -> Result<Instant> {
    let mut waited: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let Some((until, why)) = ratelimit::with(key, |q| q.admit(now)) else {
            return Ok(now);
        };
        if waited.is_some_and(|w| until <= w) {
            ratelimit::with(key, |q| q.note_sent(now));
            return Ok(now);
        }
        let wait = until.saturating_duration_since(now);
        let budget = ratelimit::budget(limits.max_wait, now);
        if wait > budget {
            return Err(Error::Provider {
                kind: ProviderErrorKind::RateLimited,
                message: format!("{ctx}: not sent — {} rate limit ({why}): next request in {}", key.api(), wait_words(wait)),
            });
        }
        sleep((wait + jitter(limits.jitter)).min(budget)).await;
        waited = Some(until);
    }
}

/// The pause before the `n`th resend (0-based), jitter included and never past the caller's
/// bound — `None` when the backoff itself doesn't fit: the answer in hand is the caller's.
fn backoff(limits: &Limits, n: u32) -> Option<Duration> {
    let budget = ratelimit::budget(limits.max_wait, Instant::now());
    let wait = limits.step(n);
    (wait <= budget).then(|| (wait + jitter(limits.jitter)).min(budget))
}

/// A header's value(s) as text (several lines joined by `, `, as HTTP combines them).
fn header_text(resp: &Response, name: &str) -> Option<String> {
    let values: Vec<&str> = resp.headers().get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// What a 429 said, briefly: a problem+json `detail` ("rate limit exceeded for the minute
/// window"), else `HTTP 429`. Pure.
fn refusal_words(text: &str) -> String {
    let detail = serde_json::from_str::<Value>(text).ok().and_then(|v| v.get("detail").and_then(Value::as_str).map(str::to_string));
    match detail {
        Some(d) if !d.trim().is_empty() => d.trim().chars().take(100).collect(),
        _ => "HTTP 429".into(),
    }
}

/// Whether a transport error is worth sending a read again for: the connection failed,
/// broke or timed out (not a request we built wrong, nor a body we couldn't decode).
fn resendable(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

/// A random delay in `[0, max)`.
fn jitter(max: Duration) -> Duration {
    use std::hash::BuildHasher;
    let Ok(n) = u64::try_from(max.as_nanos()) else { return Duration::ZERO };
    if n == 0 {
        return Duration::ZERO;
    }
    let r = std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now());
    Duration::from_nanos(r % n)
}

/// The error for a 429 given up on: the classified error ([`status_error`], rate-limited),
/// with why it wasn't retried and how long until the next request.
pub(crate) fn throttled_error(status: StatusCode, text: &str, ctx: &str, note: &str) -> Error {
    match status_error(status, text, ctx) {
        Error::Provider { kind, message } => Error::Provider { kind, message: format!("{message} — {note}") },
        other => other,
    }
}

/// [`exchange`], with a 429 given up on as its error ([`throttled_error`]): the answer
/// otherwise, for a caller that reads it its own way.
pub(crate) async fn send_limited(rb: RequestBuilder, ctx: &str, limits: &Limits) -> Result<Response> {
    match exchange(rb, ctx, limits).await? {
        Answer::Reply(resp) => Ok(resp),
        Answer::Throttled { status, text, note } => Err(throttled_error(status, &text, ctx, &note)),
    }
}

/// Send `rb` and return its JSON body, or a classified error (see [`judge`]).
///
/// The body of an error response is read best-effort: if even that read fails, the status
/// alone still classifies the error — a 401 must never turn into a transport error.
pub(crate) async fn send_json(rb: RequestBuilder, ctx: &str) -> Result<Value> {
    send_json_with(rb, ctx, &Limits::OTHERS).await
}

/// [`send_json`] under `limits`.
pub(crate) async fn send_json_with(rb: RequestBuilder, ctx: &str, limits: &Limits) -> Result<Value> {
    let resp = send_limited(rb, ctx, limits).await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(status_error(status, &text, ctx));
    }
    let text = resp.text().await?;
    judge(status, &text, ctx)
}

/// Send `rb` and check only its status: for calls whose success body we don't use (stop,
/// restart, terminate, delete). A 2xx is success whatever its body says or however it's
/// encoded — a mutating call that worked must not be reported as failed because its
/// (ignored) body didn't parse.
pub(crate) async fn send_ok(rb: RequestBuilder, ctx: &str) -> Result<()> {
    let resp = send_limited(rb, ctx, &Limits::OTHERS).await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    Err(status_error(status, &text, ctx))
}

/// Send `rb` and hand back its status and body text, unjudged — for a caller with its own
/// reading of failures (`balance::classify`, which sorts them into transient / auth / gone /
/// drift and must never quote a body). Status first, as everywhere here: an error status's
/// body is read best-effort, so a body that can't be read never hides the status; on a 2xx
/// a body that can't be read is an error. A 429 given up on is its status and body too.
/// `Err` is a transport failure — or a request not sent because its quota is held (a
/// rate-limited error).
pub(crate) async fn send_text(rb: RequestBuilder) -> Result<(StatusCode, String)> {
    send_text_with(rb, &Limits::OTHERS).await
}

/// [`send_text`] under `limits`.
pub(crate) async fn send_text_with(rb: RequestBuilder, limits: &Limits) -> Result<(StatusCode, String)> {
    let resp = match exchange(rb, "request", limits).await? {
        Answer::Reply(resp) => resp,
        Answer::Throttled { status, text, .. } => return Ok((status, text)),
    };
    let status = resp.status();
    let text = if status.is_success() { resp.text().await? } else { resp.text().await.unwrap_or_default() };
    Ok((status, text))
}

/// Turn one response (status + raw body) into its JSON body or a classified error. Pure,
/// so the status handling is table-tested.
///
/// - 2xx with an empty body (`204 No Content`) → `Null`.
/// - 2xx with a body that isn't JSON → an `Other` error (not retried — a decode error
///   would just fail again). The start of an HTML/text body is quoted (it says what
///   answered); a broken *JSON* body is not — it's our data, and a pod object carries its
///   whole `env` (tokens), which must not land in terminals and cron logs.
/// - Anything else → [`status_error`].
pub(crate) fn judge(status: StatusCode, text: &str, ctx: &str) -> Result<Value> {
    if !status.is_success() {
        return Err(status_error(status, text, ctx));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).map_err(|e| {
        let body = text.trim_start();
        let shown = if body.starts_with('{') || body.starts_with('[') {
            format!("broken JSON of {} bytes, not shown", text.len())
        } else {
            body.chars().take(120).collect()
        };
        Error::provider(format!("{ctx}: HTTP {status} with a body that isn't JSON ({e}): {shown}"))
    })
}

/// The classified error for a non-2xx response ([`Error::provider_http`]). A JSON body
/// (RunPod v2's problem+json, Hetzner's `{error: …}`) is shown compacted; anything else (a
/// proxy's HTML 502, a bare `Unauthorized`, nothing at all) as its raw text — the status
/// decides the kind either way. `provider_http` truncates what it shows.
pub(crate) fn status_error(status: StatusCode, text: &str, ctx: &str) -> Error {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Error::provider_http(status, &v, ctx),
        Err(_) => Error::provider_http(status, &text.trim(), ctx),
    }
}

/// A tiny one-shot HTTP server on 127.0.0.1 for tests: answers each connection with the next
/// canned response, so a backend's real request/response path (status-first reading) runs
/// end to end without leaving the machine. std-only (a thread + `TcpListener`) — no extra
/// tokio features, no mock-server crate. It records each request's line and body, and can
/// also misbehave the ways a network does ([`hang_up`], [`stall`]).
#[cfg(test)]
pub(crate) mod test_server {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// What the server does once it has read a request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Then {
        /// Send the canned response.
        Reply,
        /// Close the connection without a word: the request was sent, its answer never comes.
        HangUp,
        /// Keep the connection open and never answer (until the [`Server`] is dropped).
        Stall,
    }

    /// One canned response.
    #[derive(Clone)]
    pub struct Canned {
        pub status: u16,
        pub content_type: &'static str,
        pub body: String,
        pub then: Then,
        /// Extra response headers (`Location` for a redirect…), sent as given.
        pub headers: Vec<(&'static str, String)>,
    }

    pub fn canned(status: u16, content_type: &'static str, body: &str) -> Canned {
        Canned { status, content_type, body: body.to_string(), then: Then::Reply, headers: Vec::new() }
    }

    impl Canned {
        /// The same response with one more header.
        pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Canned {
            self.headers.push((name, value.into()));
            self
        }
    }

    /// Read the request, then close the connection without answering.
    pub fn hang_up() -> Canned {
        Canned { then: Then::HangUp, ..canned(0, "", "") }
    }

    /// Read the request, then never answer.
    pub fn stall() -> Canned {
        Canned { then: Then::Stall, ..canned(0, "", "") }
    }

    /// A running server: its base URL, the request lines it has seen (`GET /pods`), each
    /// request's headers (lines as sent, e.g. `authorization: Bearer k`) and body (as text,
    /// `""` for none), in the same order.
    pub struct Server {
        pub base: String,
        pub requests: Arc<Mutex<Vec<String>>>,
        pub headers: Arc<Mutex<Vec<Vec<String>>>>,
        pub bodies: Arc<Mutex<Vec<String>>>,
        /// Stalled connections, held open for as long as the server lives.
        _held: Arc<Mutex<Vec<TcpStream>>>,
    }

    /// Serve `responses` in order, one per connection, then stop accepting.
    pub fn serve(responses: Vec<Canned>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().unwrap();
        // A port an earlier test's server had: its quota (holds, counts) isn't this server's.
        crate::ratelimit::forget_api(&format!("127.0.0.1:{}", addr.port()));
        let base = format!("http://{addr}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(Mutex::new(Vec::new()));
        let (seen, seen_headers, seen_bodies, holding) = (requests.clone(), headers.clone(), bodies.clone(), held.clone());
        std::thread::spawn(move || {
            for r in responses {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream);
                // Read the request head (and any body, so closing never resets the client).
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                seen.lock().unwrap().push(line.trim().to_string());
                let mut len = 0usize;
                let mut head = Vec::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    head.push(h.trim_end().to_string());
                }
                seen_headers.lock().unwrap().push(head);
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                seen_bodies.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
                let mut stream = reader.into_inner();
                match r.then {
                    Then::HangUp => drop(stream),
                    Then::Stall => holding.lock().unwrap().push(stream),
                    Then::Reply => {
                        let extra: String = r.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                        let reply = format!(
                            "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{}",
                            r.status,
                            r.content_type,
                            r.body.len(),
                            r.body
                        );
                        let _ = stream.write_all(reply.as_bytes());
                        let _ = stream.flush();
                    }
                }
            }
        });
        Server { base, requests, headers, bodies, _held: held }
    }

    /// A client that never uses a system proxy (an `HTTP_PROXY` in the test environment
    /// must not route loopback requests elsewhere).
    pub fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{canned, client, serve};
    use super::*;
    use crate::error::ProviderErrorKind as K;

    use std::sync::{Arc, Mutex};

    /// RunPod's limits without the jitter, so a test can check each wait.
    const EXACT: Limits = Limits { jitter: Duration::ZERO, ..Limits::RUNPOD };

    /// What a test's sleeper is: [`Sleeper`] owned.
    type SleepFn = dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Sync;

    /// A sleeper that only records what it was asked to wait.
    fn recorder() -> (Arc<Mutex<Vec<Duration>>>, Box<SleepFn>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = waits.clone();
        (
            waits,
            Box::new(move |d| {
                seen.lock().unwrap().push(d);
                Box::pin(async {})
            }),
        )
    }

    /// Each wait is the one wanted, give or take the moments the test itself took.
    fn assert_waits(got: &[Duration], want: &[u64]) {
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(want) {
            let w = Duration::from_secs(*w);
            assert!(*g <= w && *g + Duration::from_millis(500) > w, "waited {g:?}, wanted {w:?}: {got:?}");
        }
    }

    async fn json(rb: RequestBuilder, limits: &Limits, sleep: Sleeper<'_>) -> Result<Value> {
        match exchange_with(rb, "list pods", limits, sleep).await? {
            Answer::Reply(resp) => {
                let status = resp.status();
                judge(status, &resp.text().await.unwrap_or_default(), "list pods")
            }
            Answer::Throttled { status, text, note } => Err(throttled_error(status, &text, "list pods", &note)),
        }
    }

    /// A 429 with `Retry-After`, then a 200: retried once, after exactly the wait asked.
    #[tokio::test]
    async fn a_429_is_retried_after_the_wait_it_asks_for() {
        let srv = serve(vec![
            canned(429, "application/problem+json", r#"{"detail":"rate limit exceeded for the minute window"}"#).header("Retry-After", "2"),
            canned(200, "application/json", r#"{"ok":1}"#),
        ]);
        let (waits, sleep) = recorder();
        let v = json(client().get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap();
        assert_eq!(v["ok"], 1);
        assert_eq!(srv.requests.lock().unwrap().len(), 2);
        assert_waits(&waits.lock().unwrap(), &[2]);
    }

    /// A 429 that names no wait: exponential backoff, 1 s then 2 s.
    #[tokio::test]
    async fn a_429_without_retry_after_backs_off_exponentially() {
        let srv = serve(vec![canned(429, "text/plain", "slow down"), canned(429, "text/plain", "slow down"), canned(200, "application/json", "{}")]);
        let (waits, sleep) = recorder();
        json(client().get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap();
        assert_eq!(srv.requests.lock().unwrap().len(), 3);
        assert_waits(&waits.lock().unwrap(), &[1, 2]);
        // Vast says its wait in the body.
        let srv = serve(vec![canned(429, "application/json", r#"{"detail":"too frequent","retry_after":3}"#), canned(200, "application/json", "{}")]);
        let (waits, sleep) = recorder();
        json(client().get(format!("{}/x", srv.base)), &EXACT, &sleep).await.unwrap();
        assert_waits(&waits.lock().unwrap(), &[3]);
    }

    /// At most `retries` retries; then the error is rate-limited and says so — which API's
    /// limit, that it gave up, and the wait.
    #[tokio::test]
    async fn retries_are_capped_and_the_error_names_the_rate_limit() {
        let srv = serve((0..5).map(|_| canned(429, "application/problem+json", r#"{"detail":"rate limit exceeded for the minute window"}"#).header("Retry-After", "1")).collect());
        let (waits, sleep) = recorder();
        let e = json(client().get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::RateLimited), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("list pods HTTP 429") && msg.contains("rate limit exceeded for the minute window"), "{msg}");
        assert!(msg.contains("rate limit: still refused after 3 retries; next request in 1 s"), "{msg}");
        assert_eq!(srv.requests.lock().unwrap().len(), 4, "the request and 3 retries");
        assert_waits(&waits.lock().unwrap(), &[1, 1, 1]);
    }

    /// A 429 is refused before anything happens, so even a create is sent again; a 5xx or a
    /// lost connection may have created something, so a write is never resent — a read is,
    /// with backoff.
    #[tokio::test]
    async fn only_reads_are_resent_after_a_5xx_or_a_network_error() {
        use super::test_server::hang_up;
        let c = client();
        let srv = serve(vec![canned(429, "text/plain", "").header("Retry-After", "1"), canned(201, "application/json", r#"{"id":"p1"}"#)]);
        let (waits, sleep) = recorder();
        let v = json(c.post(format!("{}/pods", srv.base)).body("{}"), &EXACT, &sleep).await.unwrap();
        assert_eq!((v["id"].as_str(), srv.requests.lock().unwrap().len()), (Some("p1"), 2));
        assert_waits(&waits.lock().unwrap(), &[1]);

        for (method, reply) in [(Method::POST, canned(502, "text/html", "<html>Bad Gateway</html>")), (Method::POST, hang_up()), (Method::PATCH, canned(503, "text/plain", "")), (Method::DELETE, hang_up())] {
            let srv = serve(vec![reply, canned(200, "application/json", "{}")]);
            let (waits, sleep) = recorder();
            let r = json(c.request(method.clone(), format!("{}/pods", srv.base)).body("{}"), &EXACT, &sleep).await;
            assert!(r.is_err(), "{method}: {r:?}");
            assert_eq!((srv.requests.lock().unwrap().len(), waits.lock().unwrap().len()), (1, 0), "{method}: sent once");
        }

        let srv = serve(vec![canned(502, "text/html", "<html>Bad Gateway</html>"), hang_up(), canned(200, "application/json", r#"{"ok":1}"#)]);
        let (waits, sleep) = recorder();
        let v = json(c.get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap();
        assert_eq!((v["ok"].as_i64(), srv.requests.lock().unwrap().len()), (Some(1), 3));
        assert_waits(&waits.lock().unwrap(), &[1, 2]);
        // Resends are capped too: the last answer is the caller's, as is.
        let srv = serve((0..4).map(|_| canned(503, "text/plain", "down")).collect());
        let (_, sleep) = recorder();
        let e = json(c.get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap_err();
        assert_eq!((e.kind(), srv.requests.lock().unwrap().len()), (Some(K::Transient), 3));
        // Others' reads aren't resent here: their callers have their own rules.
        let srv = serve(vec![canned(503, "text/plain", "down"), canned(200, "application/json", "{}")]);
        let e = send_json(c.get(format!("{}/x", srv.base)), "vast list").await.unwrap_err();
        assert_eq!((e.kind(), srv.requests.lock().unwrap().len()), (Some(K::Transient), 1));
    }

    /// `RateLimit` with `r=0`: the next request waits for the reset — or, when its caller can't
    /// wait that long, fails at once, not sent, saying how long. The quota and its state are
    /// kept for display.
    #[tokio::test]
    async fn a_spent_window_holds_the_next_request() {
        let spent = r#""minute";r=0;t=12, "hour";r=2800;t=1812"#;
        let srv = serve(vec![
            canned(200, "application/json", "{}").header("RateLimit-Policy", r#""minute";q=60;w=60, "hour";q=3000;w=3600"#).header("RateLimit", spent),
            canned(200, "application/json", "{}").header("RateLimit", spent),
            canned(200, "application/json", "{}"),
        ]);
        let c = client();
        let (waits, sleep) = recorder();
        json(c.get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap();
        assert!(waits.lock().unwrap().is_empty(), "nothing held before the first answer");
        // A caller with 100 ms to spare: refused before sending.
        let e = ratelimit::waits_within(Duration::from_millis(100), json(c.get(format!("{}/pods", srv.base)), &EXACT, &sleep)).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::RateLimited), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("list pods: not sent") && msg.contains("the minute window is spent") && msg.contains("next request in 12 s"), "{msg}");
        assert_eq!(srv.requests.lock().unwrap().len(), 1, "not sent");
        assert!(waits.lock().unwrap().is_empty());
        // A caller without a bound waits it out (up to `max_wait`), then sends.
        json(c.get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap();
        assert_eq!(srv.requests.lock().unwrap().len(), 2);
        assert_waits(&waits.lock().unwrap(), &[12]);
        // What `--limits` and the dashboard read.
        let api = format!("127.0.0.1:{}", srv.base.rsplit(':').next().unwrap());
        let view = ratelimit::observed().into_iter().find(|v| v.api == api).expect("recorded");
        assert_eq!(view.raw_policy.as_deref(), Some(r#""minute";q=60;w=60, "hour";q=3000;w=3600"#));
        assert_eq!(view.raw_state.as_deref(), Some(spent));
        assert_eq!((view.sent, view.throttled), (2, 0));
        assert!(view.held.as_ref().is_some_and(|(d, _)| *d > Duration::from_secs(10)), "{view:?}");
    }

    /// A 429 asking for longer than the caller can wait isn't retried: one request, and the
    /// error says how long.
    #[tokio::test]
    async fn a_429_asking_longer_than_the_caller_waits_fails_at_once() {
        let srv = serve(vec![canned(429, "text/plain", "").header("Retry-After", "1800"), canned(200, "application/json", "{}")]);
        let (waits, sleep) = recorder();
        let e = json(client().get(format!("{}/pods", srv.base)), &EXACT, &sleep).await.unwrap_err();
        assert_eq!(e.kind(), Some(K::RateLimited), "{e}");
        assert!(e.to_string().contains("next request in 30 min, longer than this call waits"), "{e}");
        assert_eq!((srv.requests.lock().unwrap().len(), waits.lock().unwrap().len()), (1, 0));
        // `send_text` hands the 429 back as an answer, for its caller to judge.
        let srv = serve(vec![canned(429, "text/plain", "busy").header("Retry-After", "1800")]);
        let (s, t) = send_text(client().get(format!("{}/x", srv.base))).await.unwrap();
        assert_eq!((s.as_u16(), t.as_str()), (429, "busy"));
    }

    /// A hold extended while a request waits (another request's 429 meanwhile) is waited out
    /// too — or, past the caller's bound, refuses the request, not sent.
    #[tokio::test]
    async fn a_hold_extended_during_the_wait_is_honoured() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let sleeper = |key: QuotaKey, waits: Arc<Mutex<Vec<Duration>>>| {
            let extended = AtomicBool::new(false);
            move |d: Duration| -> Pin<Box<dyn Future<Output = ()> + Send>> {
                waits.lock().unwrap().push(d);
                if !extended.swap(true, Ordering::SeqCst) {
                    let now = Instant::now();
                    ratelimit::with(&key, |q| q.note_throttled(now, Some(Duration::from_secs(10)), None, &[], Duration::from_secs(1), "another 429".into(), now));
                }
                Box::pin(async {})
            }
        };
        let c = client();
        for bounded in [false, true] {
            let srv = serve(vec![canned(429, "text/plain", "").header("Retry-After", "2"), canned(200, "application/json", "{}")]);
            let url = format!("{}/pods", srv.base);
            let key = QuotaKey::of(&c.get(&url).build().unwrap());
            let waits = Arc::new(Mutex::new(Vec::new()));
            let sleep = sleeper(key, waits.clone());
            let call = json(c.get(&url), &EXACT, &sleep);
            if bounded {
                let e = ratelimit::waits_within(Duration::from_secs(5), call).await.unwrap_err();
                assert!(e.to_string().contains("not sent") && e.to_string().contains("another 429"), "{e}");
                assert_eq!(srv.requests.lock().unwrap().len(), 1);
                assert_waits(&waits.lock().unwrap(), &[2]);
            } else {
                call.await.unwrap();
                assert_eq!(srv.requests.lock().unwrap().len(), 2);
                assert_waits(&waits.lock().unwrap(), &[2, 10]);
            }
        }
    }

    /// A resend's pause, jitter included, never runs past the caller's bound.
    #[tokio::test]
    async fn a_resend_pause_stays_inside_the_bound() {
        let srv = serve(vec![canned(503, "text/plain", "down"), canned(200, "application/json", "{}")]);
        let (waits, sleep) = recorder();
        let jittery = Limits { jitter: Duration::from_secs(10), ..Limits::RUNPOD };
        ratelimit::waits_within(Duration::from_millis(1_500), json(client().get(format!("{}/pods", srv.base)), &jittery, &sleep)).await.unwrap();
        let waits = waits.lock().unwrap();
        assert_eq!(waits.len(), 1);
        assert!(waits[0] >= Duration::from_secs(1) && waits[0] <= Duration::from_millis(1_500), "{waits:?}");
    }

    #[test]
    fn refusal_words_table() {
        assert_eq!(refusal_words(r#"{"title":"Too Many Requests","detail":"rate limit exceeded for the minute window"}"#), "rate limit exceeded for the minute window");
        assert_eq!(refusal_words("slow down"), "HTTP 429");
        assert_eq!(refusal_words(r#"{"detail":"  "}"#), "HTTP 429");
        assert_eq!(refusal_words(""), "HTTP 429");
    }

    #[test]
    fn judge_checks_the_status_before_the_body() {
        // (case, status, body, Ok?, kind)
        let cases: Vec<(&str, u16, &str, bool, Option<K>)> = vec![
            ("200 json", 200, r#"{"id":"p1"}"#, true, None),
            ("204 empty", 204, "", true, None),
            ("200 not json", 200, "<html>ok</html>", false, Some(K::Other)),
            // The live repro: a bogus key → 401 whose body isn't JSON. Must be Auth.
            ("401 plain text", 401, "Unauthorized", false, Some(K::Auth)),
            ("401 html", 401, "<html><body>401 Authorization Required</body></html>", false, Some(K::Auth)),
            ("401 empty", 401, "", false, Some(K::Auth)),
            ("401 json", 401, r#"{"error":"invalid api key"}"#, false, Some(K::Auth)),
            ("403 html", 403, "<html>Forbidden</html>", false, Some(K::Auth)),
            ("429 text", 429, "slow down", false, Some(K::RateLimited)),
            ("502 html", 502, "<html>Bad Gateway</html>", false, Some(K::Transient)),
            ("500 capacity json", 500, r#"{"error":"There are no instances currently available"}"#, false, Some(K::Capacity)),
            ("400 bad request", 400, r#"{"error":"bad image"}"#, false, Some(K::Other)),
        ];
        for (case, code, body, ok, kind) in cases {
            let r = judge(StatusCode::from_u16(code).unwrap(), body, "list pods");
            assert_eq!(r.is_ok(), ok, "{case}: {r:?}");
            if let Err(e) = r {
                assert_eq!(e.kind(), kind, "{case}: {e}");
                if code >= 300 {
                    let msg = e.to_string();
                    assert!(msg.contains(&format!("list pods HTTP {code}")), "{case}: the status is in the message: {msg}");
                }
            }
        }
        // A 2xx that isn't JSON: an HTML/text body is quoted, a broken JSON one never is.
        let e = judge(StatusCode::OK, "<html>maintenance</html>", "list pods").unwrap_err().to_string();
        assert!(e.contains("<html>maintenance</html>"), "{e}");
        let e = judge(StatusCode::CREATED, r#"{"id":"p1","env":{"HF_TOKEN":"hf_SECRET""#, "create pod").unwrap_err().to_string();
        assert!(!e.contains("SECRET") && e.contains("broken JSON of"), "{e}");
        // A non-JSON error body is shown as text (trimmed), never as a decode error.
        let e = status_error(StatusCode::UNAUTHORIZED, "  Unauthorized\n", "list pods");
        assert_eq!(e.to_string(), "provider error: list pods HTTP 401 Unauthorized: Unauthorized");
        // A JSON one is compacted.
        let e = status_error(StatusCode::NOT_FOUND, "{ \"detail\" : \"pod not found\" }", "get pod");
        assert_eq!(e.to_string(), r#"provider error: get pod HTTP 404 Not Found: {"detail":"pod not found"}"#);
    }

    /// End to end over a real socket: what the backends call.
    #[tokio::test]
    async fn send_json_and_send_ok_classify_a_401_html_body_as_auth() {
        let srv = serve(vec![
            canned(401, "text/html", "<html>401 Authorization Required</html>"),
            canned(200, "application/json", r#"[{"id":"a"}]"#),
            canned(403, "text/plain", "Forbidden"),
            canned(200, "text/plain", "OK (not json)"),
        ]);
        let c = client();
        let e = send_json(c.get(format!("{}/pods", srv.base)), "list pods").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("list pods HTTP 401"), "{e}");
        let v = send_json(c.get(format!("{}/pods", srv.base)), "list pods").await.unwrap();
        assert_eq!(v[0]["id"], "a");
        let e = send_ok(c.post(format!("{}/pods/a/stop", srv.base)), "stop pod").await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        // A 2xx is success for a status-only call, whatever its body.
        send_ok(c.delete(format!("{}/pods/a", srv.base)), "terminate pod").await.unwrap();
        assert_eq!(
            *srv.requests.lock().unwrap(),
            ["GET /pods HTTP/1.1", "GET /pods HTTP/1.1", "POST /pods/a/stop HTTP/1.1", "DELETE /pods/a HTTP/1.1"]
        );
    }

    /// `send_text` hands back any status with its body, unjudged; only a transport failure
    /// (here: the server hanging up) is an error.
    #[tokio::test]
    async fn send_text_returns_status_and_body_and_errs_only_on_transport() {
        use super::test_server::hang_up;
        let srv = serve(vec![
            canned(503, "text/html", "<html>down</html>"),
            canned(200, "application/json", r#"{"ok":1}"#),
            hang_up(),
        ]);
        let c = client();
        let (s, t) = send_text(c.get(format!("{}/x", srv.base))).await.unwrap();
        assert_eq!((s.as_u16(), t.as_str()), (503, "<html>down</html>"));
        let (s, t) = send_text(c.get(format!("{}/x", srv.base))).await.unwrap();
        assert_eq!((s.as_u16(), t.as_str()), (200, r#"{"ok":1}"#));
        assert!(send_text(c.get(format!("{}/x", srv.base))).await.is_err());
    }
}
