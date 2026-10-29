//! Provider rate limits: what the server said about its quota, kept process-wide, and the
//! decisions made from it — the decisions pure, so they're table-tested.
//!
//! Since 2026-10-27 every RunPod public API enforces a rate limit (RunPod's service email:
//! REST v1 at 180/min, 3,600/h, 43,200/day, stepping down each Monday until its sunset). The
//! v2 management API (`api.runpod.io/v2`) gives each user one quota per window — minute, hour,
//! day — every request counting against all three, and **every process using the key shares
//! it**: prod's dashboards and its backup cron all draw on one key. The quotas aren't
//! published; each answer carries them:
//! - `RateLimit-Policy: "minute";q=60;w=60, "hour";q=3000;w=3600, …` — `q` requests per `w`
//!   seconds ([`parse_policy`]);
//! - `RateLimit: "minute";r=0;t=12, …` — `r` left, `t` seconds to the window's reset
//!   ([`parse_state`]);
//! - a refusal is `429` with `Retry-After: 12` and a problem+json `detail` naming the window.
//!
//! No headers means no limit. Both headers are RFC 9651 structured-field lists, read
//! tolerantly ([`sf_list`]): an unknown window or parameter is skipped, and a header that
//! can't be read at all counts as absent — never as "spent", which would stall every caller.
//!
//! [`crate::http`] sends each request through this module: the quota a request draws on is
//! its API (origin and first path segment — RunPod's REST v2, REST v1 and GraphQL are kept
//! apart, since nothing says they share one) and its key ([`QuotaKey`]). Before sending, a
//! quota that is held ([`Quota::block`]: a 429's wait, or a window with `r` ≤ 1 until its
//! reset) makes the request wait — or, when the wait is longer than the caller can afford
//! ([`waits_within`]), fail at once as rate-limited without being sent. What each quota last
//! reported, and the requests this process sent in the last minute, are kept for display
//! ([`observed`]: `arena api get … --limits`, the dashboard's footer).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// One window of a `RateLimit-Policy`: `q` requests per `w` seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyWindow {
    pub name: String,
    pub quota: Option<u64>,
    pub window: Option<u64>,
}

/// One window of a `RateLimit`: `r` requests left, `t` seconds until the window resets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateWindow {
    pub name: String,
    pub remaining: Option<u64>,
    pub reset: Option<u64>,
}

// ---------------------------------------------------------------------------------------
// Structured fields (RFC 9651), the subset these headers use
// ---------------------------------------------------------------------------------------

/// A bare item, as far as these headers need one.
#[derive(Debug, Clone, PartialEq)]
enum Bare {
    Int(i64),
    Dec(f64),
    /// A string or a token: a window's name.
    Name(String),
    /// Anything else (a boolean, a byte sequence, a date, an inner list): skipped.
    Other,
}

impl Bare {
    /// A non-negative count, rounded down (`r`: never more left than said).
    fn count_down(&self) -> Option<u64> {
        match *self {
            Bare::Int(n) => u64::try_from(n).ok(),
            Bare::Dec(f) if f.is_finite() && f >= 0.0 => Some(f.floor() as u64),
            _ => None,
        }
    }

    /// A non-negative number of seconds, rounded up (`t`: never a reset sooner than said).
    fn count_up(&self) -> Option<u64> {
        match *self {
            Bare::Int(n) => u64::try_from(n).ok(),
            Bare::Dec(f) if f.is_finite() && f >= 0.0 => Some(f.ceil() as u64),
            _ => None,
        }
    }
}

/// One list member: its item and its parameters, in order.
#[derive(Debug, Clone, PartialEq)]
struct Member {
    item: Bare,
    params: Vec<(String, Bare)>,
}

impl Member {
    fn name(&self) -> Option<&str> {
        match &self.item {
            Bare::Name(n) if !n.is_empty() => Some(n),
            _ => None,
        }
    }

    /// The first parameter called `key`.
    fn param(&self, key: &str) -> Option<&Bare> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && (b[*i] == b' ' || b[*i] == b'\t') {
        *i += 1;
    }
}

/// Past a quoted string starting at `b[*i] == b'"'`, its content unescaped.
fn string(b: &[u8], i: &mut usize) -> String {
    let mut out = Vec::new();
    *i += 1;
    while *i < b.len() {
        match b[*i] {
            b'\\' if *i + 1 < b.len() => {
                out.push(b[*i + 1]);
                *i += 2;
            }
            b'"' => {
                *i += 1;
                break;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Past everything up to (not including) the next `stop` byte outside a quoted string.
fn skip_until(b: &[u8], i: &mut usize, stop: u8) {
    while *i < b.len() && b[*i] != stop {
        if b[*i] == b'"' {
            string(b, i);
        } else {
            *i += 1;
        }
    }
}

fn token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b":/!#$%&'*+-.^_`|~".contains(&c)
}

/// One bare item at `b[*i]`, advancing past it. Never fails: what isn't understood is
/// [`Bare::Other`], and the caller skips to the next member.
fn bare(b: &[u8], i: &mut usize) -> Bare {
    let Some(&c) = b.get(*i) else { return Bare::Other };
    match c {
        b'"' => Bare::Name(string(b, i)),
        b'(' => {
            *i += 1;
            skip_until(b, i, b')');
            *i = (*i + 1).min(b.len());
            Bare::Other
        }
        b':' => {
            *i += 1;
            while *i < b.len() && b[*i] != b':' {
                *i += 1;
            }
            *i = (*i + 1).min(b.len());
            Bare::Other
        }
        b'-' | b'0'..=b'9' => {
            let start = *i;
            *i += 1;
            while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == b'.') {
                *i += 1;
            }
            let text = String::from_utf8_lossy(&b[start..*i]);
            if text.contains('.') {
                text.parse().map(Bare::Dec).unwrap_or(Bare::Other)
            } else {
                text.parse().map(Bare::Int).unwrap_or(Bare::Other)
            }
        }
        c if c.is_ascii_alphabetic() || c == b'*' => {
            let start = *i;
            while *i < b.len() && token_char(b[*i]) {
                *i += 1;
            }
            Bare::Name(String::from_utf8_lossy(&b[start..*i]).into_owned())
        }
        // `?1`, `@1700000000`, `%"…"` and anything unexpected: not a name or a count.
        _ => {
            *i += 1;
            while *i < b.len() && !matches!(b[*i], b';' | b',' | b' ' | b'\t') {
                if b[*i] == b'"' {
                    string(b, i);
                } else {
                    *i += 1;
                }
            }
            Bare::Other
        }
    }
}

/// A parameter key at `b[*i]` (lowercased: the RFC allows only lowercase, a server
/// that sends `R=` still means `r`).
fn key(b: &[u8], i: &mut usize) -> String {
    let start = *i;
    while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b"_-.*".contains(&b[*i])) {
        *i += 1;
    }
    String::from_utf8_lossy(&b[start..*i]).to_ascii_lowercase()
}

/// A structured-field list (RFC 9651 §3.1), tolerantly: each member's item and
/// parameters; whatever a member has that can't be read is skipped up to the next comma
/// (outside quotes), so one odd member never costs the others. Pure.
fn sf_list(header: &str) -> Vec<Member> {
    let b = header.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    loop {
        skip_ws(b, &mut i);
        if i >= b.len() {
            break;
        }
        let item = if b[i] == b',' { Bare::Other } else { bare(b, &mut i) };
        let mut params = Vec::new();
        loop {
            skip_ws(b, &mut i);
            if b.get(i) != Some(&b';') {
                break;
            }
            i += 1;
            skip_ws(b, &mut i);
            let k = key(b, &mut i);
            let v = if b.get(i) == Some(&b'=') {
                i += 1;
                bare(b, &mut i)
            } else {
                Bare::Other // a boolean `true` parameter
            };
            if k.is_empty() {
                // Not a key at all: give up on this member's parameters.
                break;
            }
            params.push((k, v));
        }
        out.push(Member { item, params });
        skip_until(b, &mut i, b',');
        if i >= b.len() {
            break;
        }
        i += 1;
    }
    out
}

/// `RateLimit-Policy`'s windows: a name (a string or a token) with `q` (quota) and `w`
/// (seconds), each optional. Members without a name are skipped; unknown parameters
/// ignored. Pure.
pub fn parse_policy(header: &str) -> Vec<PolicyWindow> {
    sf_list(header)
        .iter()
        .filter_map(|m| {
            Some(PolicyWindow {
                name: m.name()?.to_string(),
                quota: m.param("q").and_then(Bare::count_down),
                window: m.param("w").and_then(Bare::count_up),
            })
        })
        .collect()
}

/// `RateLimit`'s windows: a name with `r` (requests left, rounded down) and `t` (seconds to
/// the reset, rounded up), each optional. Members without a name are skipped; unknown
/// parameters ignored. Pure.
pub fn parse_state(header: &str) -> Vec<StateWindow> {
    sf_list(header)
        .iter()
        .filter_map(|m| {
            Some(StateWindow {
                name: m.name()?.to_string(),
                remaining: m.param("r").and_then(Bare::count_down),
                reset: m.param("t").and_then(Bare::count_up),
            })
        })
        .collect()
}

/// The longest wait any header may ask for: a day. Past it a value is garbage, not a wait
/// (and `Duration::from_secs_f64` would panic on an absurd one).
const MAX_ASKED: f64 = 86_400.0;

/// `Retry-After` in seconds (a decimal too). An HTTP-date, a negative or absurd number:
/// `None` — the caller falls back to the quota's reset or its own backoff. Pure.
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    let s: f64 = v.trim().parse().ok()?;
    (s.is_finite() && (0.0..=MAX_ASKED).contains(&s)).then(|| Duration::from_secs_f64(s))
}

/// A JSON error body's own `retry_after` (Vast's 429: `{"retry_after": 12}` or `"12"`), as
/// [`parse_retry_after`]. Pure.
pub fn body_retry_after(text: &str) -> Option<Duration> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let after = v.get("retry_after")?;
    let s = after.as_f64().or_else(|| after.as_str().and_then(|s| s.trim().parse().ok()))?;
    (s.is_finite() && (0.0..=MAX_ASKED).contains(&s)).then(|| Duration::from_secs_f64(s))
}

// ---------------------------------------------------------------------------------------
// One quota's state
// ---------------------------------------------------------------------------------------

/// A window with this many requests left (or fewer) is treated as spent until its reset:
/// the last request is left to the other processes on the key, and a reading a moment old
/// may already be one too high.
pub const SPENT_AT: u64 = 1;

/// How far apart two readings' resets may be and still be the same window instance (`t` is
/// whole seconds, and answers take time).
const SAME_WINDOW: Duration = Duration::from_secs(2);

/// What this process knows about one quota.
#[derive(Debug, Clone, Default)]
pub struct Quota {
    /// The provider it belongs to (`runpod`, `vast`… — `""` for an API we don't know).
    pub provider: &'static str,
    /// What to call it: `RunPod REST v2`, `RunPod GraphQL`, or the host for an unknown API.
    pub api: String,
    pub policy: Vec<PolicyWindow>,
    /// `RateLimit-Policy` as last received.
    pub raw_policy: Option<String>,
    /// The windows as of `seen`.
    pub state: Vec<StateWindow>,
    /// `RateLimit` as last received.
    pub raw_state: Option<String>,
    /// When `state` was read.
    pub seen: Option<Instant>,
    /// A 429's wait: nothing is sent before this, and why.
    pub hold: Option<(Instant, String)>,
    /// 429s in a row (the backoff's exponent when the server names no wait).
    pub strikes: u32,
    /// When the requests of the last minute were sent.
    pub recent: VecDeque<Instant>,
    /// Requests sent, and 429s received, by this process.
    pub sent: u64,
    pub throttled: u64,
}

/// The window reading to keep when a new one arrives: the new one — except that within the
/// same window instance (resets within [`SAME_WINDOW`]) `r` only goes down, so an answer
/// that was sent earlier but read later can't raise it again. Pure.
fn merge_window(prev: Option<(&StateWindow, Instant)>, new: &StateWindow, now: Instant) -> StateWindow {
    let mut out = new.clone();
    if let (Some((p, seen)), Some(t_new)) = (prev, new.reset) {
        let reset_prev = p.reset.map(|t| seen + Duration::from_secs(t));
        let reset_new = now + Duration::from_secs(t_new);
        let same = reset_prev.is_some_and(|r| r.max(reset_new) - r.min(reset_new) <= SAME_WINDOW);
        if let (true, Some(rp), Some(rn)) = (same, p.remaining, new.remaining) {
            out.remaining = Some(rp.min(rn));
        }
    }
    out
}

impl Quota {
    /// Record one answer's headers. `RateLimit-Policy` replaces the last one; `RateLimit`
    /// too, window by window through [`merge_window`]. Absent headers change nothing. Pure.
    pub fn record(&mut self, policy: Option<&str>, state: Option<&str>, now: Instant) {
        if let Some(p) = policy {
            self.policy = parse_policy(p);
            self.raw_policy = Some(p.to_string());
        }
        if let Some(s) = state {
            let prev_seen = self.seen;
            let merged = parse_state(s)
                .iter()
                .map(|w| {
                    let prev = prev_seen.and_then(|seen| self.state.iter().find(|p| p.name == w.name).map(|p| (p, seen)));
                    merge_window(prev, w, now)
                })
                .collect();
            self.state = merged;
            self.raw_state = Some(s.to_string());
            self.seen = Some(now);
        }
    }

    /// A request was sent at `now`.
    pub fn note_sent(&mut self, now: Instant) {
        self.sent += 1;
        self.recent.push_back(now);
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while self.recent.front().is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(60)) {
            self.recent.pop_front();
        }
    }

    /// A 429 at `now`: hold every request for `wait`. Returns the strikes before this one.
    pub fn note_throttled(&mut self, wait: Duration, why: String, now: Instant) -> u32 {
        let before = self.strikes;
        self.strikes = self.strikes.saturating_add(1);
        self.throttled += 1;
        let until = now + wait;
        // Never shorten a longer hold another request set.
        if self.hold.as_ref().is_none_or(|(t, _)| *t <= until) {
            self.hold = Some((until, why));
        }
        before
    }

    /// An answer that wasn't a 429: the streak is over.
    pub fn note_answered(&mut self) {
        self.strikes = 0;
    }

    /// Whether, and until when, nothing should be sent: the later of a 429's hold and the
    /// reset of every window with at most [`SPENT_AT`] requests left — with why. `None` once
    /// those have passed. Pure.
    pub fn block(&self, now: Instant) -> Option<(Instant, String)> {
        let held = self.hold.clone().filter(|(t, _)| *t > now);
        let spent = self.seen.and_then(|seen| {
            self.state
                .iter()
                .filter(|w| w.remaining.is_some_and(|r| r <= SPENT_AT))
                .filter_map(|w| {
                    let until = seen + Duration::from_secs(w.reset?);
                    let why = match w.remaining {
                        Some(0) => format!("the {} window is spent", w.name),
                        _ => format!("1 request left in the {} window", w.name),
                    };
                    Some((until, why))
                })
                .filter(|(t, _)| *t > now)
                .max_by_key(|(t, _)| *t)
        });
        match (held, spent) {
            (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
            (a, b) => a.or(b),
        }
    }

    /// The windows as they stand at `now`: `t` counted down from `seen`, and a window whose
    /// reset has passed dropped (its `r` no longer says anything). Pure.
    pub fn windows_now(&self, now: Instant) -> Vec<StateWindow> {
        let Some(seen) = self.seen else { return Vec::new() };
        let age = now.saturating_duration_since(seen).as_secs();
        self.state
            .iter()
            .filter(|w| w.reset.is_none_or(|t| t > age))
            .map(|w| StateWindow { reset: w.reset.map(|t| t - age), ..w.clone() })
            .collect()
    }

    /// The requests sent in the minute before `now`.
    pub fn last_minute(&self, now: Instant) -> usize {
        self.recent.iter().filter(|t| now.saturating_duration_since(**t) < Duration::from_secs(60)).count()
    }
}

/// How long to wait after a 429: what the server asked (`Retry-After`, else the body's own
/// `retry_after`), else the reset of a window the same answer says is spent, else
/// exponential backoff — `backoff` doubled per earlier 429 in a row (1, 2, 4… s), at most a
/// minute. Pure.
pub fn wait_for_429(
    retry_after: Option<Duration>,
    body_after: Option<Duration>,
    state: &[StateWindow],
    strikes: u32,
    backoff: Duration,
) -> Duration {
    let spent = state.iter().filter(|w| w.remaining == Some(0)).filter_map(|w| w.reset).max().map(Duration::from_secs);
    retry_after
        .or(body_after)
        .or(spent)
        .unwrap_or_else(|| backoff.saturating_mul(2u32.saturating_pow(strikes.min(16))).min(Duration::from_secs(60)))
}

// ---------------------------------------------------------------------------------------
// The process-wide quotas
// ---------------------------------------------------------------------------------------

/// Which quota a request draws on: its API (label) and a fingerprint of its credentials (two
/// keys are two quotas). The fingerprint stays in memory, never printed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct QuotaKey {
    api: String,
    provider: &'static str,
    key_fp: u64,
}

/// The provider and display name of the API `url` belongs to. RunPod's three surfaces are
/// apart (nothing says they share a quota); an API not in the table is its host (and port:
/// each loopback test server is its own quota). Pure.
pub fn api_of(url: &reqwest::Url) -> (&'static str, String) {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let first = url.path_segments().and_then(|mut s| s.next()).unwrap_or("").to_ascii_lowercase();
    match (host.as_str(), first.as_str()) {
        ("api.runpod.io", "graphql") => ("runpod", "RunPod GraphQL".into()),
        ("api.runpod.io", "v2") => ("runpod", "RunPod REST v2".into()),
        ("rest.runpod.io", _) => ("runpod", "RunPod REST v1".into()),
        ("api.runpod.io", _) => ("runpod", "RunPod API".into()),
        ("console.vast.ai", _) => ("vast", "Vast".into()),
        ("api.hetzner.cloud", _) => ("hetzner", "Hetzner Cloud".into()),
        ("api.shadeform.ai", _) => ("shadeform", "Shadeform".into()),
        ("openrouter.ai", _) => ("openrouter", "OpenRouter".into()),
        _ => ("", match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        }),
    }
}

impl QuotaKey {
    /// The quota `req` draws on: [`api_of`] its URL, with its credentials (the
    /// `Authorization` or `X-API-KEY` header) hashed.
    pub(crate) fn of(req: &reqwest::Request) -> Self {
        let (provider, api) = api_of(req.url());
        let mut h = std::hash::DefaultHasher::new();
        for name in ["authorization", "x-api-key"] {
            if let Some(v) = req.headers().get(name) {
                v.as_bytes().hash(&mut h);
            }
        }
        QuotaKey { api, provider, key_fp: h.finish() }
    }

    pub(crate) fn api(&self) -> &str {
        &self.api
    }
}

static QUOTAS: Mutex<Option<HashMap<QuotaKey, Quota>>> = Mutex::new(None);

/// `f` on `key`'s quota (made on first use).
pub(crate) fn with<T>(key: &QuotaKey, f: impl FnOnce(&mut Quota) -> T) -> T {
    let mut guard = QUOTAS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    let q = map
        .entry(key.clone())
        .or_insert_with(|| Quota { provider: key.provider, api: key.api.clone(), ..Default::default() });
    f(q)
}

/// Every quota this process has drawn on, as it stands now — for display.
pub fn observed() -> Vec<QuotaView> {
    let now = Instant::now();
    let guard = QUOTAS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out: Vec<QuotaView> = guard.iter().flat_map(|m| m.values()).map(|q| QuotaView::of(q, now)).collect();
    out.sort_by(|a, b| a.api.cmp(&b.api));
    out
}

/// How long `provider`'s APIs are held for (the longest), with the API and why: what a
/// caller that just got a rate-limit error from that provider should wait before asking
/// again. `None`: none of them is held.
pub fn retry_in(provider: &str) -> Option<Hold> {
    let now = Instant::now();
    let guard = QUOTAS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .iter()
        .flat_map(|m| m.values())
        .filter(|q| q.provider == provider)
        .filter_map(|q| q.block(now).map(|(until, why)| Hold { api: q.api.clone(), wait: until - now, why }))
        .max_by_key(|h| h.wait)
}

/// A quota that is held: which API, for how long from now, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub api: String,
    pub wait: Duration,
    pub why: String,
}

/// One quota as it stands, for display ([`describe`], [`footer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaView {
    pub provider: &'static str,
    pub api: String,
    pub policy: Vec<PolicyWindow>,
    pub raw_policy: Option<String>,
    /// The windows now ([`Quota::windows_now`]).
    pub windows: Vec<StateWindow>,
    pub raw_state: Option<String>,
    /// How old the `RateLimit` reading is.
    pub age: Option<Duration>,
    pub held: Option<(Duration, String)>,
    pub last_minute: usize,
    pub sent: u64,
    pub throttled: u64,
}

impl QuotaView {
    /// `q` at `now`. Pure.
    pub fn of(q: &Quota, now: Instant) -> Self {
        QuotaView {
            provider: q.provider,
            api: q.api.clone(),
            policy: q.policy.clone(),
            raw_policy: q.raw_policy.clone(),
            windows: q.windows_now(now),
            raw_state: q.raw_state.clone(),
            age: q.seen.map(|s| now.saturating_duration_since(s)),
            held: q.block(now).map(|(t, why)| (t - now, why)),
            last_minute: q.last_minute(now),
            sent: q.sent,
            throttled: q.throttled,
        }
    }

    /// The window `name`'s quota from the policy.
    fn quota_of(&self, name: &str) -> Option<u64> {
        self.policy.iter().find(|p| p.name == name).and_then(|p| p.quota)
    }

    /// How far apart this API's requests should be so the tightest window isn't spent before
    /// its reset — spreading half of what's left over the time to the reset, the other half
    /// left to the other processes on the key — with which window. `None` while no window is
    /// tight: at most a quarter of its quota left (10 requests when the quota isn't known).
    /// Pure.
    pub fn pace_gap(&self) -> Option<(Duration, String)> {
        self.windows
            .iter()
            .filter_map(|w| {
                let (r, t) = (w.remaining?, w.reset?);
                let tight = match self.quota_of(&w.name) {
                    Some(q) => r.saturating_mul(4) <= q,
                    None => r <= 10,
                };
                tight.then(|| (Duration::from_secs(t) / u32::try_from((r / 2).max(1)).unwrap_or(u32::MAX), w.name.clone()))
            })
            .max_by_key(|(gap, _)| *gap)
    }
}

/// A count of seconds as people say it: `12 s`, `4 min`, `2 h 5 min`. Pure.
pub fn wait_words(d: Duration) -> String {
    let s = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    match s {
        0..=90 => format!("{s} s"),
        91..=5_399 => format!("{} min", s.div_ceil(60)),
        _ => {
            let m = s.div_ceil(60);
            if m % 60 == 0 { format!("{} h", m / 60) } else { format!("{} h {} min", m / 60, m % 60) }
        }
    }
}

/// `RateLimit-Policy`/`RateLimit` as received, with any `pk` (partition key: it identifies
/// the account) blanked. Pure.
fn shown_header(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        // `;pk=` outside quotes: keep the key, blank its value up to the next `;` or `,`.
        if b[i] == b'"' {
            let start = i;
            string(b, &mut i);
            out.push_str(&raw[start..i]);
            continue;
        }
        if b[i] == b';' {
            let mut j = i + 1;
            skip_ws(b, &mut j);
            let k_start = j;
            let k = key(b, &mut j);
            if k == "pk" && b.get(j) == Some(&b'=') {
                out.push_str(&raw[i..k_start]);
                out.push_str("pk=<redacted>");
                j += 1;
                while j < b.len() && b[j] != b';' && b[j] != b',' {
                    if b[j] == b'"' {
                        string(b, &mut j);
                    } else {
                        j += 1;
                    }
                }
                i = j;
                continue;
            }
        }
        let c = raw[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// One window as a line: `minute: 47 of 60 left, resets in 12 s`. Pure.
fn window_line(w: &StateWindow, quota: Option<u64>) -> String {
    let left = match (w.remaining, quota) {
        (Some(r), Some(q)) => format!("{r} of {q} left"),
        (Some(r), None) => format!("{r} left"),
        (None, Some(q)) => format!("quota {q}"),
        (None, None) => "no count".into(),
    };
    match w.reset {
        Some(t) => format!("{}: {left}, resets in {}", w.name, wait_words(Duration::from_secs(t))),
        None => format!("{}: {left}", w.name),
    }
}

/// What `arena api get … --limits` prints about one quota: the headers as received (any
/// partition key blanked), each window as it stands now, and this process's requests. No
/// secrets: the headers carry counts only, and the key fingerprint is never shown. Pure.
pub fn describe(v: &QuotaView) -> Vec<String> {
    let mut out = vec![format!("{}:", v.api)];
    if v.raw_policy.is_none() && v.raw_state.is_none() {
        out.push("  no RateLimit headers in its answer: no limit reported".into());
    }
    if let Some(p) = &v.raw_policy {
        out.push(format!("  RateLimit-Policy: {}", shown_header(p)));
    }
    if let Some(s) = &v.raw_state {
        out.push(format!("  RateLimit: {}", shown_header(s)));
    }
    // Each window the policy names, then any only the state names.
    let mut names: Vec<&str> = v.policy.iter().map(|p| p.name.as_str()).collect();
    for w in &v.windows {
        if !names.contains(&w.name.as_str()) {
            names.push(&w.name);
        }
    }
    for name in names {
        let quota = v.quota_of(name);
        let window = v.policy.iter().find(|p| p.name == name).and_then(|p| p.window);
        let per = window.map(|w| format!(" per {}", wait_words(Duration::from_secs(w)))).unwrap_or_default();
        match v.windows.iter().find(|w| w.name == name) {
            Some(w) => out.push(format!("  {}", window_line(w, quota))),
            None => match quota {
                Some(q) => out.push(format!("  {name}: quota {q}{per} (no count since its reset)")),
                None => out.push(format!("  {name}: no count")),
            },
        }
    }
    if let Some((wait, why)) = &v.held {
        out.push(format!("  held: {why}; next request in {}", wait_words(*wait)));
    }
    let throttled = if v.throttled > 0 { format!(", {} answered 429", v.throttled) } else { String::new() };
    out.push(format!("  this process: {} request(s) sent{throttled}", v.sent));
    out
}

/// The dashboard footer's part about one provider's quotas: its requests in the last minute
/// (summed over its APIs) and, when an API reports one, the tightest minute window
/// (`RunPod API 13/min · 47/60 left this minute`). `None` when the provider sent nothing in
/// the last minute and reports nothing. Pure.
pub fn footer(views: &[QuotaView], provider: &str, name: &str) -> Option<String> {
    let mine: Vec<&QuotaView> = views.iter().filter(|v| v.provider == provider).collect();
    let rate: usize = mine.iter().map(|v| v.last_minute).sum();
    let minute = mine
        .iter()
        .filter_map(|v| {
            let w = v.windows.iter().find(|w| w.name == "minute")?;
            Some((w.remaining?, v.quota_of("minute")))
        })
        .min_by_key(|(r, _)| *r);
    if rate == 0 && minute.is_none() {
        return None;
    }
    let left = match minute {
        Some((r, Some(q))) => format!(" · {r}/{q} left this minute"),
        Some((r, None)) => format!(" · {r} left this minute"),
        None => String::new(),
    };
    Some(format!("{name} API {rate}/min{left}"))
}

// ---------------------------------------------------------------------------------------
// The caller's wait budget
// ---------------------------------------------------------------------------------------

tokio::task_local! {
    /// The latest instant a rate-limit wait may end, for the requests of this task.
    static WAIT_UNTIL: Instant;
}

/// Run `f` with its rate-limit waits bounded: a request that would have to wait past
/// `budget` from now fails at once as rate-limited (saying how long), instead of sleeping
/// past what the caller can afford — a dashboard's provider call has seconds, not the
/// minute a window takes to reset. Nested calls keep the earlier bound. Bounds only the
/// *waits*: the requests themselves are as long as they take (the caller's own timeout, if
/// any, bounds those).
pub async fn waits_within<F: Future>(budget: Duration, f: F) -> F::Output {
    waits_until(Instant::now() + budget, f).await
}

/// [`waits_within`] with the bound as an instant.
pub async fn waits_until<F: Future>(until: Instant, f: F) -> F::Output {
    let until = WAIT_UNTIL.try_with(|u| (*u).min(until)).unwrap_or(until);
    WAIT_UNTIL.scope(until, f).await
}

/// `f` with this task's wait bound, if it has one: for a future that is spawned (a spawned
/// task doesn't inherit task-locals).
pub fn inherit<F: Future>(f: F) -> impl Future<Output = F::Output> {
    let until = WAIT_UNTIL.try_with(|u| *u).ok();
    async move {
        match until {
            Some(u) => WAIT_UNTIL.scope(u, f).await,
            None => f.await,
        }
    }
}

/// How long a wait may be from `now`: `max`, or less when the caller's bound is sooner.
pub(crate) fn budget(max: Duration, now: Instant) -> Duration {
    match WAIT_UNTIL.try_with(|u| *u) {
        Ok(until) => until.saturating_duration_since(now).min(max),
        Err(_) => max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(name: &str, q: Option<u64>, w: Option<u64>) -> PolicyWindow {
        PolicyWindow { name: name.into(), quota: q, window: w }
    }

    fn sw(name: &str, r: Option<u64>, t: Option<u64>) -> StateWindow {
        StateWindow { name: name.into(), remaining: r, reset: t }
    }

    /// RunPod's documented headers, and the ways a server can say the same thing: tokens
    /// for names, other spacing, extra parameters, unknown windows, decimals — and what is
    /// skipped (a member without a name, garbage), never failing the rest.
    #[test]
    fn policy_header_table() {
        let cases: Vec<(&str, Vec<PolicyWindow>)> = vec![
            (
                r#""minute";q=60;w=60, "hour";q=3000;w=3600, "day";q=50000;w=86400"#,
                vec![pw("minute", Some(60), Some(60)), pw("hour", Some(3000), Some(3600)), pw("day", Some(50000), Some(86400))],
            ),
            (r#"minute;q=180;w=60"#, vec![pw("minute", Some(180), Some(60))]),
            (r#""minute" ; q=60 ; w=60 ,"hour";w=3600;q=3000"#, vec![pw("minute", Some(60), Some(60)), pw("hour", Some(3000), Some(3600))]),
            (r#""burst";q=10;w=1;qu="requests";pk=:cHJvamVjdDEyMw==:, "minute";q=60;w=60"#, vec![pw("burst", Some(10), Some(1)), pw("minute", Some(60), Some(60))]),
            (r#""minute";Q=60;W=60"#, vec![pw("minute", Some(60), Some(60))]),
            (r#""minute";q=59.9;w=0.5"#, vec![pw("minute", Some(59), Some(1))]),
            (r#""minute";q=-5;w=60"#, vec![pw("minute", None, Some(60))]),
            (r#""minute""#, vec![pw("minute", None, None)]),
            (r#"60;w=60, "hour";q=3000;w=3600"#, vec![pw("hour", Some(3000), Some(3600))]),
            (r#"(a b);q=1, "day";q=5"#, vec![pw("day", Some(5), None)]),
            (r#""a,b";q=1, "c\"d";q=2"#, vec![pw("a,b", Some(1), None), pw("c\"d", Some(2), None)]),
            (r#"?1;q=2, @1700000000, "minute";q=3"#, vec![pw("minute", Some(3), None)]),
            (r#""minute";q=60;w=60 garbage, "hour";q=1"#, vec![pw("minute", Some(60), Some(60)), pw("hour", Some(1), None)]),
            (r#""minute";q=99999999999999999999999"#, vec![pw("minute", None, None)]),
            (r#""minute";;q=1, "hour";q=2"#, vec![pw("minute", None, None), pw("hour", Some(2), None)]),
            ("", vec![]),
            (" , ,", vec![]),
            ("limit=10, remaining=5", vec![pw("limit", None, None), pw("remaining", None, None)]),
        ];
        for (header, want) in cases {
            assert_eq!(parse_policy(header), want, "{header}");
        }
    }

    #[test]
    fn state_header_table() {
        let cases: Vec<(&str, Vec<StateWindow>)> = vec![
            (
                r#""minute";r=0;t=12, "hour";r=2800;t=1812, "day";r=49500;t=45012"#,
                vec![sw("minute", Some(0), Some(12)), sw("hour", Some(2800), Some(1812)), sw("day", Some(49500), Some(45012))],
            ),
            (r#"minute;r=5;t=30"#, vec![sw("minute", Some(5), Some(30))]),
            (r#""minute";t=30;r=5;x=1"#, vec![sw("minute", Some(5), Some(30))]),
            // r rounds down (never more left than said), t up (never a reset sooner).
            (r#""minute";r=4.7;t=11.2"#, vec![sw("minute", Some(4), Some(12))]),
            (r#""minute";r=5"#, vec![sw("minute", Some(5), None)]),
            (r#""minute";r="5";t=x"#, vec![sw("minute", None, None)]),
            (r#""weird window";r=1;t=2"#, vec![sw("weird window", Some(1), Some(2))]),
            ("nonsense", vec![sw("nonsense", None, None)]),
            ("", vec![]),
        ];
        for (header, want) in cases {
            assert_eq!(parse_state(header), want, "{header}");
        }
    }

    #[test]
    fn retry_after_table() {
        let s = Duration::from_secs;
        let cases = [
            ("12", Some(s(12))),
            (" 0 ", Some(Duration::ZERO)),
            ("1.5", Some(Duration::from_millis(1_500))),
            ("86400", Some(s(86_400))),
            ("86401", None),
            ("-1", None),
            ("NaN", None),
            ("inf", None),
            ("Wed, 21 Oct 2026 07:28:00 GMT", None),
            ("", None),
        ];
        for (v, want) in cases {
            assert_eq!(parse_retry_after(v), want, "{v:?}");
        }
        assert_eq!(body_retry_after(r#"{"detail":"too frequent","retry_after":12}"#), Some(s(12)));
        assert_eq!(body_retry_after(r#"{"retry_after":"3"}"#), Some(s(3)));
        assert_eq!(body_retry_after(r#"{"retry_after":-3}"#), None);
        assert_eq!(body_retry_after("slow down"), None);
    }

    /// The server's word first, then a window this answer says is spent, then doubling.
    #[test]
    fn wait_for_429_table() {
        let s = Duration::from_secs;
        let spent = [sw("minute", Some(0), Some(12)), sw("hour", Some(5), Some(1800))];
        let one = s(1);
        let cases = [
            (Some(s(7)), Some(s(3)), &spent[..], 0, s(7)),
            (None, Some(s(3)), &spent[..], 0, s(3)),
            (None, None, &spent[..], 0, s(12)),
            (None, None, &spent[1..], 0, s(1)), // nothing spent: backoff
            (None, None, &[][..], 0, s(1)),
            (None, None, &[][..], 1, s(2)),
            (None, None, &[][..], 2, s(4)),
            (None, None, &[][..], 10, s(60)),
            (None, None, &[][..], u32::MAX, s(60)),
        ];
        for (ra, body, state, strikes, want) in cases {
            assert_eq!(wait_for_429(ra, body, state, strikes, one), want, "{ra:?} {body:?} {state:?} {strikes}");
        }
    }

    /// A window with r ≤ 1 holds every request until its reset; a 429's hold the same; the
    /// later one wins; neither once passed.
    #[test]
    fn block_table() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut q = Quota::default();
        assert_eq!(q.block(t0), None, "nothing known: nothing held");
        q.record(None, Some(r#""minute";r=2;t=12, "hour";r=900;t=1800"#), t0);
        assert_eq!(q.block(t0), None, "2 left");
        q.record(None, Some(r#""minute";r=1;t=12"#), t0);
        assert_eq!(q.block(t0), Some((t0 + s(12), "1 request left in the minute window".into())));
        q.record(None, Some(r#""minute";r=0;t=12, "hour";r=0;t=1800"#), t0);
        assert_eq!(q.block(t0 + s(1)), Some((t0 + s(1800), "the hour window is spent".into())));
        q.record(None, Some(r#""minute";r=0;t=12"#), t0);
        assert_eq!(q.block(t0 + s(11)).map(|b| b.0), Some(t0 + s(12)));
        assert_eq!(q.block(t0 + s(12)), None, "reset: free again");
        // A 429's hold, longer than the window's.
        q.note_throttled(s(20), "HTTP 429".into(), t0);
        assert_eq!(q.block(t0 + s(13)), Some((t0 + s(20), "HTTP 429".into())));
        // A shorter hold from a later 429 doesn't shorten it.
        q.note_throttled(s(1), "HTTP 429".into(), t0 + s(1));
        assert_eq!(q.block(t0 + s(5)).map(|b| b.0), Some(t0 + s(20)));
        assert_eq!(q.block(t0 + s(20)), None);
        assert_eq!((q.strikes, q.throttled), (2, 2));
        q.note_answered();
        assert_eq!(q.strikes, 0);
        // Unreadable windows hold nothing.
        let mut q = Quota::default();
        q.record(None, Some(r#""minute";r=0, "hour";t=5"#), t0);
        assert_eq!(q.block(t0), None, "no reset to wait for, or no count");
    }

    /// Within one window instance `r` only goes down: an answer read late can't raise it.
    #[test]
    fn readings_merge_within_a_window() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut q = Quota::default();
        q.record(Some(r#""minute";q=60;w=60"#), Some(r#""minute";r=3;t=30"#), t0);
        q.record(None, Some(r#""minute";r=7;t=29"#), t0 + s(1));
        assert_eq!(q.state, [sw("minute", Some(3), Some(29))], "same reset: the lower count stays");
        q.record(None, Some(r#""minute";r=59;t=60"#), t0 + s(31));
        assert_eq!(q.state, [sw("minute", Some(59), Some(60))], "a new window: the new count");
        assert_eq!(q.policy, [pw("minute", Some(60), Some(60))], "the policy kept when absent");
        q.record(None, None, t0 + s(32));
        assert_eq!(q.state, [sw("minute", Some(59), Some(60))], "no header: nothing changes");
        // The windows as they stand later: t counted down, a reset window gone.
        assert_eq!(q.windows_now(t0 + s(41)), [sw("minute", Some(59), Some(50))]);
        assert_eq!(q.windows_now(t0 + s(91)), []);
    }

    #[test]
    fn requests_of_the_last_minute_are_counted() {
        let t0 = Instant::now();
        let mut q = Quota::default();
        for i in 0..5 {
            q.note_sent(t0 + Duration::from_secs(i * 20));
        }
        assert_eq!(q.sent, 5);
        assert_eq!(q.last_minute(t0 + Duration::from_secs(80)), 3);
        assert_eq!(q.recent.len(), 3, "older ones dropped");
    }

    #[test]
    fn apis_are_told_apart() {
        let cases = [
            ("https://api.runpod.io/v2/pods?limit=1000", "runpod", "RunPod REST v2"),
            ("https://api.runpod.io/graphql", "runpod", "RunPod GraphQL"),
            ("https://rest.runpod.io/v1/pods", "runpod", "RunPod REST v1"),
            ("https://console.vast.ai/api/v0/instances/", "vast", "Vast"),
            ("https://api.hetzner.cloud/v1/servers", "hetzner", "Hetzner Cloud"),
            ("http://127.0.0.1:4567/pods", "", "127.0.0.1:4567"),
        ];
        for (url, provider, api) in cases {
            assert_eq!(api_of(&reqwest::Url::parse(url).unwrap()), (provider, api.to_string()), "{url}");
        }
        // Two keys on one API: two quotas.
        let c = reqwest::Client::new();
        let a = c.get("https://api.runpod.io/v2/pods").bearer_auth("k1").build().unwrap();
        let b = c.get("https://api.runpod.io/v2/pods/x").bearer_auth("k2").build().unwrap();
        let a2 = c.get("https://api.runpod.io/v2/catalog/gpus").bearer_auth("k1").build().unwrap();
        assert_ne!(QuotaKey::of(&a), QuotaKey::of(&b));
        assert_eq!(QuotaKey::of(&a), QuotaKey::of(&a2));
        assert!(!format!("{:?}", QuotaKey::of(&a)).contains("k1"));
    }

    #[test]
    fn pace_gap_spreads_half_of_what_is_left() {
        let view = |policy: &str, state: &str| {
            let mut q = Quota::default();
            q.record(Some(policy).filter(|p| !p.is_empty()), Some(state), Instant::now());
            QuotaView::of(&q, Instant::now())
        };
        let s = Duration::from_secs;
        let cases = [
            (r#""minute";q=60;w=60"#, r#""minute";r=40;t=30"#, None),
            (r#""minute";q=60;w=60"#, r#""minute";r=15;t=30"#, Some((s(30) / 7, "minute"))),
            (r#""minute";q=60;w=60"#, r#""minute";r=1;t=30"#, Some((s(30), "minute"))),
            (r#""minute";q=60;w=60"#, r#""minute";r=0;t=30"#, Some((s(30), "minute"))),
            ("", r#""minute";r=11;t=30"#, None),
            ("", r#""minute";r=10;t=30"#, Some((s(6), "minute"))),
            // The tightest window wins: the hour has 700 left of 3000 for 30 min.
            (r#""minute";q=60;w=60, "hour";q=3000;w=3600"#, r#""minute";r=50;t=30, "hour";r=700;t=1800"#, Some((s(1800) / 350, "hour"))),
        ];
        for (policy, state, want) in cases {
            let got = view(policy, state).pace_gap();
            assert_eq!(got, want.map(|(d, n)| (d, n.to_string())), "{policy} / {state}");
        }
    }

    #[test]
    fn wait_words_table() {
        let s = Duration::from_secs;
        for (d, want) in [
            (s(0), "0 s"),
            (Duration::from_millis(200), "1 s"),
            (s(12), "12 s"),
            (s(90), "90 s"),
            (s(91), "2 min"),
            (s(1812), "31 min"),
            (s(3600), "60 min"),
            (s(45012), "12 h 31 min"),
            (s(7200), "2 h"),
        ] {
            assert_eq!(wait_words(d), want, "{d:?}");
        }
    }

    /// What `api get --limits` prints: the headers as received, a partition key blanked;
    /// each window now; the process's own count. Nothing else.
    #[test]
    fn describe_shows_the_headers_and_windows() {
        let t0 = Instant::now();
        let mut q = Quota { api: "RunPod REST v2".into(), provider: "runpod", ..Default::default() };
        q.record(
            Some(r#""minute";q=60;w=60;pk=:c2VjcmV0:, "hour";q=3000;w=3600, "day";q=50000;w=86400"#),
            Some(r#""minute";r=47;t=12, "hour";r=2800;t=1812"#),
            t0,
        );
        q.note_sent(t0);
        let lines = describe(&QuotaView::of(&q, t0));
        assert_eq!(
            lines,
            [
                "RunPod REST v2:",
                r#"  RateLimit-Policy: "minute";q=60;w=60;pk=<redacted>, "hour";q=3000;w=3600, "day";q=50000;w=86400"#,
                r#"  RateLimit: "minute";r=47;t=12, "hour";r=2800;t=1812"#,
                "  minute: 47 of 60 left, resets in 12 s",
                "  hour: 2800 of 3000 left, resets in 31 min",
                "  day: quota 50000 per 24 h (no count since its reset)",
                "  this process: 1 request(s) sent",
            ]
        );
        assert!(!lines.join("\n").contains("c2VjcmV0"));
        let none = Quota { api: "Vast".into(), sent: 2, ..Default::default() };
        assert_eq!(describe(&QuotaView::of(&none, t0)), ["Vast:", "  no RateLimit headers in its answer: no limit reported", "  this process: 2 request(s) sent"]);
        // Held: said with the wait.
        let mut held = q.clone();
        held.note_throttled(Duration::from_secs(12), "rate limit exceeded for the minute window".into(), t0);
        let lines = describe(&QuotaView::of(&held, t0));
        assert!(lines.contains(&"  held: rate limit exceeded for the minute window; next request in 12 s".to_string()), "{lines:?}");
        assert!(lines.last().unwrap().ends_with("1 request(s) sent, 1 answered 429"), "{lines:?}");
    }

    #[test]
    fn shown_header_blanks_only_the_partition_key() {
        for (raw, want) in [
            (r#""minute";q=60;pk=:YWJj:;w=60"#, r#""minute";q=60;pk=<redacted>;w=60"#),
            (r#""a;pk=x";q=1"#, r#""a;pk=x";q=1"#),
            (r#""m";pk="tok";q=1, "h";q=2"#, r#""m";pk=<redacted>;q=1, "h";q=2"#),
            ("plain", "plain"),
        ] {
            assert_eq!(shown_header(raw), want, "{raw}");
        }
    }

    #[test]
    fn footer_table() {
        let t0 = Instant::now();
        let mut v2 = Quota { api: "RunPod REST v2".into(), provider: "runpod", ..Default::default() };
        v2.record(Some(r#""minute";q=60;w=60"#), Some(r#""minute";r=47;t=12"#), t0);
        for _ in 0..12 {
            v2.note_sent(t0);
        }
        let mut gql = Quota { api: "RunPod GraphQL".into(), provider: "runpod", ..Default::default() };
        gql.note_sent(t0);
        let views = [QuotaView::of(&v2, t0), QuotaView::of(&gql, t0)];
        assert_eq!(footer(&views, "runpod", "RunPod").as_deref(), Some("RunPod API 13/min · 47/60 left this minute"));
        assert_eq!(footer(&views, "vast", "Vast"), None);
        assert_eq!(footer(&views[1..], "runpod", "RunPod").as_deref(), Some("RunPod API 1/min"));
    }

    #[tokio::test]
    async fn wait_budgets_nest_to_the_earlier() {
        let max = Duration::from_secs(65);
        assert_eq!(budget(max, Instant::now()), max, "no bound: the most a wait may be");
        let got = waits_within(Duration::from_secs(2), async move {
            let inner = waits_within(Duration::from_secs(60), async move { budget(max, Instant::now()) }).await;
            let spawned = tokio::spawn(inherit(async move { budget(max, Instant::now()) })).await.unwrap();
            let bare = tokio::spawn(async move { budget(max, Instant::now()) }).await.unwrap();
            (inner, spawned, bare)
        })
        .await;
        assert!(got.0 <= Duration::from_secs(2), "{got:?}");
        assert!(got.1 <= Duration::from_secs(2), "a spawned future inherits it: {got:?}");
        assert_eq!(got.2, max, "unless it isn't passed on");
    }
}
