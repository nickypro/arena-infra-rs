//! Money left on the prepaid provider accounts, how long it lasts — and a fallback chain
//! that keeps saying so, conservatively, after the live balance API goes away.
//!
//! RunPod and Vast are **prepaid**. At $0 RunPod *terminates* pods without a network volume
//! ("their data can't be recovered", RunPod's billing docs) and Vast stops every instance:
//! mid-cohort, the outage nobody sees coming. So `arena balance`, the `pods list` footer,
//! the `create`/`up` preflight, `teardown --check`, the TUI summary bar and `config check`
//! say how much is left and roughly when it runs out. Hetzner bills monthly in arrears:
//! postpaid, €/h only, never a runway or a warning.
//!
//! **Where a balance comes from** — the first tier that applies wins:
//! 1. **live**: RunPod GraphQL `myself { clientBalance currentSpendPerHr spendLimit
//!    underBalance }` (a field RunPod drops costs that field, not the balance: the query is
//!    retried with `clientBalance` alone) or Vast `GET /users/current/` → `credit` — read
//!    this run, or less than [`FRESH`] ago.
//! 2. **est.**: the last live reading (or a manual `arena balance set`) — the *anchor* —
//!    minus what has been spent since: `max(billed, metered)`. *Billed* is RunPod's REST v2
//!    hourly billing history (every resource on the account, but it lags); *metered* is
//!    arena's own step integral over the burn samples it recorded. RunPod's GraphQL is
//!    reportedly retired in early 2027 and v2 has billing *history* only; this tier keeps the
//!    surfaces honest after that, for up to [`ANCHOR_MAX_AGE`].
//! 3. **?**: nothing usable — said as `?` next to the burn, with a nudge to `arena balance set`.
//!
//! The estimate errs **low** on purpose: the anchor's first billed hour is counted whole,
//! each metered segment is charged at its higher end, billing lag is covered by metering,
//! spend arena doesn't list (volumes, stopped disks) by billing, and top-ups are invisible.
//! A wrong estimate therefore warns early, never late — and a later observation never raises
//! it (only a live reading or `set` can). Its one known undercount is a pod created *and*
//! destroyed between two observations while billing is unavailable (always the case on
//! Vast), bounded by that pod's lifetime × price.
//!
//! **Quiet failures.** A broken source is a label (`live n/a 20m`) on inline surfaces and
//! one note in `arena balance` / a ⚠ row in `config check` — never stderr noise in `pods
//! list`, the TUI, `create`/`up` or teardown, and never a changed exit code outside
//! `balance --check`. After each live read `arena balance` also runs the **fallback check**:
//! the estimate from an older live anchor against the live truth, so a fallback that
//! overstates is noticed while GraphQL still works.
//!
//! **Unknown is never zero.** A provider that failed to list has an unknown burn (`?`),
//! never $0/h; a missing balance is `?`, never $0.00; neither ever reads as "no burn".
//!
//! **Secrets.** Only the balance fields are requested or read. Vast's answer also holds the
//! account's email and API key: it is never quoted, logged or stored — a failure keeps a
//! kind, an HTTP status and our own words (or, from RunPod, a GraphQL error message, which
//! describes our query). The state file (`balance.json`, 0600, per fleet prefix, next to
//! `health.json`) holds amounts, times, a 32-bit key fingerprint and failure kinds.
//!
//! Layout as elsewhere: the formulas, the state transition ([`apply`]), the views and the
//! renderers are pure and table-tested; the I/O (the state file under a lock, the
//! [`BalanceApi`] seam and its HTTP implementation, [`observe`]) comes last.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::fleet::fmt_money;
use crate::pod::Pod;
use crate::provider::runpod::loose_f64;
use crate::proxy::{Listing, ListingStatus};
use crate::snapshot::{fmt_age, rfc3339};
use crate::status::bills_hourly;
use crate::table::{self, Align};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// A live reading younger than this is reused rather than asked for again: inline surfaces
/// (the `pods list` footer, the TUI every minute) make at most one live call per account per
/// `FRESH` seconds, and a reading this young is still shown as **live**.
pub const FRESH: u64 = 300;

/// The whole observation budget of an inline surface (live + billing), run alongside its
/// own listing: a stalled balance API costs a footer at most this.
pub const INLINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `arena balance`'s observation budget.
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// An anchor older than this is too stale to estimate from — untracked top-ups and spend
/// add up — so the balance becomes `?` (seconds).
pub const ANCHOR_MAX_AGE: u64 = 14 * 86_400;

/// How far in the future a reading may be (clock skew between machines, a clock that
/// stepped back) before it is distrusted (seconds).
pub const CLOCK_SKEW: u64 = 300;

/// Runway below this many hours is critical (unless `BALANCE_WARN_HOURS=0`).
pub const CRITICAL_HOURS: f64 = 24.0;

/// `BALANCE_WARN_HOURS` when unset: a Friday-evening glance still covers the weekend.
pub const DEFAULT_WARN_HOURS: f64 = 72.0;

/// A transient live failure (timeout, 429, 5xx) is only a label until it has lasted this
/// long with no success in between; then it is noted like any other (seconds).
pub const TRANSIENT_NOTE_AFTER: u64 = 6 * 3_600;

/// A burn sample is recorded at least this often, even when the burn hasn't changed.
pub const SAMPLE_HEARTBEAT: u64 = 3_600;

/// Samples older than this are dropped — except the newest of them, kept as the baseline
/// an anchor inside the window may need (seconds).
pub const SAMPLE_KEEP: u64 = 15 * 86_400;

/// At most this many samples per account.
pub const SAMPLE_MAX: usize = 2_000;

/// The fallback check needs a check anchor at least this old: a shorter horizon proves
/// nothing (seconds).
pub const CHECK_MIN: u64 = 30 * 60;

/// The check anchor is replaced by the newest live reading once it is this old, so the
/// check horizon cycles between [`CHECK_MIN`] and about a day (seconds).
pub const CHECK_ROTATE: u64 = 24 * 3_600;

/// A burn below this (USD/h) is "no burn": there is no runway to speak of.
pub const NO_BURN: f64 = 0.005;

/// A balance outside this range is a broken reading (or a typo in `balance set`), not money.
pub const AMOUNT_RANGE: std::ops::RangeInclusive<f64> = -1e5..=1e7;

/// `balance.json`'s format version: a file with another is ignored (and replaced) rather
/// than misread.
pub const STATE_VERSION: u32 = 1;

/// Longest GraphQL error message kept as a failure's detail.
const DETAIL_MAX: usize = 120;

/// A burn change smaller than this (USD/h) is float noise — the same pods summed in another
/// order — not a change point.
const BURN_NOISE: f64 = 1e-9;

// ---------------------------------------------------------------------------------------
// Accounts and keys
// ---------------------------------------------------------------------------------------

/// A prepaid provider account. Hetzner (postpaid) has no balance and is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Account {
    Runpod,
    Vast,
}

impl Account {
    pub const ALL: [Account; 2] = [Account::Runpod, Account::Vast];

    /// The provider's name (`runpod`, `vast`), which is also how the account reads.
    pub fn name(self) -> &'static str {
        match self {
            Account::Runpod => "runpod",
            Account::Vast => "vast",
        }
    }

    /// `RunPod`, `Vast` — for sentences.
    pub fn title(self) -> &'static str {
        match self {
            Account::Runpod => "RunPod",
            Account::Vast => "Vast",
        }
    }

    /// The config key holding its API key.
    pub fn key_name(self) -> &'static str {
        match self {
            Account::Runpod => "RUNPOD_API_KEY",
            Account::Vast => "VAST_API_KEY",
        }
    }

    pub fn from_name(s: &str) -> Option<Account> {
        Account::ALL.into_iter().find(|a| a.name().eq_ignore_ascii_case(s.trim()))
    }

    /// What happens at $0 (the providers' own docs), said on a critical balance.
    fn at_zero(self) -> &'static str {
        match self {
            Account::Runpod => "at $0 RunPod terminates pods without a network volume (data lost)",
            Account::Vast => "at $0 Vast stops every instance",
        }
    }
}

/// A key's fingerprint: FNV-1a 32-bit as 8 hex characters. Stored next to each account's
/// readings so a swapped key (another account) never inherits the old account's anchor —
/// and 32 bits can't be turned back into a key. No crate for eight lines.
pub fn key_fp(key: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// The prepaid accounts this config has a (non-empty) key for, with their fingerprints.
/// Pure: no request is made, so cache-only surfaces (`snapshot`, `config check`) use it.
pub fn configured(cfg: &Config) -> Vec<(Account, String)> {
    Account::ALL
        .into_iter()
        .filter_map(|a| cfg.get(a.key_name()).map(str::trim).filter(|k| !k.is_empty()).map(|k| (a, key_fp(k))))
        .collect()
}

/// `balance set`'s amount: a finite number of USD inside [`AMOUNT_RANGE`] (a negative
/// balance is real — an account can be overdrawn — but `1e9` is a typo).
pub fn parse_amount(s: &str) -> Result<f64, String> {
    let v: f64 = s.trim().trim_start_matches('$').parse().map_err(|_| format!("`{s}` is not an amount in USD (e.g. 250 or 31.20)"))?;
    if v.is_finite() && AMOUNT_RANGE.contains(&v) {
        Ok(v)
    } else {
        Err(format!("the amount must be a number from {} to {} (got {s})", AMOUNT_RANGE.start(), AMOUNT_RANGE.end()))
    }
}

// ---------------------------------------------------------------------------------------
// Readings and failures
// ---------------------------------------------------------------------------------------

/// One live balance reading.
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub usd: f64,
    /// The provider's own spend rate (RunPod `currentSpendPerHr`): everything on the
    /// account, including what arena doesn't list. `None` = not reported.
    pub provider_burn_per_hr: Option<f64>,
    /// RunPod's account spend limit (`spendLimit`, default $80/h).
    pub spend_limit_per_hr: Option<f64>,
    /// RunPod's `underBalance`: below what the account needs to keep running.
    pub under_balance: Option<bool>,
    /// The full query was rejected over a dropped field and the minimal one was needed.
    pub reduced: bool,
}

/// Why a read failed, in four kinds that decide how loudly it is said (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailKind {
    /// A timeout, a transport error, 429 or 5xx: likely to pass.
    Transient,
    /// 401/403 — while the same key still lists pods: its balance access went.
    Auth,
    /// 404/405/410, or a 2xx that isn't JSON: the endpoint is gone.
    Gone,
    /// The answer's shape changed: a missing or non-numeric field, a GraphQL error, any
    /// other 4xx.
    Drift,
}

impl FailKind {
    pub fn label(self) -> &'static str {
        match self {
            FailKind::Transient => "transient",
            FailKind::Auth => "auth",
            FailKind::Gone => "gone",
            FailKind::Drift => "drift",
        }
    }
}

/// A failed read. `detail` is our own fixed words, or a GraphQL `errors[].message` clipped
/// to [`DETAIL_MAX`] — **never** any other body text (Vast's carries the account's API key).
#[derive(Clone, Debug, PartialEq)]
pub struct Fail {
    pub kind: FailKind,
    pub http: Option<u16>,
    pub detail: Option<String>,
}

impl Fail {
    fn new(kind: FailKind, http: Option<u16>, detail: impl Into<String>) -> Self {
        Fail { kind, http, detail: Some(detail.into()) }
    }

    /// A transient failure in our own words (a timeout, a dropped connection).
    pub fn transient(detail: impl Into<String>) -> Self {
        Fail::new(FailKind::Transient, None, detail)
    }
}

/// RunPod's hourly billing history over a window: `(bucket start, USD)` per bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct Billed {
    pub buckets: Vec<(u64, f64)>,
    /// The window RunPod resolved (start snapped down to the hour, end up).
    pub from: u64,
    pub to: u64,
}

/// `t` rounded down to the hour.
pub fn floor_hour(t: u64) -> u64 {
    t - t % 3_600
}

/// What the billing history says was spent since `t`: every bucket from `t`'s hour on — so
/// that first hour counts whole (spend just before `t` included: errs low). `None` when the
/// window starts after that hour (it can't say).
pub fn billed_since(b: &Billed, t: u64) -> Option<f64> {
    let h = floor_hour(t);
    if b.from > h {
        return None;
    }
    Some(b.buckets.iter().filter(|(s, _)| *s >= h).map(|(_, usd)| usd).sum())
}

/// `s` cut to `n` characters, with `…` when cut.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The `errors[].message`s of a GraphQL answer, joined and clipped — and nothing else of
/// the body, however it is shaped.
fn graphql_messages(v: &Value) -> Option<String> {
    let msgs: Vec<&str> =
        v.get("errors")?.as_array()?.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect();
    (!msgs.is_empty()).then(|| clip(&msgs.join("; "), DETAIL_MAX))
}

/// One response, status first: a 2xx JSON body, or the [`Fail`] it is. 401/403 → auth;
/// 404/405/410 or a 2xx that isn't JSON (an HTML page) → gone; 408/429/5xx → transient;
/// any other status → drift. The detail is a GraphQL error message at most — never the
/// body itself.
pub fn classify(status: u16, text: &str) -> Result<Value, Fail> {
    let json: Option<Value> = serde_json::from_str(text).ok();
    let said = json.as_ref().and_then(graphql_messages);
    let http = Some(status);
    let fail = |kind| Err(Fail { kind, http, detail: said.clone() });
    match status {
        200..=299 => match json {
            Some(v) => Ok(v),
            None if text.trim().is_empty() => Err(Fail::new(FailKind::Drift, http, "an empty answer")),
            None => Err(Fail::new(FailKind::Gone, http, "the answer isn't JSON (an HTML page?)")),
        },
        401 | 403 => fail(FailKind::Auth),
        404 | 405 | 410 => fail(FailKind::Gone),
        408 | 429 | 500..=599 => fail(FailKind::Transient),
        _ => fail(FailKind::Drift),
    }
}

/// Whether a failed full balance query is RunPod rejecting a field it dropped — then the
/// minimal query (`clientBalance` alone) still gets the balance.
pub fn wants_minimal_retry(f: &Fail) -> bool {
    f.kind == FailKind::Drift && f.detail.as_deref().is_some_and(|d| d.contains("Cannot query field"))
}

fn in_range(v: f64) -> bool {
    v.is_finite() && AMOUNT_RANGE.contains(&v)
}

/// A finite, non-negative, sane rate (a negative or NaN "rate" is noise, not a credit).
fn rate(v: Option<&Value>) -> Option<f64> {
    v.and_then(loose_f64).filter(|r| r.is_finite() && (0.0..=1e5).contains(r))
}

/// RunPod's `{ data: { myself: { … } } }`. Fails closed: without an in-range numeric
/// `clientBalance` there is no reading (drift; a GraphQL error message says why). With one,
/// errors about the other fields leave just those fields unknown.
pub fn parse_runpod_myself(v: &Value) -> Result<Live, Fail> {
    let said = graphql_messages(v);
    let drift = |why: &str| Fail { kind: FailKind::Drift, http: None, detail: Some(said.clone().unwrap_or_else(|| why.into())) };
    let me = match v.pointer("/data/myself") {
        Some(m) if m.is_object() => m,
        Some(Value::Null) => return Err(drift("data.myself is null")),
        _ => return Err(drift("no data.myself in the answer")),
    };
    let usd = match me.get("clientBalance").and_then(loose_f64) {
        Some(b) if in_range(b) => b,
        Some(_) => return Err(Fail::new(FailKind::Drift, None, "clientBalance is out of range")),
        None => return Err(drift("clientBalance is missing or not a number")),
    };
    Ok(Live {
        usd,
        provider_burn_per_hr: rate(me.get("currentSpendPerHr")),
        spend_limit_per_hr: rate(me.get("spendLimit")).filter(|l| *l > 0.0),
        under_balance: me.get("underBalance").and_then(Value::as_bool),
        reduced: false,
    })
}

/// Why a Vast answer gave no balance — in our words: the body holds the email and API key.
const VAST_NO_CREDIT: &str = "no numeric `credit` in /users/current/ (the answer isn't shown: it holds the account's email and API key)";

/// Vast's `/users/current/`: `credit` (the prepaid amount the console shows) and nothing
/// else — not even in the error.
pub fn parse_vast_user(v: &Value) -> Result<Live, Fail> {
    match v.get("credit").and_then(loose_f64) {
        Some(c) if in_range(c) => {
            Ok(Live { usd: c, provider_burn_per_hr: None, spend_limit_per_hr: None, under_balance: None, reduced: false })
        }
        _ => Err(Fail::new(FailKind::Drift, None, VAST_NO_CREDIT)),
    }
}

/// RunPod's `GET /v2/billing` answer (`records[] {startTime, totalAmount, …}`, `metadata.query
/// {startTime, endTime, bucketSize}`). Anything that isn't plainly hourly, dated and
/// non-negative is drift — a misread history would make the estimate overstate.
pub fn parse_billing(v: &Value) -> Result<Billed, Fail> {
    let drift = |why: &str| Fail::new(FailKind::Drift, None, format!("billing history: {why}"));
    let time = |x: Option<&Value>| x.and_then(Value::as_str).and_then(crate::snapshot::parse_rfc3339);
    let q = v.pointer("/metadata/query").filter(|q| q.is_object()).ok_or_else(|| drift("no metadata.query"))?;
    if q.get("bucketSize").and_then(Value::as_str) != Some("hour") {
        return Err(drift("the buckets aren't hourly"));
    }
    let from = time(q.get("startTime")).ok_or_else(|| drift("no readable metadata.query.startTime"))?;
    let to = time(q.get("endTime")).ok_or_else(|| drift("no readable metadata.query.endTime"))?;
    if from > to {
        return Err(drift("the window ends before it starts"));
    }
    let records = v.get("records").and_then(Value::as_array).ok_or_else(|| drift("no records array"))?;
    let mut buckets = Vec::with_capacity(records.len());
    for r in records {
        let start = time(r.get("startTime")).ok_or_else(|| drift("a record without a readable startTime"))?;
        let usd = r
            .get("totalAmount")
            .and_then(loose_f64)
            .filter(|a| a.is_finite() && (0.0..=*AMOUNT_RANGE.end()).contains(a))
            .ok_or_else(|| drift("a record's totalAmount is missing, negative or not a number"))?;
        buckets.push((start, usd));
    }
    buckets.sort_by_key(|b| b.0);
    Ok(Billed { buckets, from, to })
}

// ---------------------------------------------------------------------------------------
// Burn
// ---------------------------------------------------------------------------------------

/// An account's burn right now (USD/h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Burn {
    pub per_hr: f64,
    /// Billing pods with no price: the sum is a floor.
    pub unpriced: usize,
    /// The last recorded sample, standing in for a listing that failed: possibly out of date.
    pub stale: bool,
}

impl Burn {
    /// The real burn may be higher than `per_hr` (shown `≥`, runway `≤`).
    pub fn floor(&self) -> bool {
        self.unpriced > 0 || self.stale
    }
}

/// Each prepaid account's burn as the fleet listed it: `None` when its provider failed to
/// list (or wasn't asked) — unknown, never zero.
pub type Burns = BTreeMap<Account, Option<Burn>>;

/// Sum the billing pods ([`bills_hourly`]) per prepaid account from a listing (`pods` are
/// its pods, enriched where the caller enriches; `listing` says which providers answered).
pub fn burns(listing: &Listing, pods: &[Pod]) -> Burns {
    Account::ALL.into_iter().map(|a| (a, listed_burn(listing, pods, a.name()))).collect()
}

fn listed_burn(listing: &Listing, pods: &[Pod], provider: &str) -> Option<Burn> {
    if listing.status(provider) != ListingStatus::Ok {
        return None;
    }
    let mut b = Burn::default();
    for p in pods.iter().filter(|p| p.provider == provider && bills_hourly(&p.provider, &p.status)) {
        match p.cost_per_hr.filter(|c| c.is_finite() && *c >= 0.0) {
            Some(c) => b.per_hr += c,
            None => b.unpriced += 1,
        }
    }
    Some(b)
}

/// The burn to judge by (`B` in the spec): with a listing, the higher of its sum and the
/// provider's own rate (the account drains for staff pods and volumes too); without one,
/// the provider's rate, else the last recorded sample (stale), else unknown — **never 0**.
pub fn burn_now(listed: Option<Burn>, provider_rate: Option<f64>, samples: &[(u64, f64)]) -> Option<Burn> {
    match (listed, provider_rate) {
        // The provider's rate covers everything on the account, unpriced pods included.
        (Some(l), Some(p)) => Some(Burn { per_hr: l.per_hr.max(p), unpriced: 0, stale: false }),
        (Some(l), None) => Some(l),
        (None, Some(p)) => Some(Burn { per_hr: p, unpriced: 0, stale: false }),
        (None, None) => samples.last().map(|&(_, b)| Burn { per_hr: b, unpriced: 0, stale: true }),
    }
}

// ---------------------------------------------------------------------------------------
// State (balance.json)
// ---------------------------------------------------------------------------------------

/// Where an anchor's amount came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorSource {
    Live,
    /// `arena balance set` — read off the console by the operator.
    Manual,
}

/// A known balance at a known time: what estimates count down from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub usd: f64,
    pub at: u64,
    pub source: AnchorSource,
}

impl Anchor {
    /// Fit to estimate from at `now`: at most [`ANCHOR_MAX_AGE`] old and at most
    /// [`CLOCK_SKEW`] in the future.
    pub fn usable(&self, now: u64) -> bool {
        self.usd.is_finite() && self.at <= now.saturating_add(CLOCK_SKEW) && now.saturating_sub(self.at) <= ANCHOR_MAX_AGE
    }
}

/// The last live reading with its extras.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderFacts {
    pub usd: f64,
    pub burn_per_hr: Option<f64>,
    pub spend_limit_per_hr: Option<f64>,
    pub under_balance: Option<bool>,
    pub at: u64,
}

/// A failing source: since when (the first failure after the last success), and the latest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailRecord {
    pub kind: FailKind,
    pub http: Option<u16>,
    pub detail: Option<String>,
    pub since: u64,
    pub last: u64,
}

/// The fallback check's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The estimate was at most a little above the truth (≤ live + max($1, 5 %)).
    Safe,
    /// The estimate would have shown more money than there was.
    Overstates,
}

/// One fallback check: what the estimate from the check anchor predicted against the live
/// reading, over `horizon_secs`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRecord {
    pub at: u64,
    pub horizon_secs: u64,
    pub predicted: f64,
    pub live: f64,
    pub verdict: Verdict,
}

/// The most billing has ever said was spent since the anchor at `anchor_at`: a later
/// billing failure (or a short window) can't make the estimate rise again.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BilledMax {
    pub anchor_at: u64,
    pub usd: f64,
}

/// Everything remembered about one account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    /// The fingerprint of the key these readings came from ([`key_fp`]).
    pub key_fp: String,
    #[serde(default)]
    pub anchor: Option<Anchor>,
    /// The (live) anchor the fallback check estimates from.
    #[serde(default)]
    pub check_anchor: Option<Anchor>,
    #[serde(default)]
    pub provider: Option<ProviderFacts>,
    /// `(when, USD/h)`: change points (any rise; a drop of >1 % or >$0.01) plus an hourly
    /// heartbeat — what metering integrates.
    #[serde(default)]
    pub samples: Vec<(u64, f64)>,
    #[serde(default)]
    pub live_ok_at: Option<u64>,
    #[serde(default)]
    pub live_fail: Option<FailRecord>,
    /// Set while only the minimal query works (RunPod dropped a field).
    #[serde(default)]
    pub live_reduced: Option<FailRecord>,
    #[serde(default)]
    pub billing_ok_at: Option<u64>,
    #[serde(default)]
    pub billing_fail: Option<FailRecord>,
    #[serde(default)]
    pub check: Option<CheckRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billed_max: Option<BilledMax>,
}

impl AccountState {
    fn new(key_fp: &str) -> Self {
        AccountState { key_fp: key_fp.to_string(), ..Default::default() }
    }

    /// The last live reading, if it is younger than [`FRESH`] (and not from the future).
    fn fresh_facts(&self, now: u64) -> Option<ProviderFacts> {
        self.provider.filter(|p| p.at <= now.saturating_add(CLOCK_SKEW) && now.saturating_sub(p.at) < FRESH)
    }

    /// When live was last tried, successfully or not.
    pub fn last_live_attempt(&self) -> Option<u64> {
        [self.live_ok_at, self.live_fail.as_ref().map(|f| f.last)].into_iter().flatten().max()
    }
}

/// The whole file: one entry per prepaid account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BalanceState {
    pub version: u32,
    #[serde(default)]
    pub accounts: BTreeMap<Account, AccountState>,
}

impl Default for BalanceState {
    fn default() -> Self {
        BalanceState { version: STATE_VERSION, accounts: BTreeMap::new() }
    }
}

impl BalanceState {
    /// `account`'s entry, if it was recorded with this key.
    pub fn get(&self, account: Account, key_fp: &str) -> Option<&AccountState> {
        self.accounts.get(&account).filter(|s| s.key_fp == key_fp)
    }
}

// ---------------------------------------------------------------------------------------
// Estimates, runway, levels
// ---------------------------------------------------------------------------------------

/// Spend from `anchor_at` to `now` by arena's own metering: a step integral over the burn
/// points — `(anchor_at, b_A)`, the samples strictly between, `(now, b_now)` — each segment
/// charged at the **higher** of its two ends (a change between two observations is never
/// undercounted). `b_A` is the last sample at/before the anchor, else the first after, else
/// `b_now`. A clock that went backwards adds nothing.
pub fn metered_spend(anchor_at: u64, samples: &[(u64, f64)], now: u64, b_now: f64) -> f64 {
    let b_a = samples
        .iter()
        .rev()
        .find(|(t, _)| *t <= anchor_at)
        .or_else(|| samples.iter().find(|(t, _)| *t > anchor_at))
        .map_or(b_now, |s| s.1);
    let mut points = vec![(anchor_at, b_a)];
    points.extend(samples.iter().filter(|(t, _)| *t > anchor_at && *t < now).copied());
    points.push((now, b_now));
    points
        .windows(2)
        .map(|w| w[1].0.saturating_sub(w[0].0) as f64 / 3_600.0 * w[0].1.max(w[1].1).max(0.0))
        .sum()
}

/// Which spend figure an estimate used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Billed,
    Metered,
}

impl Method {
    fn label(self) -> &'static str {
        match self {
            Method::Billed => "billed",
            Method::Metered => "metered",
        }
    }
}

/// An estimate and how it was made.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    pub usd: f64,
    pub method: Method,
    pub metered: f64,
    pub billed: Option<f64>,
}

/// The anchor minus `max(metered, billed)` (`billed` missing → metered alone). Pure.
pub fn estimate(anchor: &Anchor, samples: &[(u64, f64)], billed: Option<f64>, b_now: f64, now: u64) -> Estimate {
    let metered = metered_spend(anchor.at, samples, now, b_now);
    let spend = billed.map_or(metered, |b| b.max(metered));
    let method = if billed.is_some_and(|b| b >= metered) { Method::Billed } else { Method::Metered };
    Estimate { usd: anchor.usd - spend, method, metered, billed }
}

/// Hours until `balance` runs out at `burn`; `None` for no burn (below [`NO_BURN`]).
pub fn runway(balance: f64, burn: &Burn) -> Option<f64> {
    (burn.per_hr >= NO_BURN).then(|| balance.max(0.0) / burn.per_hr)
}

/// How urgent an account is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Low,
    Critical,
    /// No balance while it burns (or the burn is unknown).
    Unknown,
    /// Postpaid: nothing to judge.
    None,
}

/// Critical: balance ≤ 0, live `underBalance`, or runway under [`CRITICAL_HOURS`]; low:
/// runway under `BALANCE_WARN_HOURS` (`0` turns both runway levels off, never the other
/// two); unknown: no balance while burning or with the burn unknown; else ok — no burn too.
pub fn level(
    balance: Option<f64>,
    under_balance: Option<bool>,
    burn: Option<&Burn>,
    runway_hours: Option<f64>,
    policy: &BalancePolicy,
) -> Level {
    let Some(b) = balance else {
        return if burn.is_some_and(|b| b.per_hr < NO_BURN) { Level::Ok } else { Level::Unknown };
    };
    if b <= 0.0 || under_balance == Some(true) {
        return Level::Critical;
    }
    match runway_hours {
        Some(h) if policy.warn_hours > 0.0 && h < CRITICAL_HOURS => Level::Critical,
        Some(h) if h < policy.warn_hours => Level::Low,
        _ => Level::Ok,
    }
}

/// The warning threshold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BalancePolicy {
    pub warn_hours: f64,
}

impl Default for BalancePolicy {
    fn default() -> Self {
        BalancePolicy { warn_hours: DEFAULT_WARN_HOURS }
    }
}

impl BalancePolicy {
    /// From `BALANCE_WARN_HOURS`. Never fails: a bad value uses the default and comes back
    /// with the one warning line to show (in `arena balance`; `config check` marks it ✗) —
    /// a typo must never break `pods list`.
    pub fn from_config(cfg: &Config) -> (Self, Option<String>) {
        match warn_hours(cfg) {
            Ok(h) => (BalancePolicy { warn_hours: h }, None),
            Err(e) => (BalancePolicy::default(), Some(format!("warning: {e} — using {DEFAULT_WARN_HOURS}"))),
        }
    }
}

/// `BALANCE_WARN_HOURS` as written: unset/empty → [`DEFAULT_WARN_HOURS`]; a finite number
/// of hours ≥ 0; anything else is the error (a long value isn't echoed — it may be a key
/// pasted on the wrong line).
pub fn warn_hours(cfg: &Config) -> Result<f64, String> {
    match cfg.get("BALANCE_WARN_HOURS").map(str::trim) {
        None | Some("") => Ok(DEFAULT_WARN_HOURS),
        Some(v) => match v.parse::<f64>() {
            Ok(h) if h.is_finite() && h >= 0.0 => Ok(h),
            _ => {
                let shown = if v.chars().count() <= 12 { format!("`{v}`") } else { "a long value".into() };
                Err(format!("BALANCE_WARN_HOURS must be a number of hours ≥ 0 (got {shown})"))
            }
        },
    }
}

// ---------------------------------------------------------------------------------------
// Observations → state ([`apply`])
// ---------------------------------------------------------------------------------------

/// How a surface observes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A footer, the preflight, teardown, the TUI: live only when the last attempt is
    /// [`FRESH`] old, billing only while live is failing; within [`INLINE_TIMEOUT`].
    Inline,
    /// `arena balance`: live always, billing whenever there is an anchor (and the check).
    Full,
    /// `arena balance --no-live`: what the fallback would say — no live call, the estimate
    /// tier forced, nothing written.
    NoLive,
    /// `snapshot`, `config check`: the state file only — no call, nothing written.
    CacheOnly,
}

impl Mode {
    /// Whether this mode records what it saw.
    pub fn writes(self) -> bool {
        matches!(self, Mode::Inline | Mode::Full)
    }
}

/// What one observation got for one configured account (`None` = not asked).
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub account: Account,
    pub key_fp: String,
    pub live: Option<Result<Live, Fail>>,
    pub billed: Option<Result<Billed, Fail>>,
}

impl Fetched {
    /// A configured account with nothing fetched (cache-only surfaces).
    pub fn nothing(account: Account, key_fp: impl Into<String>) -> Self {
        Fetched { account, key_fp: key_fp.into(), live: None, billed: None }
    }

    fn live_ok(&self) -> Option<&Live> {
        self.live.as_ref().and_then(|r| r.as_ref().ok())
    }

    fn billed_ok(&self) -> Option<&Billed> {
        self.billed.as_ref().and_then(|r| r.as_ref().ok())
    }
}

/// Record one observation: anchors, burn samples, failures, the fallback check and its
/// rotation, pruning — and a fresh entry for an account whose key changed. Pure; the
/// writer runs it on the state reloaded under the lock, so concurrent writers merge.
/// `--no-live` and cache-only observations record nothing.
pub fn apply(mut state: BalanceState, fetched: &[Fetched], burns: &Burns, mode: Mode, now: u64) -> BalanceState {
    if !mode.writes() {
        return state;
    }
    state.version = STATE_VERSION;
    for f in fetched {
        let st = state.accounts.entry(f.account).or_insert_with(|| AccountState::new(&f.key_fp));
        if st.key_fp != f.key_fp {
            // Another key — another account, maybe: never inherit its anchor or samples.
            *st = AccountState::new(&f.key_fp);
        }
        apply_one(st, f, burns.get(&f.account).copied().flatten(), mode, now);
    }
    state
}

/// The failure record after `f`: `since` kept from an ongoing failure.
fn fail_record(prev: Option<&FailRecord>, f: &Fail, now: u64) -> FailRecord {
    FailRecord { kind: f.kind, http: f.http, detail: f.detail.clone(), since: prev.map_or(now, |p| p.since.min(now)), last: now }
}

fn apply_one(st: &mut AccountState, f: &Fetched, listed: Option<Burn>, mode: Mode, now: u64) {
    let live = f.live_ok();
    let billed = f.billed_ok();
    let provider_rate = match live {
        Some(l) => l.provider_burn_per_hr,
        None => st.fresh_facts(now).and_then(|p| p.burn_per_hr),
    };
    let burn = burn_now(listed, provider_rate, &st.samples);

    // The fallback check (`arena balance` only): what the estimate from the check anchor
    // says now, against the truth just read. Burn as the fallback would see it — the
    // listing, not the provider's rate (which goes with GraphQL).
    if let (Mode::Full, Some(l)) = (mode, live) {
        let fallback_burn = burn_now(listed, None, &st.samples);
        let anchor = st.check_anchor.filter(|a| a.usable(now) && now.saturating_sub(a.at) >= CHECK_MIN);
        if let (Some(ca), Some(b)) = (anchor, fallback_burn) {
            let predicted = estimate(&ca, &st.samples, billed.and_then(|bl| billed_since(bl, ca.at)), b.per_hr, now).usd;
            let verdict = if predicted <= l.usd + (0.05 * l.usd.abs()).max(1.0) { Verdict::Safe } else { Verdict::Overstates };
            st.check = Some(CheckRecord { at: now, horizon_secs: now.saturating_sub(ca.at), predicted, live: l.usd, verdict });
        }
    }

    match &f.live {
        Some(Ok(l)) => {
            let anchor = Anchor { usd: l.usd, at: now, source: AnchorSource::Live };
            st.anchor = Some(anchor);
            st.provider = Some(ProviderFacts {
                usd: l.usd,
                burn_per_hr: l.provider_burn_per_hr,
                spend_limit_per_hr: l.spend_limit_per_hr,
                under_balance: l.under_balance,
                at: now,
            });
            st.live_ok_at = Some(now);
            st.live_fail = None;
            st.live_reduced = l.reduced.then(|| {
                let why = Fail::new(FailKind::Drift, None, "the full balance query was rejected; the minimal one works");
                fail_record(st.live_reduced.as_ref(), &why, now)
            });
            let rotate = st.check_anchor.is_none_or(|ca| !ca.usable(now) || now.saturating_sub(ca.at) >= CHECK_ROTATE);
            if rotate {
                st.check_anchor = Some(anchor);
            }
        }
        Some(Err(e)) => st.live_fail = Some(fail_record(st.live_fail.as_ref(), e, now)),
        None => {}
    }

    match &f.billed {
        Some(Ok(b)) => {
            st.billing_ok_at = Some(now);
            st.billing_fail = None;
            if let Some(v) = st.anchor.and_then(|a| billed_since(b, a.at).map(|v| (a.at, v))) {
                let seen = st.billed_max.filter(|m| m.anchor_at == v.0).map_or(0.0, |m| m.usd);
                st.billed_max = Some(BilledMax { anchor_at: v.0, usd: v.1.max(seen) });
            }
        }
        Some(Err(e)) => st.billing_fail = Some(fail_record(st.billing_fail.as_ref(), e, now)),
        None => {}
    }
    if st.billed_max.is_some_and(|m| st.anchor.is_none_or(|a| a.at != m.anchor_at)) {
        st.billed_max = None;
    }

    // A sample only from a real reading (a listing, or the provider's rate this run) —
    // never the stale stand-in, which would only repeat itself.
    if let Some(b) = burn.filter(|b| !b.stale) {
        push_sample(&mut st.samples, now, b.per_hr);
    }
    prune(&mut st.samples, now);
}

/// Append `(now, b)` when it is a change point or the heartbeat is due. **Any rise** (above
/// [`BURN_NOISE`]) is recorded: an unrecorded higher burn shown once could otherwise be
/// forgotten, and the estimate rise on the next look. A drop needs >1 % or >$0.01 (an
/// ignored drop only overcounts spend).
fn push_sample(samples: &mut Vec<(u64, f64)>, now: u64, b: f64) {
    if !b.is_finite() || b < 0.0 {
        return;
    }
    match samples.last().copied() {
        None => samples.push((now, b)),
        Some((t, _)) if now <= t => {} // same second, or the clock went back
        Some((t, last)) => {
            let d = b - last;
            if d > BURN_NOISE || -d > 0.01 || -d > 0.01 * last.abs() || now - t >= SAMPLE_HEARTBEAT {
                samples.push((now, b));
            }
        }
    }
}

/// Drop samples older than [`SAMPLE_KEEP`] (keeping the newest of those as the baseline),
/// then cap at [`SAMPLE_MAX`].
fn prune(samples: &mut Vec<(u64, f64)>, now: u64) {
    let cutoff = now.saturating_sub(SAMPLE_KEEP);
    if let Some(i) = samples.iter().rposition(|&(t, _)| t <= cutoff) {
        samples.drain(..i);
    }
    if samples.len() > SAMPLE_MAX {
        let extra = samples.len() - SAMPLE_MAX;
        samples.drain(..extra);
    }
}

// ---------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------

/// Which tier a balance came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Live,
    Estimate,
    Unknown,
    Postpaid,
}

/// One account as every surface shows it (and `arena balance --json` prints it).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AccountView {
    /// `runpod`, `vast` or `hetzner`.
    pub account: String,
    /// `USD` or `EUR` — never summed across.
    pub currency: &'static str,
    pub source: Source,
    pub balance: Option<f64>,
    /// When the reading (live) or the anchor (estimate) was taken.
    pub as_of: Option<u64>,
    pub anchor_source: Option<AnchorSource>,
    pub method: Option<Method>,
    /// What the runway is judged at.
    pub burn: Option<Burn>,
    /// The fleet's billing pods there, as listed (`None`: the listing failed).
    pub listed_per_hr: Option<f64>,
    pub provider_burn_per_hr: Option<f64>,
    pub spend_limit_per_hr: Option<f64>,
    pub under_balance: Option<bool>,
    pub runway_hours: Option<f64>,
    /// The burn is a floor (unpriced pods, a stale sample): the runway may be shorter.
    pub runway_upper_bound: bool,
    pub level: Level,
    /// The live source is failing (since when, why).
    pub live_failing: Option<FailRecord>,
    /// The failure is worth a note (not a young transient one).
    pub noted: bool,
    /// Only the minimal live query works.
    pub live_reduced: bool,
    pub check: Option<CheckRecord>,
}

impl AccountView {
    fn blank(account: &str, currency: &'static str, source: Source) -> Self {
        AccountView {
            account: account.to_string(),
            currency,
            source,
            balance: None,
            as_of: None,
            anchor_source: None,
            method: None,
            burn: None,
            listed_per_hr: None,
            provider_burn_per_hr: None,
            spend_limit_per_hr: None,
            under_balance: None,
            runway_hours: None,
            runway_upper_bound: false,
            level: Level::Unknown,
            live_failing: None,
            noted: false,
            live_reduced: false,
            check: None,
        }
    }

    fn prepaid(&self) -> bool {
        self.source != Source::Postpaid
    }
}

/// Whether a failure deserves a note: anything but a transient one younger than
/// [`TRANSIENT_NOTE_AFTER`].
fn noted(f: &FailRecord, now: u64) -> bool {
    f.kind != FailKind::Transient || now.saturating_sub(f.since) >= TRANSIENT_NOTE_AFTER
}

/// Each observed account's view: the tier, balance, burn, runway and level. `state` is the
/// state after [`apply`] (or as loaded, for modes that don't write). Pure.
pub fn views(state: &BalanceState, fetched: &[Fetched], burns: &Burns, policy: &BalancePolicy, mode: Mode, now: u64) -> Vec<AccountView> {
    fetched
        .iter()
        .map(|f| view(state.get(f.account, &f.key_fp), f, burns.get(&f.account).copied().flatten(), policy, mode, now))
        .collect()
}

fn view(st: Option<&AccountState>, f: &Fetched, listed: Option<Burn>, policy: &BalancePolicy, mode: Mode, now: u64) -> AccountView {
    let empty = AccountState::default();
    let st = st.unwrap_or(&empty);
    let live_now = f.live_ok();
    // Live: read this run — or, except under `--no-live`, a reading younger than FRESH.
    let facts = match live_now {
        Some(l) => Some(ProviderFacts {
            usd: l.usd,
            burn_per_hr: l.provider_burn_per_hr,
            spend_limit_per_hr: l.spend_limit_per_hr,
            under_balance: l.under_balance,
            at: now,
        }),
        None if mode != Mode::NoLive => st.fresh_facts(now),
        None => None,
    };
    let burn = burn_now(listed, facts.and_then(|p| p.burn_per_hr), &st.samples);
    let mut v = AccountView::blank(f.account.name(), "USD", Source::Unknown);
    v.listed_per_hr = listed.map(|b| b.per_hr);
    v.provider_burn_per_hr = facts.and_then(|p| p.burn_per_hr);
    v.spend_limit_per_hr = facts.and_then(|p| p.spend_limit_per_hr).or(st.provider.and_then(|p| p.spend_limit_per_hr));
    v.under_balance = facts.and_then(|p| p.under_balance);
    v.live_failing = if live_now.is_some() { None } else { st.live_fail.clone() };
    v.noted = v.live_failing.as_ref().is_some_and(|fr| noted(fr, now));
    v.live_reduced = live_now.map_or(st.live_reduced.is_some(), |l| l.reduced);
    v.check = st.check;
    if let Some(p) = facts {
        v.source = Source::Live;
        v.balance = Some(p.usd);
        v.as_of = Some(p.at);
    } else if let (Some(a), Some(b)) = (st.anchor.filter(|a| a.usable(now)), burn) {
        let billed_now = f.billed_ok().and_then(|bl| billed_since(bl, a.at));
        let billed_seen = st.billed_max.filter(|m| m.anchor_at == a.at).map(|m| m.usd);
        let billed = match (billed_now, billed_seen) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
        let e = estimate(&a, &st.samples, billed, b.per_hr, now);
        v.source = Source::Estimate;
        v.balance = Some(e.usd);
        v.as_of = Some(a.at);
        v.anchor_source = Some(a.source);
        v.method = Some(e.method);
    }
    v.burn = burn;
    v.runway_hours = v.balance.zip(burn).and_then(|(bal, b)| runway(bal, &b));
    v.runway_upper_bound = v.runway_hours.is_some() && burn.is_some_and(|b| b.floor());
    v.level = level(v.balance, v.under_balance, burn.as_ref(), v.runway_hours, policy);
    v
}

/// Hetzner's row: postpaid, its €/h from the listing (`None` when it isn't configured).
/// Never a runway or a level; volumes aren't in the listing, so the €/h is a floor.
pub fn postpaid_view(listing: &Listing, pods: &[Pod]) -> Option<AccountView> {
    if listing.status("hetzner") == ListingStatus::NotQueried {
        return None;
    }
    let mut v = AccountView::blank("hetzner", "EUR", Source::Postpaid);
    v.burn = listed_burn(listing, pods, "hetzner");
    v.listed_per_hr = v.burn.map(|b| b.per_hr);
    v.level = Level::None;
    Some(v)
}

/// `balance --check`'s exit: 2 if any account is critical, else 3 if any is unknown while
/// burning, else 1 if any is low, else 0.
pub fn check_exit(views: &[AccountView]) -> i32 {
    let any = |l: Level| views.iter().any(|v| v.level == l);
    if any(Level::Critical) {
        2
    } else if any(Level::Unknown) {
        3
    } else if any(Level::Low) {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------

/// `$3.10`, `-$2.00` (never `$-2.00`).
fn usd(v: f64) -> String {
    if v < 0.0 {
        format!("-{}", fmt_money("$", -v))
    } else {
        fmt_money("$", v)
    }
}

fn money(v: &AccountView, x: f64) -> String {
    if v.currency == "EUR" {
        fmt_money("€", x)
    } else {
        usd(x)
    }
}

/// `2026-10-11 05:00 UTC`.
fn date(t: u64) -> String {
    format!("{} UTC", short_time(t))
}

/// `2026-10-11 05:00`.
fn short_time(t: u64) -> String {
    let s = rfc3339(t);
    s.get(..16).map(|x| x.replacen('T', " ", 1)).unwrap_or(s)
}

/// `72` / `1.5` — a threshold as written.
fn num(h: f64) -> String {
    if h.fract() == 0.0 {
        format!("{}", h as u64)
    } else {
        format!("{h}")
    }
}

/// Hours of runway: `<1h`, `≈6.6h`, `≈31h` (under 48 h), `≈3.1d`, `>60d`. Rounded down —
/// a runway is never shown longer than it is.
pub fn fmt_hours(h: f64) -> String {
    let down = |x: f64| (x * 10.0).floor() / 10.0;
    if h < 1.0 {
        "<1h".into()
    } else if h < 10.0 {
        format!("≈{:.1}h", down(h))
    } else if h < 48.0 {
        format!("≈{}h", h.floor() as u64)
    } else if h < 60.0 * 24.0 {
        format!("≈{:.1}d", down(h / 24.0))
    } else {
        ">60d".into()
    }
}

/// The RUNWAY cell: [`fmt_hours`] (`≤` instead of `≈` when the burn is a floor), `no
/// burn`, or `?` when the balance or the burn is unknown.
pub fn fmt_runway(v: &AccountView) -> String {
    match (v.balance, v.burn, v.runway_hours) {
        (Some(_), Some(b), _) if b.per_hr < NO_BURN => "no burn".into(),
        (Some(_), Some(_), Some(h)) if v.runway_upper_bound => {
            let s = fmt_hours(h);
            match s.strip_prefix('≈') {
                Some(rest) => format!("≤{rest}"),
                None => s,
            }
        }
        (Some(_), Some(_), Some(h)) => fmt_hours(h),
        _ => "?".into(),
    }
}

/// The BALANCE cell: `$32.54`, `~$31.20` (an estimate, always `~`), `?`, `postpaid`.
pub fn balance_label(v: &AccountView) -> String {
    match (v.source, v.balance) {
        (Source::Postpaid, _) => "postpaid".into(),
        (Source::Estimate, Some(b)) => format!("~{}", money(v, b)),
        (_, Some(b)) => money(v, b),
        _ => "?".into(),
    }
}

/// `live` (read this run) or `live 3m` (a reading that young).
fn live_label(v: &AccountView, now: u64) -> String {
    match v.as_of {
        Some(t) if t < now => format!("live {}", fmt_age(now - t)),
        _ => "live".into(),
    }
}

/// The SOURCE cell: `live`, `live 3m`, `est. (live n/a 2d, billed)`, `est. (manual 3h,
/// metered)`, `est. (from live 4m, metered)` (under `--no-live`), `?`, `postpaid`. Every
/// estimate says `est.`.
pub fn source_label(v: &AccountView, now: u64) -> String {
    match v.source {
        Source::Live => live_label(v, now),
        Source::Estimate => {
            let age = fmt_age(now.saturating_sub(v.as_of.unwrap_or(now)));
            let mut why = Vec::new();
            match (v.anchor_source, &v.live_failing) {
                (Some(AnchorSource::Manual), Some(f)) => {
                    why.push(format!("manual {age}"));
                    why.push(format!("live n/a {}", fmt_age(now.saturating_sub(f.since))));
                }
                (Some(AnchorSource::Manual), None) => why.push(format!("manual {age}")),
                (_, Some(_)) => why.push(format!("live n/a {age}")),
                (_, None) => why.push(format!("from live {age}")),
            }
            if let Some(m) = v.method {
                why.push(m.label().into());
            }
            format!("est. ({})", why.join(", "))
        }
        Source::Unknown => "?".into(),
        Source::Postpaid => "postpaid".into(),
    }
}

/// The BURN cell: `$0.47/h`, `≥$0.47/h` (a floor), `?`.
pub fn burn_label(v: &AccountView) -> String {
    match v.burn {
        Some(b) => format!("{}{}/h", if b.floor() { "≥" } else { "" }, money(v, b.per_hr)),
        None => "?".into(),
    }
}

fn level_label(l: Level) -> &'static str {
    match l {
        Level::Ok => "ok",
        Level::Low => "⚠ low",
        Level::Critical => "‼ critical",
        Level::Unknown => "? unknown",
        Level::None => "-",
    }
}

fn glyph(l: Level) -> &'static str {
    match l {
        Level::Low => "⚠ ",
        Level::Critical => "‼ ",
        _ => "",
    }
}

/// When it runs out (`2026-10-11 05:00`), for a runway under 60 days.
fn runs_out(v: &AccountView, now: u64) -> Option<String> {
    v.runway_hours.filter(|h| *h < 60.0 * 24.0).map(|h| short_time(now + (h * 3_600.0) as u64))
}

/// Why an account is critical, in words that say what happens.
fn critical_reason(v: &AccountView) -> String {
    let acct = Account::from_name(&v.account);
    let set = format!("arena balance set {} <USD>", v.account);
    if v.balance.is_some_and(|b| b <= 0.0) && v.source == Source::Estimate {
        if v.burn.is_some_and(|b| b.per_hr >= NO_BURN) {
            format!("≤$0 est. — pods still running: probably a top-up arena can't see; {set}")
        } else {
            format!("≤$0 est. — top up, or if you already did: {set}")
        }
    } else if v.under_balance == Some(true) {
        "RunPod reports the account under its minimum balance".into()
    } else {
        acct.map_or("at $0 the provider stops its pods", Account::at_zero).into()
    }
}

/// The `?` explanation with its nudge.
fn no_reading(v: &AccountView) -> String {
    format!("(no reading — arena balance set {} <USD>)", v.account)
}

/// One prepaid account in a footer: `runpod $32.54 live ≈69h`, `vast ~$118.30 est.
/// ≈49d`; a low or critical one with its source spelled out, its threshold or its
/// consequence; an unknown one as `?` with the nudge and its burn.
fn footer_part(v: &AccountView, policy: &BalancePolicy, now: u64) -> String {
    let name = &v.account;
    if v.source == Source::Unknown {
        return format!("{name} ? {} {}", no_reading(v), burn_label(v));
    }
    let detailed = matches!(v.level, Level::Low | Level::Critical) || v.noted;
    let src = match v.source {
        Source::Live => live_label(v, now),
        _ if detailed => source_label(v, now),
        _ => "est.".into(),
    };
    let mut s = format!("{}{name} {} {src} {}", glyph(v.level), balance_label(v), fmt_runway(v));
    match v.level {
        Level::Low => s.push_str(&format!(" < {}h", num(policy.warn_hours))),
        Level::Critical => s.push_str(&format!(" — {}", critical_reason(v))),
        _ => {}
    }
    s
}

/// The one line under `pods list`'s fleet footer (and `snapshot`'s): `balance: runpod
/// $32.54 live ≈69h · vast ~$118.30 est. ≈49d`. Prepaid accounts only (the fleet footer
/// already has Hetzner's €/h); `None` when there are none.
pub fn footer_line(views: &[AccountView], policy: &BalancePolicy, now: u64) -> Option<String> {
    let parts: Vec<String> = views.iter().filter(|v| v.prepaid()).map(|v| footer_part(v, policy, now)).collect();
    (!parts.is_empty()).then(|| format!("balance: {}", parts.join(" · ")))
}

/// `arena balance`'s table — one row per account, currencies never summed — and the line
/// saying how it's computed.
pub fn render_table(views: &[AccountView], now: u64) -> String {
    if views.is_empty() {
        return "(no provider keys configured — nothing to check)\n".into();
    }
    let headers = ["ACCOUNT", "BALANCE", "SOURCE", "BURN", "RUNWAY", "RUNS OUT (UTC)", "LEVEL"];
    let align = [Align::Left, Align::Right, Align::Left, Align::Right, Align::Left, Align::Left, Align::Left];
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| {
            let runway = if v.prepaid() { fmt_runway(v) } else { "-".into() };
            vec![
                v.account.clone(),
                balance_label(v),
                source_label(v, now),
                burn_label(v),
                runway,
                runs_out(v, now).unwrap_or_else(|| "-".into()),
                level_label(v.level).into(),
            ]
        })
        .collect();
    let mut out = table::render(&headers, &align, &rows);
    out.push_str(
        "burn = the higher of the provider's own rate and this fleet's billing pods there; runway = balance ÷ burn; \
         est. = the last reading − max(billed, metered) spend since (errs low)\n",
    );
    out
}

/// `drift, HTTP 400: Cannot query field "clientBalance"…`
fn fail_text(f: &FailRecord) -> String {
    let mut s = f.kind.label().to_string();
    if let Some(h) = f.http {
        s.push_str(&format!(", HTTP {h}"));
    }
    if let Some(d) = &f.detail {
        s.push_str(&format!(": {}", clip(d, 60)));
    }
    s
}

/// The check's reading in words: `read $0.40 low`, `read $3.10 HIGH`.
fn check_words(c: &CheckRecord) -> String {
    let d = c.predicted - c.live;
    if d <= 0.0 {
        format!("read {} low", usd(-d))
    } else if c.verdict == Verdict::Safe {
        format!("read {} high (within tolerance)", usd(d))
    } else {
        format!("read {} HIGH", usd(d))
    }
}

/// Provider spend arena doesn't list (volumes, stopped disks, serverless): the gap when the
/// provider's rate is over the listed burn by more than a cent.
fn unattributed(v: &AccountView) -> Option<f64> {
    match (v.provider_burn_per_hr, v.listed_per_hr) {
        (Some(p), Some(l)) if p > l + 0.01 => Some(p - l),
        _ => None,
    }
}

/// The notes under `arena balance`'s table: what's urgent and why, a failing source (once),
/// the fallback check, spend arena doesn't list, the `set` nudge, postpaid.
pub fn notes(views: &[AccountView], policy: &BalancePolicy, now: u64) -> Vec<String> {
    let mut out = Vec::new();
    for v in views {
        let name = &v.account;
        let at = runs_out(v, now).map(|t| format!(" (≈ {t} UTC)")).unwrap_or_default();
        match v.level {
            Level::Critical => out.push(format!(
                "‼ {name}: {} {} — {}; top up now (or enable auto-pay)",
                balance_label(v),
                fmt_runway(v),
                critical_reason(v)
            )),
            Level::Low => out.push(format!(
                "⚠ {name}: {} of runway at {}{at}, under BALANCE_WARN_HOURS={} — top up (or enable auto-pay)",
                fmt_runway(v),
                burn_label(v),
                num(policy.warn_hours)
            )),
            Level::Unknown => out.push(format!(
                "? {name}: no usable reading — read the balance off the console and run `arena balance set {name} <USD>`"
            )),
            _ => {}
        }
        if let Some(f) = v.live_failing.as_ref().filter(|_| v.noted) {
            out.push(format!("{name}: live balance failing since {} ({}) — estimates in use", date(f.since), fail_text(f)));
        }
        if v.live_reduced {
            out.push(format!(
                "{name}: RunPod rejected the full balance query (a field was dropped) — the balance is live via the \
                 minimal query; burn, spend limit and underBalance come from elsewhere"
            ));
        }
        if let Some(c) = &v.check {
            let horizon = fmt_age(c.horizon_secs);
            match c.verdict {
                Verdict::Safe => out.push(format!("{name}: fallback check ✓ over {horizon} (the estimate {})", check_words(c))),
                Verdict::Overstates => out.push(format!(
                    "⚠ {name}: fallback check: the estimate {} over {horizon} — don't rely on estimates",
                    check_words(c)
                )),
            }
        }
        if let Some(gap) = unattributed(v) {
            out.push(format!(
                "{name}: {} reports {}/h more than arena lists (network volumes, stopped pods' disks, serverless?)",
                Account::from_name(name).map_or("the provider", Account::title),
                usd(gap)
            ));
        }
        if v.source == Source::Postpaid {
            out.push(format!("{name}: postpaid — billed monthly in arrears, no balance to run out"));
        }
    }
    out
}

/// `arena balance -v`: per account the burn's parts, the provider's extras, the anchors, the
/// billing history used and the last failures — then where the state lives.
pub fn verbose_lines(views: &[AccountView], state: &BalanceState, fetched: &[Fetched], path: Option<&Path>, now: u64) -> Vec<String> {
    let mut out = Vec::new();
    let opt = |x: Option<f64>| x.map_or_else(|| "?".to_string(), |v| format!("{}/h", usd(v)));
    for v in views {
        out.push(format!("{}:", v.account));
        if v.source == Source::Postpaid {
            let burn = v.burn.map_or("? (its listing failed)".into(), |b| format!("{}/h", money(v, b.per_hr)));
            out.push(format!("  postpaid {burn} — servers only: volumes not counted"));
            continue;
        }
        let gap = match unattributed(v) {
            Some(g) => format!(" — gap {}/h not in the listing: volumes / stopped disks / serverless?", usd(g)),
            None => String::new(),
        };
        out.push(format!("  burn: provider {} · listed {}{gap}", opt(v.provider_burn_per_hr), opt(v.listed_per_hr)));
        let limit = v.spend_limit_per_hr.map_or("?".into(), |l| format!("{}/h", usd(l)));
        let under = v.under_balance.map_or("?".into(), |u| u.to_string());
        out.push(format!("  spendLimit {limit} · underBalance {under}"));
        let Some(f) = fetched.iter().find(|f| f.account.name() == v.account) else { continue };
        let st = state.get(f.account, &f.key_fp);
        let anchor = |a: Option<Anchor>| match a {
            Some(a) => format!(
                "{} {}, {} ago{}",
                if a.source == AnchorSource::Live { "live" } else { "manual" },
                usd(a.usd),
                fmt_age(now.saturating_sub(a.at)),
                if a.usable(now) { "" } else { " (unusable)" }
            ),
            None => "none".into(),
        };
        out.push(format!(
            "  anchor: {} · check anchor: {}",
            anchor(st.and_then(|s| s.anchor)),
            anchor(st.and_then(|s| s.check_anchor))
        ));
        match &f.billed {
            Some(Ok(b)) => {
                let since = st.and_then(|s| s.anchor).and_then(|a| billed_since(b, a.at));
                out.push(format!(
                    "  billing: {} hourly bucket(s), {} in all{}",
                    b.buckets.len(),
                    usd(b.buckets.iter().map(|x| x.1).sum()),
                    since.map(|s| format!(", {} since the anchor's hour", usd(s))).unwrap_or_default()
                ));
            }
            Some(Err(e)) => out.push(format!(
                "  billing: failed ({})",
                fail_text(&FailRecord { kind: e.kind, http: e.http, detail: e.detail.clone(), since: now, last: now })
            )),
            None if f.account == Account::Runpod => out.push("  billing: not fetched (no usable anchor)".into()),
            None => out.push("  billing: none (this provider has no billing history API)".into()),
        }
        if let Some(fr) = st.and_then(|s| s.billing_fail.as_ref()) {
            out.push(format!("  billing failing since {} ({})", date(fr.since), fail_text(fr)));
        }
        match st.and_then(|s| s.live_fail.as_ref()) {
            Some(fr) => out.push(format!("  last live failure: {} since {}", fail_text(fr), date(fr.since))),
            None => out.push("  last live failure: none".into()),
        }
        if let Some(fr) = st.and_then(|s| s.live_reduced.as_ref()) {
            out.push(format!("  full query rejected since {} — minimal query in use", date(fr.since)));
        }
    }
    match path {
        Some(p) => out.push(format!("state file: {}", p.display())),
        None => out.push("state file: none (no state directory) — live readings only".into()),
    }
    out
}

/// The TUI summary bar's balance: `(first, last, worst)`. A low or critical account goes
/// **first** (` ⚠ runpod balance ≈1.9d ·`, ahead of the counts — the bar is one unwrapped
/// line and its tail is cut on a narrow terminal); the rest last (`runpod $32.54 ≈69h · vast
/// ~$118.30 est. ≈49d`); `worst` picks the tone (critical → bad, low → warn).
pub fn tui_parts(views: &[AccountView]) -> (Option<String>, Option<String>, Level) {
    let mut first = Vec::new();
    let mut last = Vec::new();
    let mut worst = Level::Ok;
    for v in views.iter().filter(|v| v.prepaid()) {
        match v.level {
            Level::Low | Level::Critical => {
                let what = if v.under_balance == Some(true) && v.level == Level::Critical && v.balance.is_some_and(|b| b > 0.0) {
                    "under its minimum".to_string()
                } else {
                    fmt_runway(v)
                };
                first.push(format!("{}{} balance {what}", glyph(v.level), v.account));
                if worst != Level::Critical {
                    worst = v.level;
                }
            }
            _ => {
                let src = if v.source == Source::Estimate { " est." } else { "" };
                let runway = if v.source == Source::Unknown { String::new() } else { format!(" {}", fmt_runway(v)) };
                last.push(format!("{} {}{src}{runway}", v.account, balance_label(v)));
            }
        }
    }
    let first = (!first.is_empty()).then(|| format!(" {} ·", first.join(" · ")));
    let last = (!last.is_empty()).then(|| last.join(" · "));
    (first, last, worst)
}

/// The `create`/`up` preflight for the target provider's account: the balance, the burn now
/// plus the new pods' (`n × the dearest option`, an upper bound) and the runway after — then
/// RunPod's own refusals ahead of time: under an hour's credit for the new pods (it needs
/// "at least one hour's worth"), the planned burn over the spend limit, `underBalance`.
/// Advisory: it never blocks, and `--yes` changes nothing.
pub fn preflight_lines(v: &AccountView, new_pods: usize, price_per_pod: Option<f64>, policy: &BalancePolicy, now: u64) -> Vec<String> {
    let title = Account::from_name(&v.account).map_or("the provider", Account::title);
    let head = match v.source {
        Source::Unknown => format!("balance: {} ? {}", v.account, no_reading(v)),
        _ => format!("balance: {} {} {}", v.account, balance_label(v), source_label(v, now)),
    };
    let now_burn = v.burn.map(|b| b.per_hr);
    let new = price_per_pod.filter(|p| p.is_finite() && *p >= 0.0).map(|p| p * new_pods as f64);
    let floor = if v.burn.is_some_and(|b| b.floor()) { "≥" } else { "" };
    let now_part = now_burn.map_or("?/h now".into(), |b| format!("{floor}{}/h now", usd(b)));
    let new_part = match new {
        Some(x) => format!("≤{}/h for {new_pods} new pod(s)", usd(x)),
        None => "new pods (price unknown)".into(),
    };
    let after = v.balance.zip(now_burn).map(|(bal, b)| (bal, b + new.unwrap_or(0.0)));
    let (runway, flag) = match after {
        Some((_, total)) if total < NO_BURN => ("no burn".to_string(), String::new()),
        Some((bal, total)) => {
            let h = bal.max(0.0) / total;
            let flag = if policy.warn_hours > 0.0 && h < CRITICAL_HOURS {
                " ‼ under 24h".to_string()
            } else if h < policy.warn_hours {
                format!(" ⚠ below BALANCE_WARN_HOURS={}", num(policy.warn_hours))
            } else {
                String::new()
            };
            (fmt_hours(h), flag)
        }
        None => ("?".into(), String::new()),
    };
    let mut out = vec![format!("{head} · {now_part} + {new_part} → {runway}{flag}")];
    let runpod = v.account == Account::Runpod.name();
    if let (true, Some(bal), Some(x)) = (runpod, v.balance, new) {
        if bal < x {
            out.push(format!(
                "⚠ {title} needs ≥1h of credit for the new pods (≈{}); the account has {} — creates will likely fail (402)",
                usd(x),
                balance_label(v)
            ));
        }
    }
    if let (Some(limit), Some(b), Some(x)) = (v.spend_limit_per_hr, now_burn, new) {
        if b + x > limit {
            out.push(format!(
                "⚠ planned {}/h exceeds {title}'s spend limit {}/h — creates past it will be refused",
                usd(b + x),
                usd(limit)
            ));
        }
    }
    if v.under_balance == Some(true) {
        out.push(format!("‼ {title} reports the account under its minimum balance — creates will likely fail"));
    }
    out
}

/// `teardown --check`'s lines, after its verdict — informational, never part of it: what
/// is left on each prepaid account, and spend the provider reports that arena doesn't list.
pub fn teardown_info(views: &[AccountView], now: u64) -> Vec<String> {
    let prepaid: Vec<&AccountView> = views.iter().filter(|v| v.prepaid()).collect();
    if prepaid.is_empty() {
        return Vec::new();
    }
    let parts: Vec<String> = prepaid
        .iter()
        .map(|v| match v.source {
            Source::Unknown => format!("{} ?", v.account),
            Source::Live => format!("{}{} {} {}", glyph(v.level), v.account, balance_label(v), live_label(v, now)),
            _ => format!("{}{} {} est.", glyph(v.level), v.account, balance_label(v)),
        })
        .collect();
    let mut out = vec![format!("info: prepaid balance — {} (not part of the verdict)", parts.join(", "))];
    for v in prepaid {
        if let Some(gap) = unattributed(v) {
            out.push(format!(
                "info: {} reports {}/h more than arena lists (network volumes, stopped pods' disks, serverless?)",
                Account::from_name(&v.account).map_or("the provider", Account::title),
                usd(gap)
            ));
        }
    }
    out
}

/// What `arena balance set` prints: old → new, then the account as the footers will now
/// show it — and, while a live reading younger than [`FRESH`] exists, that live wins: a
/// manual value is used only while live is unavailable.
pub fn set_lines(old: Option<Anchor>, state: &BalanceState, account: Account, key_fp: &str, policy: &BalancePolicy, now: u64) -> Vec<String> {
    let was = match old {
        Some(a) => format!(
            "{} ({}, {} ago)",
            usd(a.usd),
            if a.source == AnchorSource::Live { "live" } else { "manual" },
            fmt_age(now.saturating_sub(a.at))
        ),
        None => "no reading".into(),
    };
    let st = state.get(account, key_fp);
    let new = st.and_then(|s| s.anchor).map_or_else(|| "?".to_string(), |a| usd(a.usd));
    let mut out = vec![format!("{}: {was} → {new} (manual)", account.name())];
    let f = Fetched::nothing(account, key_fp);
    let v = views(state, std::slice::from_ref(&f), &Burns::new(), policy, Mode::Inline, now).remove(0);
    if v.source == Source::Unknown {
        // No burn known yet (no listing here): the next listing's burn counts it down.
        out.push(format!("now: {} ~{new} est. (manual) — counted down at the fleet's burn from the next listing", account.name()));
    } else {
        out.push(format!("now: {}", footer_part(&v, policy, now)));
    }
    if let Some(p) = st.and_then(|s| s.fresh_facts(now)) {
        out.push(format!("note: live is {}; a manual value is used only while live is unavailable", usd(p.usd)));
    }
    out
}

/// `24`→`1d`, `72`→`3d`, `30`→`30h`.
fn fmt_threshold(h: f64) -> String {
    if h > 0.0 && h % 24.0 == 0.0 {
        format!("{}d", num(h / 24.0))
    } else {
        format!("{}h", num(h))
    }
}

/// `config check`'s "Balance (`arena balance`)" rows, from the state file alone (no
/// request): the threshold, then per configured account how its sources stand. A ⚠ row
/// is advice, never a missing key.
pub fn config_rows(cfg: &Config, state: &BalanceState, accounts: &[(Account, String)], now: u64) -> Vec<String> {
    let mut out = Vec::new();
    let key = "BALANCE_WARN_HOURS";
    match warn_hours(cfg) {
        Ok(h) => {
            let default = if cfg.get(key).is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" };
            let what = if h == 0.0 { "runway warnings off".to_string() } else { format!("warn below {} of runway", fmt_threshold(h)) };
            out.push(format!("  ✓ {key:<26} {}{default} — {what}", num(h)));
        }
        Err(e) => out.push(format!("  ✗ {key:<26} {e} — {} is used", num(DEFAULT_WARN_HOURS))),
    }
    for (a, fp) in accounts {
        let name = a.name();
        let Some(st) = state.get(*a, fp) else {
            out.push(format!("  · {name:<26} no reading yet — run `arena balance`"));
            continue;
        };
        match (&st.live_fail, st.live_ok_at) {
            (Some(f), _) if noted(f, now) => out.push(format!(
                "  ⚠ {name:<26} live balance failing since {} ({}) — estimates in use",
                date(f.since),
                fail_text(f)
            )),
            (Some(f), _) => out.push(format!(
                "  · {name:<26} live balance unavailable for {} ({}) — estimates in use",
                fmt_age(now.saturating_sub(f.since)),
                f.kind.label()
            )),
            (None, Some(t)) => {
                let check = match &st.check {
                    Some(c) if c.verdict == Verdict::Safe => {
                        format!(" · fallback check ✓ over {} ({})", fmt_age(c.horizon_secs), check_words(c))
                    }
                    _ => String::new(),
                };
                out.push(format!("  ✓ {name:<26} live {} ago{check}", fmt_age(now.saturating_sub(t))));
            }
            (None, None) => match st.anchor {
                Some(a) if a.source == AnchorSource::Manual => out.push(format!(
                    "  · {name:<26} manual {} set {} ago — no live reading yet",
                    usd(a.usd),
                    fmt_age(now.saturating_sub(a.at))
                )),
                _ => out.push(format!("  · {name:<26} no reading yet — run `arena balance`")),
            },
        }
        if let Some(f) = &st.live_reduced {
            out.push(format!(
                "  ⚠ {name:<26} full balance query rejected since {} — live via the minimal query (no burn/limit/underBalance)",
                date(f.since)
            ));
        }
        if let Some(c) = st.check.filter(|c| c.verdict == Verdict::Overstates) {
            out.push(format!(
                "  ⚠ {name:<26} fallback check: the estimate {} over {} — don't rely on estimates",
                check_words(&c),
                fmt_age(c.horizon_secs)
            ));
        }
        if let Some(f) = &st.billing_fail {
            out.push(format!(
                "  ⚠ {name:<26} billing history failing ({}) — estimates use arena's own metering only",
                fail_text(f)
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// The state file
// ---------------------------------------------------------------------------------------

/// Parse `balance.json`. `Err` (with why) for anything that isn't a version-1 file; samples
/// are put back in time order (a hand-edited file must not break metering).
pub fn parse_state(text: &str) -> Result<BalanceState, String> {
    let mut s: BalanceState = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if s.version != STATE_VERSION {
        return Err(format!("unsupported version {} (expected {STATE_VERSION})", s.version));
    }
    for st in s.accounts.values_mut() {
        st.samples.retain(|(_, b)| b.is_finite() && *b >= 0.0);
        st.samples.sort_by_key(|s| s.0);
        st.samples.dedup_by_key(|s| s.0);
    }
    Ok(s)
}

/// Load tolerantly: missing → empty (normal before the first reading); unreadable, corrupt
/// or a newer version → empty plus one warning line (shown only by `arena balance`). Never
/// an error: live readings work without it, and the next write replaces it.
pub fn load(path: &Path) -> (BalanceState, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_state(&text) {
            Ok(s) => (s, None),
            Err(e) => (
                BalanceState::default(),
                Some(format!("warning: ignoring the balance state {} ({e}) — the next reading replaces it", path.display())),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (BalanceState::default(), None),
        Err(e) => (BalanceState::default(), Some(format!("warning: can't read the balance state {} ({e})", path.display()))),
    }
}

/// [`load`] when there is a path; empty otherwise (no state directory, or a test).
pub fn load_opt(path: Option<&Path>) -> (BalanceState, Option<String>) {
    path.map_or_else(|| (BalanceState::default(), None), load)
}

/// The file's text (pretty, one trailing newline).
pub fn to_json(state: &BalanceState) -> String {
    // Plain structs and maps with string keys: serializing can't fail.
    let mut s = serde_json::to_string_pretty(state).unwrap_or_default();
    s.push('\n');
    s
}

/// Write the state to `path` atomically, owner-only, in an owner-only directory.
pub fn save(state: &BalanceState, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        crate::snapshot::create_private_dir(dir)?;
    }
    crate::snapshot::write_atomic(path, to_json(state).as_bytes(), 0o600)
}

/// Record into the state file at `path`: reload it under an exclusive `flock` on the state
/// directory (the lock the health cache takes too), apply `f`, save atomically (0600). So
/// the TUI and a CLI recording at once merge rather than overwrite each other — at worst
/// one sample is lost. `Ok` carries the state written and the load warning, if the old
/// file had to be ignored.
pub fn record(path: &Path, f: impl FnOnce(BalanceState) -> BalanceState) -> std::io::Result<(BalanceState, Option<String>)> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir)?;
    // Best effort: a filesystem without flock still gets the atomic write.
    let lock = std::fs::File::open(dir).ok().filter(|f| f.lock().is_ok());
    let (state, warning) = load(path);
    let next = f(state);
    let saved = save(&next, path);
    drop(lock);
    saved.map(|()| (next, warning))
}

/// `arena balance set`: a manual anchor for `account`, scoped by the key's fingerprint (an
/// entry for another key is replaced, never edited). Returns the anchor it replaced, for
/// "old → new". The next live reading replaces it in turn.
pub fn record_manual(
    path: &Path,
    account: Account,
    key_fp: &str,
    usd: f64,
    now: u64,
) -> std::io::Result<(Option<Anchor>, BalanceState, Option<String>)> {
    let mut old = None;
    let (state, warning) = record(path, |mut s| {
        s.version = STATE_VERSION;
        let st = s.accounts.entry(account).or_insert_with(|| AccountState::new(key_fp));
        if st.key_fp != key_fp {
            *st = AccountState::new(key_fp);
        }
        old = st.anchor;
        st.anchor = Some(Anchor { usd, at: now, source: AnchorSource::Manual });
        st.billed_max = None;
        s
    })?;
    Ok((old, state, warning))
}

/// After an observation: record it (modes that write, with a state file) and return the
/// state to build views from. Without a state file, or when the write fails, it is applied
/// in memory — the surface still shows what it saw — and the second value is the warning
/// (which only `arena balance` prints: everywhere else a balance problem is silent).
pub fn commit(
    path: Option<&Path>,
    loaded: BalanceState,
    fetched: &[Fetched],
    burns: &Burns,
    mode: Mode,
    now: u64,
) -> (BalanceState, Option<String>) {
    if !mode.writes() {
        return (loaded, None);
    }
    let Some(path) = path else {
        return (apply(loaded, fetched, burns, mode, now), None);
    };
    match record(path, |s| apply(s, fetched, burns, mode, now)) {
        Ok(done) => done,
        Err(e) => (
            apply(loaded, fetched, burns, mode, now),
            Some(format!("warning: couldn't save the balance state {}: {e}", path.display())),
        ),
    }
}

// ---------------------------------------------------------------------------------------
// The balance API
// ---------------------------------------------------------------------------------------

/// The reads, behind a seam: [`HttpBalanceApi`] for real, [`FakeBalanceApi`] in tests.
#[async_trait]
pub trait BalanceApi: Send + Sync {
    /// The prepaid accounts with a configured key, and their fingerprints. Makes no request.
    fn accounts(&self) -> Vec<(Account, String)>;
    /// One live reading. RunPod: the full query, then the minimal one when a field was
    /// dropped (`reduced`). (A future REST balance endpoint goes here, tried first — the
    /// chain doesn't change.)
    async fn live(&self, account: Account) -> Result<Live, Fail>;
    /// RunPod's hourly billing history from `from_hour` to `to`.
    async fn billed(&self, from_hour: u64, to: u64) -> Result<Billed, Fail>;
}

/// The real reads: one GraphQL *query* (never a mutation) and two GETs, the keys in the
/// bearer header only, to fixed hosts — redirecting a key elsewhere is never configurable.
pub struct HttpBalanceApi {
    client: Client,
    runpod_key: Option<String>,
    vast_key: Option<String>,
    graphql_url: String,
    v2_base: String,
    vast_base: String,
}

impl HttpBalanceApi {
    pub fn from_config(cfg: &Config) -> Self {
        let key = |a: Account| cfg.get(a.key_name()).map(str::trim).filter(|k| !k.is_empty()).map(String::from);
        HttpBalanceApi {
            client: Client::new(),
            runpod_key: key(Account::Runpod),
            vast_key: key(Account::Vast),
            graphql_url: crate::provider::runpod::GRAPHQL.to_string(),
            v2_base: crate::provider::runpod_v2::BASE.to_string(),
            vast_base: crate::provider::vast::BASE.to_string(),
        }
    }

    /// The same reads against loopback test servers (one per endpoint, so concurrent
    /// requests can't take each other's canned answers).
    #[cfg(test)]
    fn with_bases(graphql_url: &str, v2_base: &str, vast_base: &str, runpod_key: Option<&str>, vast_key: Option<&str>) -> Self {
        HttpBalanceApi {
            client: crate::http::test_server::client(),
            runpod_key: runpod_key.map(String::from),
            vast_key: vast_key.map(String::from),
            graphql_url: graphql_url.into(),
            v2_base: v2_base.into(),
            vast_base: vast_base.into(),
        }
    }

    fn key(&self, a: Account) -> Option<&str> {
        match a {
            Account::Runpod => self.runpod_key.as_deref(),
            Account::Vast => self.vast_key.as_deref(),
        }
    }

    async fn runpod_myself(&self, key: &str, minimal: bool) -> Result<Live, Fail> {
        let rb = crate::provider::runpod::balance_request(&self.client, &self.graphql_url, key, minimal);
        fetch(rb).await.and_then(|v| parse_runpod_myself(&v))
    }
}

/// Send, then [`classify`]. A transport failure is transient, in our words (reqwest's own
/// message names the URL — no key there, but no reason to print it either).
async fn fetch(rb: reqwest::RequestBuilder) -> Result<Value, Fail> {
    match crate::http::send_text(rb).await {
        Ok((status, text)) => classify(status.as_u16(), &text),
        Err(_) => Err(Fail::transient("no answer (network error)")),
    }
}

#[async_trait]
impl BalanceApi for HttpBalanceApi {
    fn accounts(&self) -> Vec<(Account, String)> {
        Account::ALL.into_iter().filter_map(|a| self.key(a).map(|k| (a, key_fp(k)))).collect()
    }

    async fn live(&self, account: Account) -> Result<Live, Fail> {
        let Some(key) = self.key(account) else {
            return Err(Fail::new(FailKind::Auth, None, "no API key configured"));
        };
        match account {
            Account::Runpod => match self.runpod_myself(key, false).await {
                Err(f) if wants_minimal_retry(&f) => {
                    self.runpod_myself(key, true).await.map(|l| Live { reduced: true, ..l })
                }
                r => r,
            },
            Account::Vast => {
                let rb = crate::provider::vast::user_request(&self.client, &self.vast_base, key);
                // Vast's body is never quoted: an error status keeps its kind and code only.
                fetch(rb)
                    .await
                    .map_err(|f| match f.http {
                        Some(h) if !(200..300).contains(&h) => Fail { detail: None, ..f },
                        _ => f,
                    })
                    .and_then(|v| parse_vast_user(&v))
            }
        }
    }

    async fn billed(&self, from_hour: u64, to: u64) -> Result<Billed, Fail> {
        let Some(key) = self.runpod_key.as_deref() else {
            return Err(Fail::new(FailKind::Auth, None, "no API key configured"));
        };
        let rb = crate::provider::runpod_v2::billing_request(&self.client, &self.v2_base, key, from_hour, to);
        fetch(rb).await.and_then(|v| parse_billing(&v))
    }
}

// ---------------------------------------------------------------------------------------
// Observing
// ---------------------------------------------------------------------------------------

/// Whether an inline surface should try live now: never tried, the last attempt (success
/// or failure) at least [`FRESH`] ago, or one from the future (the clock went back).
fn live_due(st: Option<&AccountState>, now: u64) -> bool {
    match st.and_then(AccountState::last_live_attempt) {
        None => true,
        Some(t) => t > now.saturating_add(CLOCK_SKEW) || now.saturating_sub(t) >= FRESH,
    }
}

/// Where RunPod's billing history is needed from: the hour of the usable anchor — and, for
/// `arena balance`, of the check anchor too. `None`: nothing to count down from.
fn billing_from(account: Account, st: Option<&AccountState>, mode: Mode, now: u64) -> Option<u64> {
    if account != Account::Runpod {
        return None;
    }
    let st = st?;
    let check = if mode == Mode::Full { st.check_anchor } else { None };
    [st.anchor, check].into_iter().flatten().filter(|a| a.usable(now)).map(|a| a.at.min(now)).min().map(floor_hour)
}

/// `fut`, or a transient failure once `deadline` passes.
async fn within<T>(deadline: tokio::time::Instant, fut: impl std::future::Future<Output = Result<T, Fail>>) -> Result<T, Fail> {
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(r) => r,
        Err(_) => Err(Fail::transient("no answer in time")),
    }
}

/// Observe every configured account at once, the whole of it within `limit`, as `mode`
/// says: **inline** — live only when the last attempt is [`FRESH`] old, billing only while
/// live is failing (alongside it when it was already failing, after it when it just
/// failed); **full** — live, with billing alongside whenever there is an anchor; **no-live**
/// — billing only; **cache-only** — nothing (configured accounts, nothing fetched). Never
/// fails: whatever didn't answer in time is a transient failure.
pub async fn observe(api: &dyn BalanceApi, state: &BalanceState, mode: Mode, limit: std::time::Duration, now: u64) -> Vec<Fetched> {
    let deadline = tokio::time::Instant::now() + limit;
    let accounts = api.accounts();
    let one = |a: Account| {
        let found = accounts.iter().find(|(x, _)| *x == a).cloned();
        async move {
            let (a, fp) = found?;
            let st = state.get(a, &fp);
            Some(observe_one(api, a, fp.clone(), st, mode, deadline, now).await)
        }
    };
    let (runpod, vast) = tokio::join!(one(Account::Runpod), one(Account::Vast));
    [runpod, vast].into_iter().flatten().collect()
}

/// [`observe`] for one account (the `create`/`up` preflight, which concerns only the target
/// provider's): `None` when it isn't configured.
pub async fn observe_account(
    api: &dyn BalanceApi,
    state: &BalanceState,
    account: Account,
    mode: Mode,
    limit: std::time::Duration,
    now: u64,
) -> Option<Fetched> {
    let deadline = tokio::time::Instant::now() + limit;
    let (a, fp) = api.accounts().into_iter().find(|(a, _)| *a == account)?;
    let st = state.get(a, &fp);
    Some(observe_one(api, a, fp.clone(), st, mode, deadline, now).await)
}

async fn observe_one(
    api: &dyn BalanceApi,
    account: Account,
    key_fp: String,
    st: Option<&AccountState>,
    mode: Mode,
    deadline: tokio::time::Instant,
    now: u64,
) -> Fetched {
    let mut f = Fetched::nothing(account, key_fp);
    let from = billing_from(account, st, mode, now);
    let live = || within(deadline, api.live(account));
    let billed = |from: u64| within(deadline, api.billed(from, now));
    match mode {
        Mode::CacheOnly => {}
        Mode::NoLive => {
            if let Some(from) = from {
                f.billed = Some(billed(from).await);
            }
        }
        Mode::Full => match from {
            Some(from) => {
                let (l, b) = tokio::join!(live(), billed(from));
                (f.live, f.billed) = (Some(l), Some(b));
            }
            None => f.live = Some(live().await),
        },
        Mode::Inline => {
            let failing = st.is_some_and(|s| s.live_fail.is_some());
            match (live_due(st, now), from) {
                // Within FRESH of a failed attempt: the estimate needs billing, not live.
                (false, Some(from)) if failing => f.billed = Some(billed(from).await),
                (false, _) => {}
                (true, Some(from)) if failing => {
                    let (l, b) = tokio::join!(live(), billed(from));
                    (f.live, f.billed) = (Some(l), Some(b));
                }
                (true, from) => {
                    let l = live().await;
                    let failed = l.is_err();
                    f.live = Some(l);
                    if let (true, Some(from)) = (failed, from) {
                        if tokio::time::Instant::now() < deadline {
                            f.billed = Some(billed(from).await);
                        }
                    }
                }
            }
        }
    }
    f
}

// ---------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fake::FakeBalanceApi;

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Clone)]
    enum Answer<T> {
        Give(Result<T, Fail>),
        Stall,
    }

    /// A scripted [`BalanceApi`] for tests (here and, via the `test-util` feature, in the CLI
    /// and TUI): standing answers per account, call counts — and [`FakeBalanceApi::panicking`]
    /// for surfaces that must make no call at all.
    #[derive(Default)]
    pub struct FakeBalanceApi {
        accounts: Vec<(Account, String)>,
        live: Mutex<BTreeMap<Account, Answer<Live>>>,
        billed: Mutex<Option<Answer<Billed>>>,
        live_calls: AtomicUsize,
        billed_calls: AtomicUsize,
        billed_args: Mutex<Vec<(u64, u64)>>,
        panics: bool,
    }

    impl FakeBalanceApi {
        /// No account configured.
        pub fn new() -> Self {
            Self::default()
        }

        /// The fingerprint a fake account's key has.
        pub fn fp(account: Account) -> String {
            key_fp(&format!("fake-{}-key", account.name()))
        }

        /// `account` configured (its live answer unscripted: a transient failure).
        pub fn with(mut self, account: Account) -> Self {
            if !self.accounts.iter().any(|(a, _)| *a == account) {
                self.accounts.push((account, Self::fp(account)));
                self.accounts.sort();
            }
            self
        }

        fn answer_live(self, account: Account, a: Answer<Live>) -> Self {
            self.live.lock().unwrap().insert(account, a);
            self.with(account)
        }

        pub fn live_ok(self, account: Account, live: Live) -> Self {
            self.answer_live(account, Answer::Give(Ok(live)))
        }

        pub fn live_err(self, account: Account, fail: Fail) -> Self {
            self.answer_live(account, Answer::Give(Err(fail)))
        }

        /// Live never answers.
        pub fn live_stall(self, account: Account) -> Self {
            self.answer_live(account, Answer::Stall)
        }

        pub fn billed_ok(self, b: Billed) -> Self {
            *self.billed.lock().unwrap() = Some(Answer::Give(Ok(b)));
            self
        }

        pub fn billed_err(self, fail: Fail) -> Self {
            *self.billed.lock().unwrap() = Some(Answer::Give(Err(fail)));
            self
        }

        pub fn billed_stall(self) -> Self {
            *self.billed.lock().unwrap() = Some(Answer::Stall);
            self
        }

        /// Any live or billing call panics: the surface must make none.
        pub fn panicking(mut self) -> Self {
            self.panics = true;
            self
        }

        pub fn live_calls(&self) -> usize {
            self.live_calls.load(Ordering::SeqCst)
        }

        pub fn billed_calls(&self) -> usize {
            self.billed_calls.load(Ordering::SeqCst)
        }

        /// Each billing call's `(from_hour, to)`.
        pub fn billed_args(&self) -> Vec<(u64, u64)> {
            self.billed_args.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl BalanceApi for FakeBalanceApi {
        fn accounts(&self) -> Vec<(Account, String)> {
            self.accounts.clone()
        }

        async fn live(&self, account: Account) -> Result<Live, Fail> {
            assert!(!self.panics, "this surface must make no balance call (live {account:?})");
            self.live_calls.fetch_add(1, Ordering::SeqCst);
            let answer = self.live.lock().unwrap().get(&account).cloned();
            match answer {
                Some(Answer::Give(r)) => r,
                Some(Answer::Stall) => std::future::pending().await,
                None => Err(Fail::transient("no answer scripted")),
            }
        }

        async fn billed(&self, from_hour: u64, to: u64) -> Result<Billed, Fail> {
            assert!(!self.panics, "this surface must make no balance call (billing)");
            self.billed_calls.fetch_add(1, Ordering::SeqCst);
            self.billed_args.lock().unwrap().push((from_hour, to));
            let answer = self.billed.lock().unwrap().clone();
            match answer {
                Some(Answer::Give(r)) => r,
                Some(Answer::Stall) => std::future::pending().await,
                None => Err(Fail::transient("no answer scripted")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Listing;
    use serde_json::json;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z
    const H: u64 = 3_600;
    const FP: &str = "9c3e01ab";

    fn fixture(name: &str) -> Value {
        let text = match name {
            "runpod_myself" => include_str!("provider/fixtures/balance/runpod_myself.json"),
            "runpod_myself_unknown_field" => include_str!("provider/fixtures/balance/runpod_myself_unknown_field.json"),
            "runpod_myself_null" => include_str!("provider/fixtures/balance/runpod_myself_null.json"),
            "billing_hourly_openapi" => include_str!("provider/fixtures/balance/billing_hourly_openapi.json"),
            "vast_user_canary" => include_str!("provider/fixtures/balance/vast_user_canary.json"),
            _ => panic!("no fixture {name}"),
        };
        serde_json::from_str(text).unwrap()
    }

    /// The canary values planted in `vast_user_canary.json`.
    pub(super) const CANARIES: [&str; 6] = [
        "CANARYVASTKEY0123456789abcdef",
        "canary.owner@example.invalid",
        "CANARYSESSIONID",
        "Canary Street",
        "CANARYSSHKEY",
        "4242424",
    ];

    fn live(usd: f64, rate: Option<f64>) -> Live {
        Live { usd, provider_burn_per_hr: rate, spend_limit_per_hr: None, under_balance: None, reduced: false }
    }

    fn fetched(account: Account, live: Option<Result<Live, Fail>>, billed: Option<Result<Billed, Fail>>) -> Fetched {
        Fetched { account, key_fp: FP.into(), live, billed }
    }

    fn burn(per_hr: f64) -> Option<Burn> {
        Some(Burn { per_hr, unpriced: 0, stale: false })
    }

    fn burns_of(runpod: Option<Burn>, vast: Option<Burn>) -> Burns {
        [(Account::Runpod, runpod), (Account::Vast, vast)].into_iter().collect()
    }

    fn gone() -> Fail {
        Fail::new(FailKind::Gone, Some(404), "endpoint gone")
    }

    fn policy() -> BalancePolicy {
        BalancePolicy::default()
    }

    fn pod(provider: &str, status: &str, cost: Option<f64>) -> Pod {
        Pod { provider: provider.into(), status: status.into(), cost_per_hr: cost, ..Default::default() }
    }

    fn listing(results: Vec<(&str, std::result::Result<Vec<Pod>, &str>)>) -> Listing {
        Listing::from_results(results.into_iter().map(|(p, r)| (p.to_string(), r.map_err(crate::Error::provider))).collect())
    }

    /// An account with a live anchor `age` seconds old (no samples).
    fn anchored(usd: f64, age: u64, source: AnchorSource) -> AccountState {
        AccountState { key_fp: FP.into(), anchor: Some(Anchor { usd, at: NOW - age, source }), ..Default::default() }
    }

    fn state_with(st: AccountState) -> BalanceState {
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Runpod, st);
        s
    }

    fn one_view(state: &BalanceState, f: &Fetched, b: Option<Burn>, mode: Mode, now: u64) -> AccountView {
        views(state, std::slice::from_ref(f), &burns_of(b, None), &policy(), mode, now).remove(0)
    }

    #[test]
    fn key_fp_vectors() {
        assert_eq!(key_fp(""), "811c9dc5");
        assert_eq!(key_fp("a"), "e40c292c");
        assert_eq!(key_fp("foobar"), "bf9cf968");
        assert_ne!(key_fp("rpa_one"), key_fp("rpa_two"));
        // Configured accounts: only non-empty keys, trimmed before fingerprinting.
        let cfg = Config::parse("RUNPOD_API_KEY=\" a \"\nVAST_API_KEY=\"\"\nHETZNER_API_KEY=h\n");
        assert_eq!(configured(&cfg), vec![(Account::Runpod, "e40c292c".to_string())]);
        assert_eq!(Account::from_name("VAST"), Some(Account::Vast));
        assert_eq!(Account::from_name("hetzner"), None);
    }

    #[test]
    fn amounts_for_balance_set_are_finite_and_in_range() {
        assert_eq!(parse_amount("250"), Ok(250.0));
        assert_eq!(parse_amount("$31.20"), Ok(31.2));
        assert_eq!(parse_amount("-12.5"), Ok(-12.5));
        for bad in ["NaN", "inf", "-inf", "2e7", "-2e5", "lots", ""] {
            assert!(parse_amount(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_runpod_myself_table() {
        // The fixture: numbers as strings (`loose_f64`).
        let l = parse_runpod_myself(&fixture("runpod_myself")).unwrap();
        assert_eq!(
            l,
            Live { usd: 32.54, provider_burn_per_hr: Some(0.47), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false }
        );
        // (case, body, Ok(usd) | Err(detail needle))
        let cases: Vec<(&str, Value, std::result::Result<f64, &str>)> = vec![
            ("numbers", json!({"data": {"myself": {"clientBalance": 5, "currentSpendPerHr": 0, "underBalance": true}}}), Ok(5.0)),
            ("missing balance", json!({"data": {"myself": {"currentSpendPerHr": 1}}}), Err("clientBalance is missing")),
            ("null myself", fixture("runpod_myself_null"), Err("data.myself is null")),
            ("no data", json!({}), Err("no data.myself")),
            ("not an object", json!([1, 2]), Err("no data.myself")),
            ("NaN", json!({"data": {"myself": {"clientBalance": "NaN"}}}), Err("out of range")),
            ("1e12", json!({"data": {"myself": {"clientBalance": 1e12}}}), Err("out of range")),
            ("words", json!({"data": {"myself": {"clientBalance": "lots"}}}), Err("missing or not a number")),
            ("unknown field", fixture("runpod_myself_unknown_field"), Err("Cannot query field \"spendLimit\"")),
            // A balance with an error about another field: the balance still counts.
            ("errors+data", json!({"data": {"myself": {"clientBalance": 7}}, "errors": [{"message": "spendLimit: denied"}]}), Ok(7.0)),
        ];
        for (case, body, want) in cases {
            match (parse_runpod_myself(&body), want) {
                (Ok(l), Ok(usd)) => assert_eq!(l.usd, usd, "{case}"),
                (Err(f), Err(needle)) => {
                    assert_eq!(f.kind, FailKind::Drift, "{case}");
                    assert!(f.detail.as_deref().unwrap_or("").contains(needle), "{case}: {f:?}");
                }
                (got, want) => panic!("{case}: got {got:?}, want {want:?}"),
            }
        }
        // Rates: a negative or junk rate is unknown, a zero spend limit is none.
        let l = parse_runpod_myself(&json!({"data": {"myself": {"clientBalance": 1, "currentSpendPerHr": -3, "spendLimit": 0}}})).unwrap();
        assert_eq!((l.provider_burn_per_hr, l.spend_limit_per_hr, l.under_balance), (None, None, None));
        // The unknown-field error is exactly what makes the minimal retry.
        assert!(wants_minimal_retry(&parse_runpod_myself(&fixture("runpod_myself_unknown_field")).unwrap_err()));
        assert!(!wants_minimal_retry(&parse_runpod_myself(&fixture("runpod_myself_null")).unwrap_err()));
    }

    #[test]
    fn parse_vast_user_reads_credit_only() {
        let body = fixture("vast_user_canary");
        assert_eq!(parse_vast_user(&body).unwrap(), live(136.87, None));
        // `balance` (or anything else) is never read as the balance.
        let mut no_credit = body.clone();
        no_credit.as_object_mut().unwrap().remove("credit");
        for bad in [no_credit, json!({"credit": "much"}), json!({"credit": 1e9}), json!("nope"), json!({"user": body.clone()})] {
            let f = parse_vast_user(&bad).unwrap_err();
            assert_eq!(f.kind, FailKind::Drift);
            let shown = format!("{f:?}");
            assert!(shown.contains("no numeric `credit`"), "{shown}");
            for c in CANARIES {
                assert!(!shown.contains(c), "{c} leaked: {shown}");
            }
        }
    }

    #[test]
    fn parse_billing_table() {
        let b = parse_billing(&fixture("billing_hourly_openapi")).unwrap();
        assert_eq!((b.from, b.to), (NOW - 2 * H, NOW + H));
        assert_eq!(b.buckets, vec![(NOW - 2 * H, 0.47), (NOW - H, 0.52), (NOW, 0.0)]);
        // Since a time inside the first hour: that whole hour counts.
        assert!((billed_since(&b, NOW - 2 * H + 1_800).unwrap() - 0.99).abs() < 1e-9);
        assert!((billed_since(&b, NOW - H + 5).unwrap() - 0.52).abs() < 1e-9);
        // A time before the window: the history can't say.
        assert_eq!(billed_since(&b, NOW - 3 * H), None);
        // Empty records: nothing spent (a valid answer).
        let mut empty = fixture("billing_hourly_openapi");
        empty["records"] = json!([]);
        assert_eq!(parse_billing(&empty).unwrap().buckets, vec![]);
        // (case, mutation, detail needle)
        let bad: Vec<(&str, Box<dyn Fn(&mut Value)>, &str)> = vec![
            ("empty object", Box::new(|v: &mut Value| *v = json!({})), "no metadata.query"),
            ("negative", Box::new(|v: &mut Value| v["records"][1]["totalAmount"] = json!(-0.5)), "negative"),
            ("not a number", Box::new(|v: &mut Value| v["records"][0]["totalAmount"] = json!("x")), "totalAmount"),
            ("day buckets", Box::new(|v: &mut Value| v["metadata"]["query"]["bucketSize"] = json!("day")), "aren't hourly"),
            ("no records", Box::new(|v: &mut Value| { v.as_object_mut().unwrap().remove("records"); }), "no records"),
            ("bad time", Box::new(|v: &mut Value| v["records"][0]["startTime"] = json!("yesterday")), "startTime"),
            ("backwards window", Box::new(|v: &mut Value| v["metadata"]["query"]["endTime"] = json!("2026-10-01T00:00:00Z")), "ends before"),
        ];
        for (case, mutate, needle) in bad {
            let mut v = fixture("billing_hourly_openapi");
            mutate(&mut v);
            let f = parse_billing(&v).unwrap_err();
            assert!(f.kind == FailKind::Drift && f.detail.as_deref().unwrap().contains(needle), "{case}: {f:?}");
        }
    }

    #[test]
    fn classify_table() {
        use FailKind::*;
        let gql_err = r#"{"errors":[{"message":"Cannot query field \"clientBalance\" on type \"User\"."}]}"#;
        // (status, body, Ok? / kind, detail needle)
        let cases: Vec<(u16, &str, Option<FailKind>, Option<&str>)> = vec![
            (200, r#"{"data":{}}"#, None, None),
            (200, "<html>maintenance</html>", Some(Gone), Some("isn't JSON")),
            (200, "  ", Some(Drift), Some("empty")),
            (400, gql_err, Some(Drift), Some("Cannot query field")),
            (401, "<html>401 Authorization Required</html>", Some(Auth), None),
            (403, r#"{"error":"forbidden"}"#, Some(Auth), None),
            (404, "Not Found", Some(Gone), None),
            (405, "", Some(Gone), None),
            (410, r#"{"title":"Gone"}"#, Some(Gone), None),
            (418, "teapot", Some(Drift), None),
            (429, "slow down", Some(Transient), None),
            (500, "", Some(Transient), None),
            (502, "<html>Bad Gateway</html>", Some(Transient), None),
            (503, r#"{"errors":[{"message":"try later"}]}"#, Some(Transient), Some("try later")),
        ];
        for (status, body, kind, needle) in cases {
            match (classify(status, body), kind) {
                (Ok(_), None) => {}
                (Err(f), Some(k)) => {
                    assert_eq!((f.kind, f.http), (k, Some(status)), "{status} {body}");
                    match needle {
                        Some(n) => assert!(f.detail.as_deref().unwrap_or("").contains(n), "{status}: {f:?}"),
                        // No GraphQL message: no detail at all — the body is never quoted.
                        None if status != 200 => assert_eq!(f.detail, None, "{status}: {f:?}"),
                        None => {}
                    }
                }
                (got, want) => panic!("{status}: got {got:?}, want {want:?}"),
            }
        }
        // A long GraphQL message is clipped.
        let long = json!({"errors": [{"message": "x".repeat(500)}]}).to_string();
        assert_eq!(classify(400, &long).unwrap_err().detail.unwrap().chars().count(), DETAIL_MAX + 1);
    }

    #[test]
    fn metered_spend_table() {
        // (case, anchor_at, samples, now, b_now, want USD)
        let cases: Vec<(&str, u64, Vec<(u64, f64)>, u64, f64, f64)> = vec![
            ("no samples: B over the whole span", NOW - 2 * H, vec![], NOW, 0.5, 1.0),
            ("constant", NOW - 2 * H, vec![(NOW - 3 * H, 1.0), (NOW - H, 1.0)], NOW, 1.0, 2.0),
            // A step up between samples is charged at the higher end for that segment.
            ("step up", NOW - 2 * H, vec![(NOW - 2 * H, 1.0), (NOW - H, 2.0)], NOW, 2.0, 1.0 * 2.0 + 1.0 * 2.0),
            ("step down", NOW - 2 * H, vec![(NOW - 2 * H, 2.0), (NOW - H, 1.0)], NOW, 1.0, 2.0 + 1.0),
            ("gap (one sample long ago)", NOW - 4 * H, vec![(NOW - 10 * H, 0.5)], NOW, 0.5, 2.0),
            ("anchor before the first sample", NOW - 2 * H, vec![(NOW - H, 1.0)], NOW, 1.0, 2.0),
            ("idle", NOW - 2 * H, vec![(NOW - 5 * H, 0.0)], NOW, 0.0, 0.0),
            ("clock backwards", NOW + 100, vec![(NOW - H, 1.0)], NOW, 1.0, 0.0),
            ("samples after now ignored", NOW - H, vec![(NOW + H, 9.0)], NOW, 1.0, 9.0),
        ];
        for (case, a, samples, now, b, want) in cases {
            let got = metered_spend(a, &samples, now, b);
            assert!((got - want).abs() < 1e-9, "{case}: {got} != {want}");
        }
    }

    #[test]
    fn burn_now_rules() {
        let listed = Some(Burn { per_hr: 0.4, unpriced: 1, stale: false });
        // (case, listed, provider rate, samples, want)
        let cases: Vec<(&str, Option<Burn>, Option<f64>, Vec<(u64, f64)>, Option<Burn>)> = vec![
            ("listing + provider: the higher", listed, Some(0.6), vec![], Some(Burn { per_hr: 0.6, unpriced: 0, stale: false })),
            ("listing above provider", listed, Some(0.1), vec![], Some(Burn { per_hr: 0.4, unpriced: 0, stale: false })),
            ("listing alone keeps its unpriced floor", listed, None, vec![], listed),
            ("listed nothing: a real zero", burn(0.0), None, vec![(NOW - H, 2.0)], burn(0.0)),
            ("no listing: the provider rate", None, Some(0.3), vec![(NOW - H, 2.0)], burn(0.3)),
            ("no listing, no rate: last sample, stale", None, None, vec![(NOW - 2 * H, 1.0), (NOW - H, 2.0)], Some(Burn { per_hr: 2.0, unpriced: 0, stale: true })),
            // Unknown is never zero.
            ("nothing at all: unknown", None, None, vec![], None),
        ];
        for (case, l, p, s, want) in cases {
            assert_eq!(burn_now(l, p, &s), want, "{case}");
        }
        // From a listing: per provider; a failed provider is unknown, never $0/h.
        let l = listing(vec![
            ("runpod", Ok(vec![pod("runpod", "RUNNING", Some(0.25)), pod("runpod", "EXITED", Some(1.0)), pod("runpod", "RUNNING", None)])),
            ("vast", Err("HTTP 429")),
        ]);
        let pods = l.pods();
        let b = burns(&l, &pods);
        assert_eq!(b[&Account::Runpod], Some(Burn { per_hr: 0.25, unpriced: 1, stale: false }));
        assert_eq!(b[&Account::Vast], None);
        let none = burns(&listing(vec![("runpod", Ok(vec![]))]), &[]);
        assert_eq!((none[&Account::Runpod], none[&Account::Vast]), (burn(0.0), None), "not queried is unknown too");
    }

    #[test]
    fn level_table() {
        let p = |h: f64| BalancePolicy { warn_hours: h };
        let b = |r: f64| Burn { per_hr: r, unpriced: 0, stale: false };
        let lv = |bal: Option<f64>, under: Option<bool>, burn: Option<Burn>, pol: f64| {
            let runway = bal.zip(burn).and_then(|(x, y)| runway(x, &y));
            level(bal, under, burn.as_ref(), runway, &p(pol))
        };
        // (case, balance, underBalance, burn/h, warn hours, want)
        let cases: Vec<(&str, Option<f64>, Option<bool>, Option<Burn>, f64, Level)> = vec![
            ("23.9h", Some(23.9), None, Some(b(1.0)), 72.0, Level::Critical),
            ("24h", Some(24.0), None, Some(b(1.0)), 72.0, Level::Low),
            ("71.9h", Some(71.9), None, Some(b(1.0)), 72.0, Level::Low),
            ("72h", Some(72.0), None, Some(b(1.0)), 72.0, Level::Ok),
            ("warn 0: no runway levels", Some(5.0), None, Some(b(1.0)), 0.0, Level::Ok),
            ("warn 0: ≤ 0 still critical", Some(0.0), None, Some(b(1.0)), 0.0, Level::Critical),
            ("warn 0: underBalance still critical", Some(500.0), Some(true), Some(b(1.0)), 0.0, Level::Critical),
            ("negative", Some(-3.0), None, Some(b(0.0)), 72.0, Level::Critical),
            ("underBalance", Some(500.0), Some(true), Some(b(0.1)), 72.0, Level::Critical),
            ("no burn", Some(10.0), None, Some(b(0.001)), 72.0, Level::Ok),
            ("burn unknown, balance known", Some(10.0), None, None, 72.0, Level::Ok),
            ("unknown while burning", None, None, Some(b(0.5)), 72.0, Level::Unknown),
            ("unknown, burn unknown", None, None, None, 72.0, Level::Unknown),
            ("unknown but idle", None, None, Some(b(0.0)), 72.0, Level::Ok),
            ("warn 12: critical stays at 24", Some(20.0), None, Some(b(1.0)), 12.0, Level::Critical),
        ];
        for (case, bal, under, burn, pol, want) in cases {
            assert_eq!(lv(bal, under, burn, pol), want, "{case}");
        }
    }

    #[test]
    fn fmt_runway_table() {
        for (h, want) in [(0.5, "<1h"), (6.66, "≈6.6h"), (31.9, "≈31h"), (47.99, "≈47h"), (74.4, "≈3.1d"), (1_439.0, "≈59.9d"), (2_000.0, ">60d")] {
            assert_eq!(fmt_hours(h), want, "{h}");
        }
        let v = |balance: Option<f64>, burn: Option<Burn>| {
            let mut v = AccountView::blank("runpod", "USD", Source::Live);
            v.balance = balance;
            v.burn = burn;
            v.runway_hours = balance.zip(burn).and_then(|(x, y)| runway(x, &y));
            v.runway_upper_bound = v.runway_hours.is_some() && burn.is_some_and(|b| b.floor());
            fmt_runway(&v)
        };
        assert_eq!(v(Some(74.4), burn(1.0)), "≈3.1d");
        assert_eq!(v(Some(10.0), burn(0.0)), "no burn");
        assert_eq!(v(Some(10.0), None), "?");
        assert_eq!(v(None, burn(1.0)), "?");
        assert_eq!(v(Some(74.4), Some(Burn { per_hr: 1.0, unpriced: 2, stale: false })), "≤3.1d");
        assert_eq!(v(Some(0.2), Some(Burn { per_hr: 1.0, unpriced: 0, stale: true })), "<1h");
    }

    #[test]
    fn policy_from_config() {
        let p = |t: &str| BalancePolicy::from_config(&Config::parse(t));
        assert_eq!(p(""), (BalancePolicy { warn_hours: 72.0 }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=\"\""), (BalancePolicy { warn_hours: 72.0 }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=0"), (BalancePolicy { warn_hours: 0.0 }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=1.5"), (BalancePolicy { warn_hours: 1.5 }, None));
        for bad in ["-1", "two days", "inf", "NaN"] {
            let (pol, warn) = p(&format!("BALANCE_WARN_HOURS=\"{bad}\""));
            assert_eq!(pol.warn_hours, 72.0, "{bad}: the default is used");
            assert!(warn.unwrap().starts_with("warning: BALANCE_WARN_HOURS must be a number of hours ≥ 0"), "{bad}");
        }
        assert!(p("BALANCE_WARN_HOURS=rpa_AAAAAAAAAAAAAAAA").1.unwrap().contains("a long value"), "a pasted key isn't echoed");
    }

    /// The tier table: what each situation shows.
    #[test]
    fn estimate_tiers_table() {
        let b1 = || burn(1.0);
        let ok = |l: Live| fetched(Account::Runpod, Some(Ok(l)), None);
        let none = || fetched(Account::Runpod, None, None);
        let failing = || fetched(Account::Runpod, Some(Err(gone())), None);
        let with_facts = |at: u64| AccountState {
            provider: Some(ProviderFacts { usd: 40.0, burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, at }),
            ..anchored(40.0, NOW - at, AnchorSource::Live)
        };
        let billing = |usd: f64| Billed { buckets: vec![(NOW - 2 * H, usd)], from: NOW - 3 * H, to: NOW + H };
        // (case, state, fetched, mode, want source, want balance, want method)
        let cases: Vec<(&str, AccountState, Fetched, Mode, Source, Option<f64>, Option<Method>)> = vec![
            ("live this run", AccountState::default(), ok(live(32.54, None)), Mode::Inline, Source::Live, Some(32.54), None),
            ("fresh cache", with_facts(NOW - 100), none(), Mode::Inline, Source::Live, Some(40.0), None),
            ("fresh cache, --no-live", with_facts(NOW - 100), none(), Mode::NoLive, Source::Estimate, Some(40.0 - 100.0 / 3_600.0), Some(Method::Metered)),
            ("cache too old", with_facts(NOW - FRESH), none(), Mode::Inline, Source::Estimate, Some(40.0 - FRESH as f64 / 3_600.0), Some(Method::Metered)),
            ("metered wins", anchored(50.0, 2 * H, AnchorSource::Live), fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing(0.5)))), Mode::Inline, Source::Estimate, Some(48.0), Some(Method::Metered)),
            ("billed wins", anchored(50.0, 2 * H, AnchorSource::Live), fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing(5.0)))), Mode::Inline, Source::Estimate, Some(45.0), Some(Method::Billed)),
            ("manual", anchored(250.0, H, AnchorSource::Manual), failing(), Mode::Inline, Source::Estimate, Some(249.0), Some(Method::Metered)),
            ("13.9 d", anchored(500.0, 13 * 86_400 + 21 * H, AnchorSource::Live), failing(), Mode::Inline, Source::Estimate, Some(500.0 - 333.0), Some(Method::Metered)),
            ("14.1 d", anchored(500.0, 14 * 86_400 + 3 * H, AnchorSource::Live), failing(), Mode::Inline, Source::Unknown, None, None),
            ("4 min in the future", AccountState { anchor: Some(Anchor { usd: 9.0, at: NOW + 240, source: AnchorSource::Live }), ..anchored(0.0, 0, AnchorSource::Live) }, failing(), Mode::Inline, Source::Estimate, Some(9.0), Some(Method::Metered)),
            ("6 min in the future", AccountState { anchor: Some(Anchor { usd: 9.0, at: NOW + 360, source: AnchorSource::Live }), ..anchored(0.0, 0, AnchorSource::Live) }, failing(), Mode::Inline, Source::Unknown, None, None),
            ("key mismatch", AccountState { key_fp: "deadbeef".into(), ..anchored(50.0, H, AnchorSource::Live) }, failing(), Mode::Inline, Source::Unknown, None, None),
        ];
        for (case, st, f, mode, source, balance, method) in cases {
            let v = one_view(&state_with(st), &f, b1(), mode, NOW);
            assert_eq!(v.source, source, "{case}: {v:?}");
            match (v.balance, balance) {
                (Some(x), Some(y)) => assert!((x - y).abs() < 1e-6, "{case}: {x} != {y}"),
                (x, y) => assert_eq!(x, y, "{case}"),
            }
            assert_eq!(v.method, method, "{case}");
        }
        // No burn known and no samples: even a good anchor can't be counted down.
        let v = one_view(&state_with(anchored(50.0, H, AnchorSource::Live)), &failing(), None, Mode::Inline, NOW);
        assert_eq!((v.source, v.level), (Source::Unknown, Level::Unknown));
        // A manual anchor is used only while live is unavailable: a live reading wins.
        let v = one_view(&state_with(anchored(250.0, 60, AnchorSource::Manual)), &ok(live(31.0, None)), b1(), Mode::Full, NOW);
        assert_eq!((v.source, v.balance), (Source::Live, Some(31.0)));
    }

    /// One scripted run: a live anchor, then live gone; observations at `times`, the listed
    /// burn changing at `changes` (each visible at the next observation), a listing that
    /// may fail, `unlisted` USD/h more that only billing sees (volumes, stopped disks), and
    /// billing as `billing` says. Returns `(time, estimate, truth)` per observation, through
    /// the real `apply` + `views`.
    fn simulate(
        changes: &[(u64, f64)],
        times: &[u64],
        list_fails: &[u64],
        unlisted: f64,
        billing: &dyn Fn(u64, &dyn Fn(u64, u64) -> f64) -> Option<std::result::Result<Billed, Fail>>,
    ) -> Vec<(u64, f64, f64)> {
        let start = NOW - 10 * H;
        let a0 = 100.0;
        let listed_rate = |t: u64| changes.iter().rev().find(|(c, _)| *c <= t).map_or(0.0, |c| c.1);
        let rate = |t: u64| listed_rate(t) + unlisted;
        // ∫ rate from t0 to t1, exactly (the rate is a step function).
        let spent = |t0: u64, t1: u64| -> f64 {
            let mut points = vec![t0];
            points.extend(changes.iter().map(|c| c.0).filter(|&c| c > t0 && c < t1));
            points.push(t1.max(t0));
            points.windows(2).map(|w| (w[1] - w[0]) as f64 / 3_600.0 * rate(w[0])).sum()
        };
        let listed = |t: u64| if list_fails.contains(&t) { None } else { burn(listed_rate(t)) };
        let mut state = BalanceState::default();
        let f0 = fetched(Account::Runpod, Some(Ok(live(a0, None))), None);
        state = apply(state, &[f0], &burns_of(listed(start), None), Mode::Inline, start);
        let mut out = Vec::new();
        for &t in times {
            let f = fetched(Account::Runpod, Some(Err(gone())), billing(t, &spent));
            let b = burns_of(listed(t), None);
            state = apply(state, std::slice::from_ref(&f), &b, Mode::Inline, t);
            let v = views(&state, &[f], &b, &policy(), Mode::Inline, t).remove(0);
            // Within FRESH of the anchor the reading is still shown as live; from then on
            // it is an estimate.
            if t - start < FRESH {
                assert_eq!(v.source, Source::Live, "at {t}");
                continue;
            }
            assert_eq!(v.source, Source::Estimate, "at {t}");
            out.push((t, v.balance.unwrap(), a0 - spent(start, t)));
        }
        out
    }

    /// Billing as RunPod would answer at `t`: hourly buckets from the anchor's hour, the
    /// last `lag_h` hours not yet billed, the in-progress bucket at `partial` of its spend.
    fn billing_at(t: u64, spent: &dyn Fn(u64, u64) -> f64, lag_h: u64, partial: f64) -> Billed {
        let from = floor_hour(NOW - 10 * H);
        let mut buckets = Vec::new();
        let mut h = from;
        while h <= floor_hour(t) {
            let end = (h + H).min(t);
            let usd = if h + H + lag_h * H <= t || lag_h == 0 {
                if end < h + H { spent(h, end) * partial } else { spent(h, end) }
            } else {
                0.0
            };
            buckets.push((h, usd));
            h += H;
        }
        Billed { buckets, from, to: floor_hour(t) + H }
    }

    fn scenarios() -> Vec<(&'static str, Box<dyn Fn(u64, &dyn Fn(u64, u64) -> f64) -> Option<std::result::Result<Billed, Fail>>>)> {
        vec![
            ("complete", Box::new(|t, s| Some(Ok(billing_at(t, s, 0, 1.0))))),
            ("lagging 2h", Box::new(|t, s| Some(Ok(billing_at(t, s, 2, 1.0))))),
            ("partial in-progress", Box::new(|t, s| Some(Ok(billing_at(t, s, 0, 0.5))))),
            ("missing", Box::new(|_, _| Some(Err(Fail::new(FailKind::Auth, Some(403), "x"))))),
            ("not fetched", Box::new(|_, _| None)),
            ("flapping", Box::new(|t, s| if (t / 600) % 2 == 0 { Some(Ok(billing_at(t, s, 1, 1.0))) } else { Some(Err(Fail::transient("5xx"))) })),
        ]
    }

    /// Burn histories: steps up and down between observations, idle spells, a failed
    /// listing (while the rate holds), irregular observation times.
    fn histories() -> Vec<(&'static str, Vec<(u64, f64)>, Vec<u64>, Vec<u64>)> {
        let start = NOW - 10 * H;
        let every = |step: u64| (1..=(9 * H / step)).map(|i| start + i * step).collect::<Vec<_>>();
        vec![
            ("constant", vec![(start - H, 0.5)], every(600), vec![]),
            (
                "steps between observations",
                vec![(start - H, 0.5), (start + 25 * 60, 2.0), (start + 2 * H + 7 * 60, 0.2), (start + 5 * H + 33 * 60, 1.3)],
                every(900),
                vec![],
            ),
            ("idle then busy", vec![(start - H, 0.0), (start + 3 * H + 60, 4.0)], every(1_200), vec![]),
            (
                "listing fails while the rate holds",
                vec![(start - H, 1.0), (start + 4 * H, 0.3)],
                every(600),
                vec![start + 600, start + 1_200, start + 3 * H, start + 5 * H, start + 5 * H + 600],
            ),
            // A rise too small for the drop rule (0.8 %, $0.004), then back, seconds apart: only
            // recording every rise keeps the second look from undercutting the first.
            ("a tiny rise and back", vec![(start - H, 0.5), (start + 1_800, 0.504), (start + 1_815, 0.5)], vec![start + 600, start + 1_810, start + 1_820, start + 3_000], vec![]),
            (
                "irregular looks",
                vec![(start - H, 0.7), (start + 2 * H, 0.9), (start + 6 * H, 0.1)],
                vec![start + 60, start + 61, start + 400, start + 2 * H + 1, start + 7 * H, start + 9 * H],
                vec![],
            ),
        ]
    }

    /// Invariant 5: on scripted burn histories with lagging, partial and missing billing,
    /// the estimate never exceeds the truth. Spend the listing doesn't show is covered only
    /// as far as billing is current — so with it, only complete billing is held to this
    /// (the documented limit: unlisted spend inside the billing lag goes unseen).
    #[test]
    fn estimate_never_exceeds_truth() {
        for (hist, changes, times, fails) in histories() {
            for (scen, billing) in scenarios() {
                let unlisted: &[f64] = if scen == "complete" { &[0.0, 0.2] } else { &[0.0] };
                for &extra in unlisted {
                    for (t, est, truth) in simulate(&changes, &times, &fails, extra, billing.as_ref()) {
                        let at = t - (NOW - 10 * H);
                        assert!(est <= truth + 1e-9, "{hist} / {scen} / +{extra}/h unlisted at +{at}s: estimate {est} > truth {truth}");
                    }
                }
            }
        }
    }

    /// Invariant 6: for a fixed anchor, a later observation never raises the estimate —
    /// not a burn that drops, not a failed listing, not billing that fails after it worked.
    #[test]
    fn estimate_is_monotone() {
        // With spend only billing sees, billing wins over metering — and must keep winning
        // when a later billing read fails.
        for (hist, changes, times, fails) in histories() {
            for (scen, billing) in scenarios() {
                for extra in [0.0, 0.2] {
                    let run = simulate(&changes, &times, &fails, extra, billing.as_ref());
                    for w in run.windows(2) {
                        assert!(
                            w[1].1 <= w[0].1 + 1e-9,
                            "{hist} / {scen} / +{extra}/h unlisted: {} at {} rose to {} at {}",
                            w[0].1,
                            w[0].0,
                            w[1].1,
                            w[1].0
                        );
                    }
                }
            }
        }
        // Only a live reading (or `set`) can raise it.
        let mut s = apply(BalanceState::default(), &[fetched(Account::Runpod, Some(Ok(live(50.0, None))), None)], &burns_of(burn(1.0), None), Mode::Inline, NOW - 2 * H);
        let f = fetched(Account::Runpod, Some(Ok(live(80.0, None))), None);
        s = apply(s, std::slice::from_ref(&f), &burns_of(burn(1.0), None), Mode::Inline, NOW);
        assert_eq!(one_view(&s, &f, burn(1.0), Mode::Inline, NOW).balance, Some(80.0));
    }

    #[test]
    fn apply_table() {
        let b = burns_of(burn(0.5), burn(0.0));
        let ok = |usd: f64| fetched(Account::Runpod, Some(Ok(live(usd, Some(0.5)))), None);
        let mut s = apply(BalanceState::default(), &[ok(40.0)], &b, Mode::Inline, NOW);
        let st = &s.accounts[&Account::Runpod];
        // A live reading: anchor, provider facts, a first sample, the check anchor.
        assert_eq!(st.anchor, Some(Anchor { usd: 40.0, at: NOW, source: AnchorSource::Live }));
        assert_eq!(st.check_anchor, st.anchor);
        assert_eq!((st.live_ok_at, st.samples.as_slice()), (Some(NOW), &[(NOW, 0.5)][..]));
        assert_eq!(st.provider.unwrap().burn_per_hr, Some(0.5));

        // Samples: no change within the hour → none; the heartbeat → one; any rise → one;
        // a small drop → none; a drop over 1 % → one.
        let none = |acct| fetched(acct, None, None);
        let noise = 0.4 + 1e-12;
        for (dt, rate, appended) in [(600, 0.5, false), (H, 0.5, true), (H + 60, 0.51, true), (H + 120, 0.508, false), (H + 180, 0.4, true), (H + 240, noise, false)] {
            let before = s.accounts[&Account::Runpod].samples.len();
            s = apply(s, &[none(Account::Runpod)], &burns_of(burn(rate), None), Mode::Inline, NOW + dt);
            assert_eq!(s.accounts[&Account::Runpod].samples.len(), before + usize::from(appended), "+{dt}s at {rate}");
        }
        // A failed listing appends nothing (no stale echo).
        let before = s.accounts[&Account::Runpod].samples.clone();
        s = apply(s, &[none(Account::Runpod)], &burns_of(None, None), Mode::Inline, NOW + 3 * H);
        assert_eq!(s.accounts[&Account::Runpod].samples, before);

        // Failures: `since` kept, `last` moves; a success clears it.
        let fail = || fetched(Account::Runpod, Some(Err(Fail::transient("timed out"))), None);
        s = apply(s, &[fail()], &b, Mode::Inline, NOW + 4 * H);
        s = apply(s, &[fail()], &b, Mode::Inline, NOW + 5 * H);
        let lf = s.accounts[&Account::Runpod].live_fail.clone().unwrap();
        assert_eq!((lf.kind, lf.since, lf.last), (FailKind::Transient, NOW + 4 * H, NOW + 5 * H));
        // A young transient failure is only a label; after 6 h it is noted.
        let f = fetched(Account::Runpod, None, None);
        assert!(!one_view(&s, &f, burn(0.5), Mode::Inline, NOW + 5 * H).noted);
        assert!(one_view(&s, &f, burn(0.5), Mode::Inline, NOW + 10 * H).noted);
        // An auth failure is noted at once.
        let auth = fetched(Account::Runpod, Some(Err(Fail { kind: FailKind::Auth, http: Some(403), detail: None })), None);
        let s2 = apply(s.clone(), std::slice::from_ref(&auth), &b, Mode::Inline, NOW + 5 * H + 1);
        assert!(one_view(&s2, &f, burn(0.5), Mode::Inline, NOW + 5 * H + 1).noted);
        s = apply(s, &[ok(30.0)], &b, Mode::Inline, NOW + 6 * H);
        assert_eq!(s.accounts[&Account::Runpod].live_fail, None);

        // Reduced: recorded while only the minimal query works, cleared when the full one does.
        let reduced = fetched(Account::Runpod, Some(Ok(Live { reduced: true, ..live(29.0, None) })), None);
        s = apply(s, &[reduced.clone()], &b, Mode::Inline, NOW + 7 * H);
        s = apply(s, &[reduced], &b, Mode::Inline, NOW + 8 * H);
        assert_eq!(s.accounts[&Account::Runpod].live_reduced.as_ref().map(|r| (r.since, r.last)), Some((NOW + 7 * H, NOW + 8 * H)));
        s = apply(s, &[ok(28.0)], &b, Mode::Inline, NOW + 9 * H);
        assert_eq!(s.accounts[&Account::Runpod].live_reduced, None);

        // Billing: ok/fail recorded; billed_max follows the anchor.
        let billing = Billed { buckets: vec![(floor_hour(NOW + 9 * H), 0.3)], from: floor_hour(NOW + 9 * H), to: NOW + 11 * H };
        let with_billing = fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing)));
        s = apply(s, &[with_billing], &b, Mode::Inline, NOW + 10 * H);
        let st = &s.accounts[&Account::Runpod];
        assert_eq!((st.billing_ok_at, st.billed_max), (Some(NOW + 10 * H), Some(BilledMax { anchor_at: NOW + 9 * H, usd: 0.3 })));
        let billing_down = fetched(Account::Runpod, Some(Err(gone())), Some(Err(Fail { kind: FailKind::Auth, http: Some(403), detail: None })));
        s = apply(s, &[billing_down], &b, Mode::Inline, NOW + 10 * H + 60);
        assert_eq!(s.accounts[&Account::Runpod].billing_fail.as_ref().map(|f| f.kind), Some(FailKind::Auth));
        assert!(s.accounts[&Account::Runpod].billed_max.is_some(), "kept for the same anchor");

        // Prune and cap.
        let mut old = BalanceState::default();
        old.accounts.insert(
            Account::Runpod,
            AccountState { key_fp: FP.into(), samples: (0..2_500u64).map(|i| (NOW - 20 * 86_400 + i * 600, 1.0 + i as f64)).collect(), ..Default::default() },
        );
        let pruned = apply(old, &[none(Account::Runpod)], &burns_of(None, None), Mode::Inline, NOW);
        let samples = &pruned.accounts[&Account::Runpod].samples;
        assert!(samples.len() <= SAMPLE_MAX);
        assert!(samples[0].0 <= NOW - SAMPLE_KEEP && samples[1].0 > NOW - SAMPLE_KEEP, "one baseline older than the window is kept");

        // Key reset: another key's entry is replaced, never inherited.
        let other = Fetched { key_fp: "deadbeef".into(), ..fetched(Account::Runpod, None, None) };
        let reset = apply(s.clone(), &[other], &burns_of(None, None), Mode::Inline, NOW + 11 * H);
        assert_eq!(reset.accounts[&Account::Runpod], AccountState::new("deadbeef"));

        // --no-live and cache-only record nothing.
        for mode in [Mode::NoLive, Mode::CacheOnly] {
            assert_eq!(apply(s.clone(), &[ok(1.0)], &b, mode, NOW + 12 * H), s, "{mode:?}");
        }
    }

    /// The fallback check: skipped under 30 min or outside `balance`; safe within $1 / 5 %;
    /// overstates beyond; the check anchor rotates after a day.
    #[test]
    fn apply_fallback_check_table() {
        let b = burns_of(burn(1.0), None);
        let at = |usd: f64, t: u64| (fetched(Account::Runpod, Some(Ok(live(usd, None))), None), t);
        let run = |steps: &[(Fetched, u64)], mode: Mode| {
            let mut s = BalanceState::default();
            for (f, t) in steps {
                s = apply(s, std::slice::from_ref(f), &b, mode, *t);
            }
            s.accounts[&Account::Runpod].clone()
        };
        let t0 = NOW - 26 * H;
        // 20 min later: too short a horizon.
        assert_eq!(run(&[at(100.0, t0), at(99.6, t0 + 1_200)], Mode::Full).check, None);
        // Inline surfaces never check.
        assert_eq!(run(&[at(100.0, t0), at(99.0, t0 + H)], Mode::Inline).check, None);
        // 26 h at $1/h: predicted 74; live 74.40 → safe (read $0.40 low).
        let st = run(&[at(100.0, t0), at(74.4, NOW)], Mode::Full);
        let c = st.check.unwrap();
        assert_eq!((c.verdict, c.horizon_secs), (Verdict::Safe, 26 * H));
        assert!((c.predicted - 74.0).abs() < 1e-9);
        // Live 70: predicted 74 is $4 over (> max($1, 5 % = $3.50)) → overstates.
        assert_eq!(run(&[at(100.0, t0), at(70.0, NOW)], Mode::Full).check.unwrap().verdict, Verdict::Overstates);
        // Within the tolerance: predicted 74 vs live 73.20 → safe.
        assert_eq!(run(&[at(100.0, t0), at(73.2, NOW)], Mode::Full).check.unwrap().verdict, Verdict::Safe);
        // Rotation: after ≥ 24 h the check anchor moves to the newest live reading; before, it stays.
        assert_eq!(st.check_anchor.unwrap().at, NOW);
        let st = run(&[at(100.0, t0), at(90.0, t0 + 10 * H)], Mode::Full);
        assert_eq!(st.check_anchor.unwrap().at, t0);
    }

    #[test]
    fn views_label_table() {
        let b = burn(0.47);
        let live_view = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Ok(live(32.54, Some(0.47)))), None), b, Mode::Inline, NOW);
        let mut cached = state_with(AccountState {
            provider: Some(ProviderFacts { usd: 32.54, burn_per_hr: Some(0.47), spend_limit_per_hr: None, under_balance: None, at: NOW - 180 }),
            ..anchored(32.54, 180, AnchorSource::Live)
        });
        let cache_view = one_view(&cached, &fetched(Account::Runpod, None, None), b, Mode::Inline, NOW);
        let no_live_view = one_view(&cached, &fetched(Account::Runpod, None, None), b, Mode::NoLive, NOW);
        cached.accounts.get_mut(&Account::Runpod).unwrap().live_fail =
            Some(FailRecord { kind: FailKind::Gone, http: Some(404), detail: None, since: NOW - 2 * 86_400, last: NOW });
        let failing_view = one_view(&cached, &fetched(Account::Runpod, None, None), b, Mode::Inline, NOW + FRESH);
        let unknown_view = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), b, Mode::Inline, NOW);
        // (case, view, BALANCE, SOURCE, source in JSON)
        let cases = [
            ("live", &live_view, "$32.54", "live", "live"),
            ("cached", &cache_view, "$32.54", "live 3m", "live"),
            ("--no-live: never live", &no_live_view, "~$32.52", "est. (from live 3m, metered)", "estimate"),
            ("failing", &failing_view, "~$32.48", "est. (live n/a 8m, metered)", "estimate"),
            ("unknown", &unknown_view, "?", "?", "unknown"),
        ];
        for (case, v, bal, src, json_src) in cases {
            let now = if case == "failing" { NOW + FRESH } else { NOW };
            assert_eq!((balance_label(v).as_str(), source_label(v, now).as_str()), (bal, src), "{case}");
            assert_eq!(serde_json::to_value(v).unwrap()["source"], json_src, "{case}: JSON always carries the source");
            if v.source == Source::Estimate {
                let line = footer_line(std::slice::from_ref(v), &policy(), now).unwrap();
                assert!(line.contains('~') && line.contains("est."), "{case}: {line}");
            }
        }
    }

    #[test]
    fn unknown_renders_question_mark() {
        // A failed listing with no reading: `?` with the nudge and an unknown burn — never
        // $0.00, never no burn.
        let v = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), None, Mode::Inline, NOW);
        assert_eq!((v.balance, v.burn, v.level), (None, None, Level::Unknown));
        let line = footer_line(&[v.clone()], &policy(), NOW).unwrap();
        assert_eq!(line, "balance: runpod ? (no reading — arena balance set runpod <USD>) ?");
        let table = super::render_table(&[v.clone()], NOW);
        assert!(!table.contains("$0.00") && !table.contains("no burn") && !table.contains("∞"), "{table}");
        // Burning with no balance: the burn is said.
        let v = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), burn(0.47), Mode::Inline, NOW);
        assert_eq!(footer_line(&[v], &policy(), NOW).unwrap(), "balance: runpod ? (no reading — arena balance set runpod <USD>) $0.47/h");
    }

    /// A view of `usd` at `rate`/h from a live reading this run.
    fn lv(account: Account, usd: f64, rate: f64) -> AccountView {
        let f = Fetched { account, key_fp: FP.into(), live: Some(Ok(live(usd, Some(rate)))), billed: None };
        views(&BalanceState::default(), &[f], &burns_of(burn(rate), burn(rate)), &policy(), Mode::Inline, NOW).remove(0)
    }

    #[test]
    fn footer_table() {
        let ok = lv(Account::Runpod, 48.0, 0.5); // 96h
        let low = lv(Account::Runpod, 31.2, 0.68); // ≈45h
        let crit = lv(Account::Runpod, 3.1, 0.47); // ≈6.5h
        let mut est = lv(Account::Vast, 118.3, 0.1);
        est.source = Source::Estimate;
        est.anchor_source = Some(AnchorSource::Live);
        est.method = Some(Method::Metered);
        est.as_of = Some(NOW - 3 * H);
        let p = policy();
        assert_eq!(footer_line(&[ok.clone(), est.clone()], &p, NOW).unwrap(), "balance: runpod $48.00 live ≈4.0d · vast ~$118.30 est. ≈49.2d");
        assert_eq!(footer_line(&[low], &p, NOW).unwrap(), "balance: ⚠ runpod $31.20 live ≈45h < 72h");
        assert_eq!(
            footer_line(&[crit], &p, NOW).unwrap(),
            "balance: ‼ runpod $3.10 live ≈6.5h — at $0 RunPod terminates pods without a network volume (data lost)"
        );
        // A low estimate spells out its source.
        let mut low_est = lv(Account::Runpod, 31.2, 0.68);
        low_est.source = Source::Estimate;
        low_est.anchor_source = Some(AnchorSource::Live);
        low_est.method = Some(Method::Billed);
        low_est.as_of = Some(NOW - 2 * 86_400);
        low_est.live_failing = Some(FailRecord { kind: FailKind::Gone, http: Some(404), detail: None, since: NOW - 86_400, last: NOW });
        assert_eq!(footer_line(&[low_est], &p, NOW).unwrap(), "balance: ⚠ runpod ~$31.20 est. (live n/a 2d, billed) ≈45h < 72h");
        // ≤ 0 by estimate while pods run: the probable top-up.
        let mut zero = lv(Account::Runpod, -1.2, 0.47);
        zero.source = Source::Estimate;
        zero.method = Some(Method::Metered);
        zero.as_of = Some(NOW - H);
        zero.anchor_source = Some(AnchorSource::Live);
        let line = footer_line(&[zero], &p, NOW).unwrap();
        assert!(line.starts_with("balance: ‼ runpod ~-$1.20 est. (from live 1h, metered) <1h — ≤$0 est. — pods still running"), "{line}");
        // Hetzner (postpaid) is never in the footer; no prepaid accounts → no line.
        let mut hz = AccountView::blank("hetzner", "EUR", Source::Postpaid);
        hz.level = Level::None;
        assert_eq!(footer_line(&[hz], &p, NOW), None);
        assert_eq!(footer_line(&[], &p, NOW), None);
    }

    #[test]
    fn render_table() {
        let mut hz = AccountView::blank("hetzner", "EUR", Source::Postpaid);
        hz.burn = Some(Burn { per_hr: 0.006, unpriced: 0, stale: false });
        hz.level = Level::None;
        let views = vec![lv(Account::Runpod, 48.0, 0.5), lv(Account::Vast, 3.0, 0.5), hz];
        let text = super::render_table(&views, NOW);
        for needle in [
            "ACCOUNT   BALANCE  SOURCE        BURN  RUNWAY  RUNS OUT (UTC)    LEVEL",
            "runpod     $48.00  live       $0.50/h  ≈4.0d   2026-10-12 12:00  ok",
            "vast        $3.00  live       $0.50/h  ≈6.0h   2026-10-08 18:00  ‼ critical",
            "hetzner  postpaid  postpaid  €0.006/h  -       -                 -",
            "burn = the higher of",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
        // One row per account: USD and EUR are never added up anywhere.
        assert!(!text.contains("total"), "{text}");
        assert!(super::render_table(&[], NOW).contains("no provider keys configured"));
        // Postpaid never gets a level or a runway.
        let l = listing(vec![("hetzner", Ok(vec![pod("hetzner", "running", Some(0.006))]))]);
        let v = postpaid_view(&l, &l.pods()).unwrap();
        assert_eq!((v.level, v.runway_hours, v.balance), (Level::None, None, None));
        assert_eq!(postpaid_view(&listing(vec![("runpod", Ok(vec![]))]), &[]), None, "not configured: no row");
        assert_eq!(check_exit(&[v]), 0);
    }

    #[test]
    fn notes_table() {
        let mut v = lv(Account::Runpod, 32.54, 0.47);
        v.listed_per_hr = Some(0.12);
        v.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 32.14, live: 32.54, verdict: Verdict::Safe });
        let mut bad = lv(Account::Vast, 10.0, 0.1);
        bad.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 13.1, live: 10.0, verdict: Verdict::Overstates });
        bad.live_failing = Some(FailRecord { kind: FailKind::Drift, http: Some(400), detail: Some("Cannot query field \"credit\"".into()), since: NOW - 86_400, last: NOW });
        bad.noted = true;
        let unknown = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), burn(0.5), Mode::Inline, NOW);
        let n = notes(&[v, bad, unknown], &policy(), NOW);
        let want = [
            "runpod: fallback check ✓ over 1d (the estimate read $0.40 low)",
            "runpod: RunPod reports $0.35/h more than arena lists (network volumes, stopped pods' disks, serverless?)",
            "vast: live balance failing since 2026-10-07 12:00 UTC (drift, HTTP 400: Cannot query field \"credit\") — estimates in use",
            "⚠ vast: fallback check: the estimate read $3.10 HIGH over 1d — don't rely on estimates",
            "? runpod: no usable reading — read the balance off the console and run `arena balance set runpod <USD>`",
        ];
        for w in want {
            assert!(n.iter().any(|l| l == w), "`{w}` missing: {n:#?}");
        }
        // A young transient failure gets no note (only the label).
        let mut quiet = lv(Account::Runpod, 48.0, 0.5);
        quiet.live_failing = Some(FailRecord { kind: FailKind::Transient, http: Some(503), detail: None, since: NOW - 600, last: NOW });
        assert!(notes(&[quiet], &policy(), NOW).is_empty());
    }

    #[test]
    fn preflight_table() {
        let mut v = lv(Account::Runpod, 32.54, 0.47);
        v.spend_limit_per_hr = Some(80.0);
        let p = policy();
        assert_eq!(
            preflight_lines(&v, 8, Some(0.17), &p, NOW),
            ["balance: runpod $32.54 live · $0.47/h now + ≤$1.36/h for 8 new pod(s) → ≈17h ‼ under 24h"]
        );
        let lines = preflight_lines(&v, 2, Some(0.17), &p, NOW);
        assert_eq!(lines, ["balance: runpod $32.54 live · $0.47/h now + ≤$0.34/h for 2 new pod(s) → ≈40h ⚠ below BALANCE_WARN_HOURS=72"]);
        // Under an hour's credit, over the spend limit, under balance.
        let mut poor = lv(Account::Runpod, 0.8, 0.47);
        poor.spend_limit_per_hr = Some(80.0);
        poor.under_balance = Some(true);
        let lines = preflight_lines(&poor, 500, Some(0.17), &p, NOW);
        assert_eq!(lines[1], "⚠ RunPod needs ≥1h of credit for the new pods (≈$85.00); the account has $0.80 — creates will likely fail (402)");
        assert_eq!(lines[2], "⚠ planned $85.47/h exceeds RunPod's spend limit $80.00/h — creates past it will be refused");
        assert_eq!(lines[3], "‼ RunPod reports the account under its minimum balance — creates will likely fail");
        // The single-spec path: price unknown.
        let rich = lv(Account::Runpod, 500.0, 0.47);
        assert_eq!(preflight_lines(&rich, 1, None, &p, NOW), ["balance: runpod $500.00 live · $0.47/h now + new pods (price unknown) → ≈44.3d"]);
        // Vast: no RunPod-specific lines.
        let vast = lv(Account::Vast, 0.1, 0.0);
        assert_eq!(preflight_lines(&vast, 4, Some(0.3), &p, NOW).len(), 1);
        // Unknown: said, with the nudge.
        let unknown = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), None, Mode::Inline, NOW);
        assert_eq!(
            preflight_lines(&unknown, 1, Some(0.2), &p, NOW),
            ["balance: runpod ? (no reading — arena balance set runpod <USD>) · ?/h now + ≤$0.20/h for 1 new pod(s) → ?"]
        );
    }

    #[test]
    fn teardown_info_table() {
        let mut rp = lv(Account::Runpod, 12.4, 0.1);
        (rp.provider_burn_per_hr, rp.listed_per_hr) = (Some(0.35), Some(0.0));
        let mut vast = lv(Account::Vast, 3.1, 0.0);
        vast.source = Source::Estimate;
        let mut hz = AccountView::blank("hetzner", "EUR", Source::Postpaid);
        hz.level = Level::None;
        assert_eq!(
            teardown_info(&[rp, vast, hz.clone()], NOW),
            [
                "info: prepaid balance — runpod $12.40 live, vast ~$3.10 est. (not part of the verdict)",
                "info: RunPod reports $0.35/h more than arena lists (network volumes, stopped pods' disks, serverless?)",
            ]
        );
        assert!(teardown_info(&[hz], NOW).is_empty());
    }

    #[test]
    fn tui_parts_table() {
        let ok = lv(Account::Runpod, 48.0, 0.5);
        let mut est = lv(Account::Vast, 118.3, 0.1);
        est.source = Source::Estimate;
        assert_eq!(tui_parts(&[ok.clone(), est]), (None, Some("runpod $48.00 ≈4.0d · vast ~$118.30 est. ≈49.2d".into()), Level::Ok));
        let low = lv(Account::Runpod, 31.2, 0.68);
        let crit = lv(Account::Vast, 3.0, 0.5);
        assert_eq!(tui_parts(&[low.clone()]), (Some(" ⚠ runpod balance ≈45h ·".into()), None, Level::Low));
        assert_eq!(tui_parts(&[low, crit]), (Some(" ⚠ runpod balance ≈45h · ‼ vast balance ≈6.0h ·".into()), None, Level::Critical));
        assert_eq!(tui_parts(&[]), (None, None, Level::Ok));
    }

    #[test]
    fn set_lines_table() {
        let p = policy();
        // First ever, no burn known: said plainly.
        let s = state_with(anchored(250.0, 0, AnchorSource::Manual));
        assert_eq!(
            set_lines(None, &s, Account::Runpod, FP, &p, NOW),
            [
                "runpod: no reading → $250.00 (manual)",
                "now: runpod ~$250.00 est. (manual) — counted down at the fleet's burn from the next listing",
            ]
        );
        // With samples: the footer's own words (the last sample stands in for the burn: `≤`).
        let s = state_with(AccountState { samples: vec![(NOW - H, 0.5)], ..anchored(250.0, 0, AnchorSource::Manual) });
        let old = Some(Anchor { usd: 31.0, at: NOW - 3 * H, source: AnchorSource::Live });
        assert_eq!(
            set_lines(old, &s, Account::Runpod, FP, &p, NOW),
            ["runpod: $31.00 (live, 3h ago) → $250.00 (manual)", "now: runpod ~$250.00 est. ≤20.8d"]
        );
        // A live reading younger than FRESH: it wins for now, and that is said.
        let s = state_with(AccountState {
            provider: Some(ProviderFacts { usd: 31.0, burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, at: NOW - 60 }),
            ..anchored(250.0, 0, AnchorSource::Manual)
        });
        let lines = set_lines(old, &s, Account::Runpod, FP, &p, NOW);
        assert_eq!(lines[1], "now: ⚠ runpod $31.00 live 1m ≈2.5d < 72h");
        assert_eq!(lines[2], "note: live is $31.00; a manual value is used only while live is unavailable");
    }

    #[test]
    fn config_rows_table() {
        let cfg = Config::parse("");
        let accounts = vec![(Account::Runpod, FP.to_string()), (Account::Vast, FP.to_string())];
        let mut s = state_with(AccountState {
            live_ok_at: Some(NOW - 240),
            check: Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 32.14, live: 32.54, verdict: Verdict::Safe }),
            ..anchored(32.54, 240, AnchorSource::Live)
        });
        assert_eq!(
            config_rows(&cfg, &s, &accounts, NOW),
            [
                "  ✓ BALANCE_WARN_HOURS         72 (default) — warn below 3d of runway",
                "  ✓ runpod                     live 4m ago · fallback check ✓ over 1d (read $0.40 low)",
                "  · vast                       no reading yet — run `arena balance`",
            ]
        );
        let st = s.accounts.get_mut(&Account::Runpod).unwrap();
        st.live_fail = Some(FailRecord {
            kind: FailKind::Drift,
            http: Some(400),
            detail: Some("Cannot query field \"clientBalance\" on type \"User\".".into()),
            since: 1_796_256_000, // 2026-12-03
            last: NOW,
        });
        st.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 13.1, live: 10.0, verdict: Verdict::Overstates });
        st.billing_fail = Some(FailRecord { kind: FailKind::Auth, http: Some(403), detail: None, since: NOW, last: NOW });
        st.live_reduced = Some(FailRecord { kind: FailKind::Drift, http: None, detail: None, since: NOW - H, last: NOW });
        let rows = config_rows(&Config::parse("BALANCE_WARN_HOURS=soon"), &s, &accounts[..1], NOW);
        assert_eq!(
            rows,
            [
                "  ✗ BALANCE_WARN_HOURS         BALANCE_WARN_HOURS must be a number of hours ≥ 0 (got `soon`) — 72 is used",
                "  ⚠ runpod                     live balance failing since 2026-12-03 00:00 UTC (drift, HTTP 400: Cannot query field \"clientBalance\" on type \"User\".) — estimates in use",
                "  ⚠ runpod                     full balance query rejected since 2026-10-08 11:00 UTC — live via the minimal query (no burn/limit/underBalance)",
                "  ⚠ runpod                     fallback check: the estimate read $3.10 HIGH over 1d — don't rely on estimates",
                "  ⚠ runpod                     billing history failing (auth, HTTP 403) — estimates use arena's own metering only",
            ]
        );
        assert_eq!(config_rows(&Config::parse("BALANCE_WARN_HOURS=0"), &s, &[], NOW), ["  ✓ BALANCE_WARN_HOURS         0 — runway warnings off"]);
    }

    #[test]
    fn check_exit_codes() {
        let at = |l: Level| {
            let mut v = AccountView::blank("runpod", "USD", Source::Live);
            v.level = l;
            v
        };
        for (levels, want) in [
            (vec![], 0),
            (vec![Level::Ok, Level::None], 0),
            (vec![Level::Low], 1),
            (vec![Level::Unknown], 3),
            (vec![Level::Critical], 2),
            (vec![Level::Low, Level::Unknown], 3),
            (vec![Level::Low, Level::Unknown, Level::Critical], 2),
        ] {
            let views: Vec<AccountView> = levels.iter().map(|l| at(*l)).collect();
            assert_eq!(check_exit(&views), want, "{levels:?}");
        }
    }

    /// A fresh temp directory, removed at the end of the test.
    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-balance-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    #[test]
    fn state_roundtrip_tolerant_load() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("roundtrip");
        let path = dir.0.join("state").join("devtest").join(crate::snapshot::BALANCE_FILE);
        let s = apply(
            BalanceState::default(),
            &[fetched(Account::Runpod, Some(Ok(live(32.54, Some(0.47)))), None)],
            &burns_of(burn(0.47), None),
            Mode::Inline,
            NOW,
        );
        save(&s, &path).unwrap();
        assert_eq!(load(&path), (s.clone(), None));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&path), mode(path.parent().unwrap())), (0o600, 0o700));
        // The documented shape.
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["accounts"]["runpod"]["anchor"], json!({"usd": 32.54, "at": NOW, "source": "live"}));
        assert_eq!(v["accounts"]["runpod"]["samples"], json!([[NOW, 0.47]]));
        // Missing: empty, no warning. Corrupt / foreign / newer: empty plus one warning.
        assert_eq!(load(&dir.0.join("nope.json")), (BalanceState::default(), None));
        for junk in ["{not json", "[]", "{\"version\": 2, \"accounts\": {}}", ""] {
            std::fs::write(&path, junk).unwrap();
            let (st, warn) = load(&path);
            assert_eq!(st, BalanceState::default(), "{junk}");
            assert!(warn.unwrap().starts_with("warning: ignoring the balance state"), "{junk}");
        }
        let (st, warn) = load(&dir.0);
        assert!(st.accounts.is_empty() && warn.unwrap().contains("can't read"));
        // Unsorted samples from a hand edit are put back in order.
        std::fs::write(&path, r#"{"version":1,"accounts":{"vast":{"key_fp":"x","samples":[[5,1.0],[2,0.5],[5,9.0]]}}}"#).unwrap();
        assert_eq!(load(&path).0.accounts[&Account::Vast].samples, vec![(2, 0.5), (5, 1.0)]);
    }

    // ---- I/O: the HTTP reads over a loopback server, observe, record ----

    use crate::http::test_server::{canned, hang_up, serve, stall, Server};
    use crate::provider::runpod::{BALANCE_QUERY_FULL, BALANCE_QUERY_MINIMAL};

    const RP_KEY: &str = "rpa_SECRETKEY0123";
    const VAST_KEY: &str = "VASTSECRETKEY0123";
    const JSON: &str = "application/json";

    /// One server per endpoint, so concurrent requests can't take each other's answers.
    fn api_at(gql: &Server, v2: &Server, vast: &Server) -> HttpBalanceApi {
        HttpBalanceApi::with_bases(
            &format!("{}/graphql", gql.base),
            &format!("{}/v2", v2.base),
            &format!("{}/api/v0", vast.base),
            Some(RP_KEY),
            Some(VAST_KEY),
        )
    }

    fn bodies(s: &Server) -> Vec<Value> {
        s.bodies.lock().unwrap().iter().map(|b| serde_json::from_str(b).unwrap_or(Value::Null)).collect()
    }

    fn minimal_body(usd: &str) -> String {
        json!({"data": {"myself": {"clientBalance": usd}}}).to_string()
    }

    /// A state where RunPod (this API's key) has a usable anchor — and a check anchor.
    fn anchored_for(api: &dyn BalanceApi, age: u64) -> BalanceState {
        let mut s = BalanceState::default();
        for (a, fp) in api.accounts() {
            let anchor = Some(Anchor { usd: 50.0, at: NOW - age, source: AnchorSource::Live });
            s.accounts.insert(a, AccountState { key_fp: fp, anchor, check_anchor: anchor, ..Default::default() });
        }
        s
    }

    #[tokio::test]
    async fn runpod_full_then_minimal_on_unknown_field() {
        // The validation error as HTTP 400 (and as a 200 with `errors`, the other form).
        for status in [400, 200] {
            let gql = serve(vec![
                canned(status, JSON, &fixture("runpod_myself_unknown_field").to_string()),
                canned(200, JSON, &minimal_body("32.54")),
            ]);
            let (v2, vast) = (serve(vec![]), serve(vec![]));
            let l = api_at(&gql, &v2, &vast).live(Account::Runpod).await.unwrap();
            assert_eq!(l, Live { reduced: true, ..live(32.54, None) }, "{status}");
            let sent = bodies(&gql);
            assert_eq!(sent, [json!({"query": BALANCE_QUERY_FULL}), json!({"query": BALANCE_QUERY_MINIMAL})], "exactly two requests, the second minimal");
        }
        // The full query working: one request, every field.
        let gql = serve(vec![canned(200, JSON, &fixture("runpod_myself").to_string())]);
        let (v2, vast) = (serve(vec![]), serve(vec![]));
        let l = api_at(&gql, &v2, &vast).live(Account::Runpod).await.unwrap();
        assert_eq!((l.usd, l.provider_burn_per_hr, l.reduced), (32.54, Some(0.47), false));
        assert_eq!(bodies(&gql).len(), 1);
        // Any other failure isn't retried with the minimal query.
        let gql = serve(vec![canned(502, "text/html", "<html>Bad Gateway</html>"), canned(200, JSON, &minimal_body("1"))]);
        let (v2, vast) = (serve(vec![]), serve(vec![]));
        let f = api_at(&gql, &v2, &vast).live(Account::Runpod).await.unwrap_err();
        assert_eq!((f.kind, f.http, f.detail), (FailKind::Transient, Some(502), None));
        assert_eq!(bodies(&gql).len(), 1);
        // The minimal query failing too: its failure is the answer.
        let gone_too = json!({"errors": [{"message": "Cannot query field \"clientBalance\" on type \"User\"."}]}).to_string();
        let gql = serve(vec![canned(400, JSON, &gone_too), canned(400, JSON, &gone_too)]);
        let (v2, vast) = (serve(vec![]), serve(vec![]));
        let f = api_at(&gql, &v2, &vast).live(Account::Runpod).await.unwrap_err();
        assert!(f.kind == FailKind::Drift && f.detail.unwrap().contains("clientBalance"));
        assert_eq!(bodies(&gql).len(), 2);
    }

    #[test]
    fn billing_and_user_requests_keep_keys_out_of_urls() {
        let c = reqwest::Client::new();
        let req = crate::provider::runpod_v2::billing_request(&c, "https://api.runpod.io/v2/", "rpa_SECRET", NOW - 2 * H, NOW + 5)
            .build()
            .unwrap();
        assert_eq!(req.method(), reqwest::Method::GET);
        assert_eq!(
            req.url().as_str(),
            "https://api.runpod.io/v2/billing?bucketSize=hour&startTime=2026-10-08T10%3A00%3A00Z&endTime=2026-10-08T12%3A00%3A05Z"
        );
        assert_eq!(req.headers()["authorization"], "Bearer rpa_SECRET");
        let req = crate::provider::vast::user_request(&c, "https://console.vast.ai/api/v0", "VSECRET").build().unwrap();
        assert_eq!((req.method(), req.url().as_str()), (&reqwest::Method::GET, "https://console.vast.ai/api/v0/users/current/"));
        assert_eq!(req.headers()["authorization"], "Bearer VSECRET");
        for r in [&req] {
            assert!(!r.url().as_str().contains("SECRET") && r.body().is_none());
        }
    }

    /// Invariant 1: every request the HTTP API sends is a GraphQL POST of a *query* (no
    /// `mutation`), or one of the two GETs — keys only in the header.
    #[tokio::test]
    async fn only_reads() {
        let gql = serve(vec![
            canned(400, JSON, &fixture("runpod_myself_unknown_field").to_string()),
            canned(200, JSON, &minimal_body("5")),
            canned(200, JSON, &fixture("runpod_myself").to_string()),
        ]);
        let v2 = serve(vec![canned(200, JSON, &fixture("billing_hourly_openapi").to_string())]);
        let vast = serve(vec![canned(200, JSON, &fixture("vast_user_canary").to_string())]);
        let api = api_at(&gql, &v2, &vast);
        api.live(Account::Runpod).await.unwrap();
        api.live(Account::Runpod).await.unwrap();
        api.billed(NOW - 2 * H, NOW).await.unwrap();
        api.live(Account::Vast).await.unwrap();
        let lines = |s: &Server| s.requests.lock().unwrap().clone();
        assert!(lines(&gql).iter().all(|l| l == "POST /graphql HTTP/1.1"), "{:?}", lines(&gql));
        for b in bodies(&gql) {
            let q = b.as_object().filter(|o| o.len() == 1).and_then(|o| o["query"].as_str()).unwrap_or_else(|| panic!("{b}"));
            assert!(q.trim_start().starts_with('{') && !q.contains("mutation"), "{q}");
        }
        assert!(lines(&v2).iter().all(|l| l.starts_with("GET /v2/billing?bucketSize=hour&")), "{:?}", lines(&v2));
        assert_eq!(lines(&vast), ["GET /api/v0/users/current/ HTTP/1.1"]);
        for s in [&gql, &v2, &vast] {
            assert!(lines(s).iter().all(|l| !l.contains("SECRET")));
            for h in s.headers.lock().unwrap().iter() {
                assert!(h.iter().any(|l| l.to_ascii_lowercase().starts_with("authorization: bearer ")), "{h:?}");
            }
        }
        assert!(bodies(&v2).iter().chain(bodies(&vast).iter()).all(Value::is_null), "the GETs carry no body");
    }

    /// Invariant 3: with servers that never answer, an inline observation returns within
    /// INLINE_TIMEOUT + 1 s and `arena balance`'s within COMMAND_TIMEOUT + 1 s, every read a
    /// transient failure.
    #[tokio::test]
    async fn stall_is_transient_within_limit() {
        let stalled = || (serve(vec![stall(), stall()]), serve(vec![stall()]), serve(vec![stall()]));
        let (g1, b1, v1) = stalled();
        let (g2, b2, v2) = stalled();
        let (inline_api, full_api) = (api_at(&g1, &b1, &v1), api_at(&g2, &b2, &v2));
        let full_state = anchored_for(&full_api, 2 * H);
        async fn timed(api: &HttpBalanceApi, state: BalanceState, mode: Mode, limit: std::time::Duration) -> (Vec<Fetched>, std::time::Duration) {
            let t = std::time::Instant::now();
            let got = observe(api, &state, mode, limit, NOW).await;
            (got, t.elapsed())
        }
        let ((inline, ti), (full, tf)) = tokio::join!(
            timed(&inline_api, BalanceState::default(), Mode::Inline, INLINE_TIMEOUT),
            timed(&full_api, full_state, Mode::Full, COMMAND_TIMEOUT),
        );
        let second = std::time::Duration::from_secs(1);
        assert!(ti < INLINE_TIMEOUT + second, "inline took {ti:?}");
        assert!(tf < COMMAND_TIMEOUT + second, "full took {tf:?}");
        assert_eq!(inline.len(), 2);
        assert!(full.iter().any(|f| f.billed.is_some()), "the full run fetched billing too");
        for f in inline.iter().chain(full.iter()) {
            for r in [f.live.as_ref().map(|r| r.as_ref().err()), f.billed.as_ref().map(|r| r.as_ref().err())].into_iter().flatten() {
                assert_eq!(r.map(|e| e.kind), Some(FailKind::Transient), "{f:?}");
            }
        }
    }

    #[tokio::test]
    async fn hang_up_is_transient() {
        let (gql, v2, vast) = (serve(vec![hang_up()]), serve(vec![hang_up()]), serve(vec![hang_up()]));
        let api = api_at(&gql, &v2, &vast);
        for f in [api.live(Account::Runpod).await.unwrap_err(), api.live(Account::Vast).await.unwrap_err(), api.billed(NOW - H, NOW).await.unwrap_err()] {
            assert_eq!((f.kind, f.http), (FailKind::Transient, None), "{f:?}");
            assert!(!format!("{f:?}").contains("127.0.0.1"), "our words, not reqwest's: {f:?}");
        }
    }

    #[tokio::test]
    async fn inline_skips_live_when_fresh() {
        let fp = FakeBalanceApi::fp(Account::Runpod);
        let fresh = |st: AccountState| {
            let mut s = BalanceState::default();
            s.accounts.insert(Account::Runpod, AccountState { key_fp: fp.clone(), ..st });
            s
        };
        // A success 100 s ago: no request at all.
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(1.0, None));
        let s = fresh(AccountState { live_ok_at: Some(NOW - 100), ..anchored(5.0, 100, AnchorSource::Live) });
        let got = observe(&api, &s, Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (0, 0));
        assert_eq!(got, [Fetched::nothing(Account::Runpod, fp.clone())]);
        // A failure 100 s ago: no live call either (the estimate needs billing, not live).
        let failed = FailRecord { kind: FailKind::Gone, http: Some(404), detail: None, since: NOW - 3 * H, last: NOW - 100 };
        let s = fresh(AccountState { live_fail: Some(failed), ..anchored(5.0, 4 * H, AnchorSource::Live) });
        observe(&api, &s, Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (0, 1));
        // FRESH old: live again.
        let s = fresh(AccountState { live_ok_at: Some(NOW - FRESH), ..anchored(5.0, FRESH, AnchorSource::Live) });
        observe(&api, &s, Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!(api.live_calls(), 1);
    }

    #[tokio::test]
    async fn inline_billing_only_while_live_failing() {
        let fp = FakeBalanceApi::fp(Account::Runpod);
        let state = |fail: bool| {
            let lf = fail.then(|| FailRecord { kind: FailKind::Gone, http: Some(410), detail: None, since: NOW - 3 * H, last: NOW - H });
            let mut s = BalanceState::default();
            s.accounts.insert(
                Account::Runpod,
                AccountState { key_fp: fp.clone(), live_ok_at: Some(NOW - 4 * H), live_fail: lf, ..anchored(5.0, 4 * H, AnchorSource::Live) },
            );
            s
        };
        let bill = Billed { buckets: vec![], from: floor_hour(NOW - 4 * H), to: NOW };
        // Live works: no billing.
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(4.0, None)).billed_ok(bill.clone());
        let got = observe(&api, &state(false), Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls(), got[0].billed.is_none()), (1, 0, true));
        // Live fails now (it was fine before): billing after it.
        let api = FakeBalanceApi::new().live_err(Account::Runpod, gone()).billed_ok(bill.clone());
        let got = observe(&api, &state(false), Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (1, 1));
        assert_eq!(got[0].billed, Some(Ok(bill.clone())));
        // Live was already failing: both at once.
        let api = FakeBalanceApi::new().live_err(Account::Runpod, gone()).billed_ok(bill.clone());
        observe(&api, &state(true), Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (1, 1));
        assert_eq!(api.billed_args(), [(floor_hour(NOW - 4 * H), NOW)]);
        // No usable anchor: nothing to count down from, so no billing.
        let api = FakeBalanceApi::new().live_err(Account::Runpod, gone());
        observe(&api, &BalanceState::default(), Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!(api.billed_calls(), 0);
        // Vast has no billing history: never asked.
        let api = FakeBalanceApi::new().live_err(Account::Vast, gone());
        observe(&api, &BalanceState::default(), Mode::Inline, INLINE_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (1, 0));
    }

    #[tokio::test]
    async fn full_fetches_billing_for_check() {
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(40.0, None)).live_ok(Account::Vast, live(9.0, None));
        let mut s = anchored_for(&api, H);
        // The check anchor is older than the anchor: billing from its hour.
        s.accounts.get_mut(&Account::Runpod).unwrap().check_anchor = Some(Anchor { usd: 60.0, at: NOW - 26 * H, source: AnchorSource::Live });
        let got = observe(&api, &s, Mode::Full, COMMAND_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (2, 1), "live for both, billing for RunPod");
        assert_eq!(api.billed_args(), [(floor_hour(NOW - 26 * H), NOW)]);
        assert!(got.iter().all(|f| f.live.is_some()));
        // No anchor yet: live only.
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(40.0, None));
        observe(&api, &BalanceState::default(), Mode::Full, COMMAND_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (1, 0));
    }

    #[tokio::test]
    async fn no_live_makes_no_live_call() {
        let api = FakeBalanceApi::new()
            .live_ok(Account::Runpod, live(40.0, None))
            .billed_ok(Billed { buckets: vec![], from: floor_hour(NOW - H), to: NOW });
        let s = anchored_for(&api, H);
        let got = observe(&api, &s, Mode::NoLive, COMMAND_TIMEOUT, NOW).await;
        assert_eq!((api.live_calls(), api.billed_calls()), (0, 1));
        assert!(got[0].live.is_none() && got[0].billed.is_some());
        assert_eq!(apply(s.clone(), &got, &burns_of(burn(1.0), None), Mode::NoLive, NOW), s, "and records nothing");
        // Cache-only: no call of any kind, every configured account listed.
        let api = FakeBalanceApi::new().with(Account::Runpod).with(Account::Vast).panicking();
        let got = observe(&api, &s, Mode::CacheOnly, COMMAND_TIMEOUT, NOW).await;
        assert_eq!(got.iter().map(|f| f.account).collect::<Vec<_>>(), [Account::Runpod, Account::Vast]);
        assert!(got.iter().all(|f| f.live.is_none() && f.billed.is_none()));
    }

    #[tokio::test]
    async fn vast_2xx_without_credit_is_drift_and_unquoted() {
        let mut no_credit = fixture("vast_user_canary");
        no_credit.as_object_mut().unwrap().remove("credit");
        let canary_err = json!({"errors": [{"message": format!("bad key {}", CANARIES[0])}]}).to_string();
        let vast = serve(vec![
            canned(200, JSON, &no_credit.to_string()),
            canned(400, JSON, &canary_err),
            canned(200, "text/html", &format!("<html>{}</html>", CANARIES[1])),
        ]);
        let (gql, v2) = (serve(vec![]), serve(vec![]));
        let api = api_at(&gql, &v2, &vast);
        let f = api.live(Account::Vast).await.unwrap_err();
        assert_eq!((f.kind, f.detail.as_deref()), (FailKind::Drift, Some(VAST_NO_CREDIT)));
        // A Vast error body is never quoted — even one shaped like a GraphQL error.
        let f = api.live(Account::Vast).await.unwrap_err();
        assert_eq!((f.kind, f.http, f.detail.as_deref()), (FailKind::Drift, Some(400), None));
        let f = api.live(Account::Vast).await.unwrap_err();
        assert_eq!(f.kind, FailKind::Gone);
        assert!(!format!("{f:?}").contains(CANARIES[1]));
    }

    /// Two writers at once (the TUI and a CLI): each one's update survives.
    #[test]
    fn record_merges_concurrent_writers() {
        let dir = tmp("concurrent");
        let path = dir.0.join("devtest").join(crate::snapshot::BALANCE_FILE);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writers: Vec<_> = Account::ALL
            .into_iter()
            .map(|a| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..25u64 {
                        let f = Fetched { account: a, key_fp: FP.into(), live: Some(Ok(live(100.0 + i as f64, None))), billed: None };
                        record(&path, |s| apply(s, &[f], &BTreeMap::new(), Mode::Inline, NOW + i)).unwrap();
                    }
                })
            })
            .collect();
        writers.into_iter().for_each(|w| w.join().unwrap());
        let (s, warn) = load(&path);
        assert_eq!(warn, None);
        for a in Account::ALL {
            assert_eq!(s.accounts[&a].anchor.map(|x| (x.usd, x.at)), Some((124.0, NOW + 24)), "{a:?}");
        }
        // `balance set` goes through the same lock and replaces only the anchor.
        let (old, s, _) = record_manual(&path, Account::Runpod, FP, 250.0, NOW + 100).unwrap();
        assert_eq!(old.map(|a| (a.usd, a.source)), Some((124.0, AnchorSource::Live)));
        let st = &s.accounts[&Account::Runpod];
        assert_eq!(st.anchor, Some(Anchor { usd: 250.0, at: NOW + 100, source: AnchorSource::Manual }));
        assert_eq!(st.live_ok_at, Some(NOW + 24), "the rest is kept");
        // Another key's `set` starts that account over.
        let (old, s, _) = record_manual(&path, Account::Vast, "deadbeef", 5.0, NOW + 100).unwrap();
        assert_eq!((old, s.accounts[&Account::Vast].live_ok_at), (None, None));
    }

    #[test]
    fn commit_records_or_falls_back_to_memory() {
        let dir = tmp("commit");
        let path = dir.0.join("p").join(crate::snapshot::BALANCE_FILE);
        let f = [fetched(Account::Runpod, Some(Ok(live(7.0, None))), None)];
        let b = burns_of(burn(0.1), None);
        let (s, warn) = commit(Some(&path), BalanceState::default(), &f, &b, Mode::Inline, NOW);
        assert_eq!((warn, load(&path).0), (None, s.clone()));
        // --no-live / cache-only: nothing written, the loaded state handed back.
        std::fs::remove_file(&path).unwrap();
        for mode in [Mode::NoLive, Mode::CacheOnly] {
            assert_eq!(commit(Some(&path), BalanceState::default(), &f, &b, mode, NOW), (BalanceState::default(), None));
            assert!(!path.exists(), "{mode:?} wrote");
        }
        // No state file: applied in memory. An unwritable one: in memory, plus the warning.
        assert_eq!(commit(None, BalanceState::default(), &f, &b, Mode::Inline, NOW).0, s);
        let blocked = dir.0.join("file");
        std::fs::write(&blocked, "x").unwrap();
        let (s2, warn) = commit(Some(&blocked.join("balance.json")), BalanceState::default(), &f, &b, Mode::Inline, NOW);
        assert_eq!(s2, s);
        assert!(warn.unwrap().starts_with("warning: couldn't save the balance state"));
    }

    /// Invariant 8: the canary key, email and address in Vast's answer — and the API keys
    /// themselves — never reach a failure, a `Debug`, a rendered line, JSON or the saved state.
    #[tokio::test]
    async fn no_secret_anywhere() {
        let canary = fixture("vast_user_canary").to_string();
        let mut no_credit = fixture("vast_user_canary");
        no_credit.as_object_mut().unwrap().remove("credit");
        let dir = tmp("secrets");
        let path = dir.0.join("devtest").join(crate::snapshot::BALANCE_FILE);
        let mut seen = String::new();
        let rounds = [
            canned(200, JSON, &canary),
            canned(200, JSON, &no_credit.to_string()),
            canned(401, "text/html", &format!("<html>{canary}</html>")),
            canned(500, JSON, &canary),
        ];
        let mut now = NOW;
        for reply in rounds {
            let (gql, v2, vast) = (serve(vec![canned(200, JSON, &fixture("runpod_myself").to_string())]), serve(vec![canned(200, JSON, &fixture("billing_hourly_openapi").to_string())]), serve(vec![reply]));
            let api = api_at(&gql, &v2, &vast);
            let (state, _) = load(&path);
            let fetched = observe(&api, &state, Mode::Full, COMMAND_TIMEOUT, now).await;
            let b = burns_of(burn(0.5), burn(0.5));
            let (state, _) = commit(Some(&path), state, &fetched, &b, Mode::Full, now);
            let v = views(&state, &fetched, &b, &policy(), Mode::Full, now);
            seen.push_str(&format!("{fetched:?}\n{state:?}\n{v:?}\n{}\n", serde_json::to_string(&v).unwrap()));
            seen.push_str(&std::fs::read_to_string(&path).unwrap());
            seen.push_str(&footer_line(&v, &policy(), now).unwrap_or_default());
            seen.push_str(&super::render_table(&v, now));
            seen.push_str(&notes(&v, &policy(), now).join("\n"));
            seen.push_str(&verbose_lines(&v, &state, &fetched, Some(&path), now).join("\n"));
            seen.push_str(&config_rows(&Config::parse(""), &state, &api.accounts(), now).join("\n"));
            seen.push_str(&teardown_info(&v, now).join("\n"));
            seen.push_str(&format!("{:?}", tui_parts(&v)));
            for view in &v {
                seen.push_str(&preflight_lines(view, 2, Some(0.2), &policy(), now).join("\n"));
            }
            now += 2 * FRESH;
        }
        assert!(seen.contains("136.87") && seen.contains("vast"), "the run did read Vast: {seen}");
        for secret in CANARIES.iter().chain(&[RP_KEY, VAST_KEY, "canary-user", "Canary Owner"]) {
            assert!(!seen.contains(secret), "{secret} leaked");
        }
    }
}
