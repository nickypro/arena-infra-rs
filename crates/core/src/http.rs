//! Reading provider HTTP responses: one place that decides "did it work, and what failed".
//!
//! The rule every backend (RunPod v1/v2, Vast, Hetzner, OpenRouter) follows: read the body
//! as **text** and look at the **status first**, only then parse JSON. Decoding first was a
//! real bug — a bad RunPod v1 key answers `401` with a body that isn't JSON, the decode
//! failed, and the operator saw "http error: error decoding response body": unclassified,
//! so not `Auth` (abort), and with the 401 nowhere in it. With the status checked first an
//! error response always becomes [`Error::provider_http`] (classified by status: 401/403
//! auth, 429 throttled, 5xx transient, capacity sniffed from the body), whatever its body.

use reqwest::{RequestBuilder, StatusCode};
use serde_json::Value;

use crate::error::{Error, Result};

/// Send `rb` and return its JSON body, or a classified error (see [`judge`]).
///
/// The body of an error response is read best-effort: if even that read fails, the status
/// alone still classifies the error — a 401 must never turn into a transport error.
pub(crate) async fn send_json(rb: RequestBuilder, ctx: &str) -> Result<Value> {
    let resp = rb.send().await?;
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
    let resp = rb.send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    Err(status_error(status, &text, ctx))
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
        let base = format!("http://{}", listener.local_addr().unwrap());
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
}
