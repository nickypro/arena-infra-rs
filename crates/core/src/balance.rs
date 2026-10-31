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
//!    hourly billing history (every resource on the account, but it lags: live, a bucket
//!    shows up about an hour after its usage and fills in ~5-min steps; the hour in progress,
//!    and an hour with nothing billed, aren't there at all); *metered* is
//!    arena's own step integral over the burn samples it recorded. RunPod's GraphQL is
//!    reportedly retired in early 2027 and v2 has billing *history* only; this tier keeps the
//!    surfaces honest after that, for up to [`ANCHOR_MAX_AGE`].
//! 3. **?**: nothing usable — said as `?` next to the burn, with a nudge to `arena balance set`.
//!
//! The estimate errs **low** on purpose: the anchor's first billed hour is counted whole,
//! each metered segment is charged at its higher end, spend is taken per UTC hour as the
//! larger of billing and metering (so billing's lag is covered by metering hour by hour and
//! spend arena doesn't list by billing), a running pod nobody priced counts at its last seen
//! price or a stand-in that errs high (never $0), and top-ups are invisible. A wrong estimate
//! therefore warns early, never late — and a later observation never raises it (only a live
//! reading or `set` can). Known undercounts: a pod created *and* destroyed between two
//! observations while billing is unavailable (always the case on Vast), bounded by that
//! pod's lifetime × price; a disk whose size the listing doesn't give (a running RunPod
//! pod's is priced at RunPod's storage rate, a stopped pod's at a stand-in — else the burn is
//! marked a floor); and spend arena doesn't list that falls inside billing's lag (about an
//! hour).
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
use crate::http::Limits;
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

/// What RunPod charged per GPU-hour on its dearest cloud for each GPU it listed, as recorded
/// live on 2026-10-08 (`provider/fixtures/runpod_v2/catalog_community_cuda13.json`, max of
/// community and secure). A known GPU's stand-in is never below this — the placement
/// presets ([`crate::gpu::PRESETS`], from June) are, for five of them; a test keeps every
/// entry at or above the fixture.
pub const RECORDED_PER_GPU: &[(&str, f64)] = &[
    ("AMD Instinct MI300X OAM", 2.39),
    ("AMD Instinct MI350 OAM", 5.49),
    ("NVIDIA A100 80GB PCIe", 1.59),
    ("NVIDIA A100-SXM4-40GB", 1.0),
    ("NVIDIA A100-SXM4-80GB", 1.59),
    ("NVIDIA A40", 0.49),
    ("NVIDIA B200", 6.79),
    ("NVIDIA B300 SXM6 AC", 7.89),
    ("NVIDIA B300 SXM6 AC MIG 1g.34gb", 0.5),
    ("NVIDIA GeForce RTX 3070", 0.13),
    ("NVIDIA GeForce RTX 3080", 0.17),
    ("NVIDIA GeForce RTX 3080 Ti", 0.18),
    ("NVIDIA GeForce RTX 3090", 0.5),
    ("NVIDIA GeForce RTX 3090 Ti", 0.46),
    ("NVIDIA GeForce RTX 4070 Ti", 0.5),
    ("NVIDIA GeForce RTX 4080", 0.5),
    ("NVIDIA GeForce RTX 4090", 0.74),
    ("NVIDIA GeForce RTX 5080", 0.59),
    ("NVIDIA GeForce RTX 5090", 0.99),
    ("NVIDIA H100 80GB HBM3", 3.49),
    ("NVIDIA H100 NVL", 3.19),
    ("NVIDIA H100 PCIe", 2.89),
    ("NVIDIA H200", 4.59),
    ("NVIDIA H200 NVL", 3.79),
    ("NVIDIA L4", 0.49),
    ("NVIDIA L40", 0.82),
    ("NVIDIA L40S", 1.09),
    ("NVIDIA RTX 2000 Ada Generation", 0.5),
    ("NVIDIA RTX 4000 Ada Generation", 0.28),
    ("NVIDIA RTX 4000 SFF Ada Generation", 0.44),
    ("NVIDIA RTX 5000 Ada Generation", 0.83),
    ("NVIDIA RTX 6000 Ada Generation", 0.84),
    ("NVIDIA RTX A2000", 0.5),
    ("NVIDIA RTX A4000", 0.25),
    ("NVIDIA RTX A4500", 0.25),
    ("NVIDIA RTX A5000", 0.27),
    ("NVIDIA RTX A6000", 0.53),
    ("NVIDIA RTX PRO 4000 Blackwell", 0.57),
    ("NVIDIA RTX PRO 4500 Blackwell", 0.72),
    ("NVIDIA RTX PRO 4500 Blackwell Server Edition", 0.72),
    ("NVIDIA RTX PRO 5000 Blackwell", 0.96),
    ("NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition", 1.64),
    ("NVIDIA RTX PRO 6000 Blackwell Server Edition", 2.09),
    ("NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb", 0.59),
    ("NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 2g.48gb", 1.09),
    ("NVIDIA RTX PRO 6000 Blackwell Workstation Edition", 2.19),
    ("Tesla V100-PCIE-16GB", 0.19),
    ("Tesla V100-SXM2-16GB", 0.23),
];

/// A running RunPod pod's disk — container and volume — in $/GB-month, billed on top of the
/// $/h its listing shows (RunPod's pricing, "Storage": $0.10/GB-month for a running pod's
/// container and volume disk; a stopped pod's volume disk $0.20). Live 2026-10-13: a
/// listed $0.13/h RTX 3070 with a 60 GB container disk drained $0.138/h — 60 × $0.10 over a
/// month is $0.008/h — so without it the fallback estimate read high by that much.
pub const RUNPOD_RUNNING_DISK_PER_GB_MONTH: f64 = 0.10;

/// A stopped RunPod pod's **volume** disk, $/GB-month: RunPod's published rate for an idle
/// pod's volume. Its container disk isn't billed while stopped — live 2026-10-13, a stopped
/// pod with a 60 GB container disk and no volume drained $0.00/h.
pub const RUNPOD_STOPPED_PER_GB_MONTH: f64 = 0.20;

/// A stopped Vast instance's disk, $/GB-month: a ceiling — hosts set their own storage price
/// (the recorded search's dearest is $0.33/GB-month), and Vast has no billing history to
/// check against.
pub const VAST_STOPPED_PER_GB_MONTH: f64 = 0.50;

/// Hours in a storage month (Vast's own convention; RunPod divides by ~730 — 720 makes its
/// disks a little dearer per hour, which errs the right way).
const HOURS_PER_MONTH: f64 = 720.0;

/// A burn observed less than this long ago (a listing, or the provider's rate) stands in for
/// a listing that failed now — `$0` included: one throttled Vast listing must not turn an
/// idle account `unknown` (live, a 429 did). Older, only a burn above [`NO_BURN`] stands in,
/// as a floor (seconds).
pub const STALE_BURN_OK: u64 = 30 * 60;

/// Spend the provider's rate shows and the listing doesn't is only reported once two live
/// readings at least this far apart both show it: RunPod's rate lags a create, stop or
/// terminate by minutes (live: $0.14/h for 1–4 min after the last pod was gone) (seconds).
pub const GAP_CONFIRM: u64 = 5 * 60;

/// A live catalog or offer price stays good for a `create`/`up` preflight that can't read
/// one itself for this long (seconds).
pub const QUOTE_FRESH: u64 = 24 * 3_600;

/// `BALANCE_WARN_HOURS` when unset: a Friday-evening glance still covers the weekend.
pub const DEFAULT_WARN_HOURS: f64 = 72.0;

/// `BALANCE_UNPRICED_PER_GPU` when unset: the ceiling $/h per GPU for a running pod whose
/// price nobody reports (RunPod v1's REST list has none; after GraphQL nothing fills it)
/// and whose GPU type isn't a known preset — an H200, a B200/B300, whatever comes next, a
/// MIG slice of one, or no type at all. Above the dearest pod price in RunPod's recorded
/// catalog (B300 SXM6 secure, $7.89/GPU-h; B200 $6.79), so an unknown GPU's burn errs high
/// and the runway short. A known preset GPU is priced at its own dearer preset instead.
pub const DEFAULT_UNPRICED_PER_GPU: f64 = 8.0;

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

/// A prepaid provider account. Hetzner (postpaid) has no balance and is not one. Shadeform is
/// a prepaid wallet; its balance comes from an undocumented endpoint (`GET /account`), so any
/// failure there reads as "unknown" — the estimate and `?` chain — never as $0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Account {
    Runpod,
    Vast,
    Shadeform,
}

impl Account {
    pub const ALL: [Account; 3] = [Account::Runpod, Account::Vast, Account::Shadeform];

    /// The provider's name (`runpod`, `vast`), which is also how the account reads.
    pub fn name(self) -> &'static str {
        match self {
            Account::Runpod => "runpod",
            Account::Vast => "vast",
            Account::Shadeform => "shadeform",
        }
    }

    /// `RunPod`, `Vast` — for sentences.
    pub fn title(self) -> &'static str {
        match self {
            Account::Runpod => "RunPod",
            Account::Vast => "Vast",
            Account::Shadeform => "Shadeform",
        }
    }

    /// The config key holding its API key.
    pub fn key_name(self) -> &'static str {
        match self {
            Account::Runpod => "RUNPOD_API_KEY",
            Account::Vast => "VAST_API_KEY",
            Account::Shadeform => "SHADEFORM_API_KEY",
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
            Account::Shadeform => "what Shadeform does at $0 is undocumented — keep Auto Top Up on",
        }
    }

    /// An opt-in account: its provider isn't in the default provider chain
    /// ([`crate::plan::in_default_chain`]) — Shadeform, overflow made only on `--provider
    /// shadeform` — so an empty wallet there is usually deliberate, not an outage coming
    /// ([`unfunded_overflow`]). RunPod and Vast never are.
    pub fn opt_in(self) -> bool {
        !crate::plan::in_default_chain(self.name())
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
    /// Vast: what its `balance` field says is owed (the magnitude of a negative `balance`).
    /// Shown as a note, never taken off the credit — its meaning isn't confirmed.
    pub owed: Option<f64>,
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

/// RunPod's hourly billing history over a window: `(bucket start, USD)` per bucket. Live
/// (2026-10-13) a bucket appears about an hour after its usage began and fills in ~5-min
/// steps; the hour in progress is missing, as is any hour with nothing billed — so billing
/// never covers the last hour: metering does.
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
        owed: None,
    })
}

/// Why a Vast answer gave no balance — in our words: the body holds the email and API key.
const VAST_NO_CREDIT: &str = "no numeric `credit` in /users/current/ (the answer isn't shown: it holds the account's email and API key)";

/// Vast's `/users/current/`: `credit` (the prepaid amount the console shows) — and,
/// numerically only, a negative `balance` as `owed` (wave 6 surfaced it as "possibly owed";
/// whether it is charges not yet taken from the credit is for the live console comparison
/// to settle, so it is shown, never subtracted). Nothing else is read — not even in the
/// error: the body holds the email and API key.
pub fn parse_vast_user(v: &Value) -> Result<Live, Fail> {
    let owed = v.get("balance").and_then(loose_f64).filter(|b| in_range(*b) && *b < 0.0).map(|b| -b);
    match v.get("credit").and_then(loose_f64) {
        Some(c) if in_range(c) => Ok(Live {
            usd: c,
            provider_burn_per_hr: None,
            spend_limit_per_hr: None,
            under_balance: None,
            reduced: false,
            owed,
        }),
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
    /// Running pods with no price at all — none listed, none remembered, and no GPU to
    /// stand a price in for: the sum is a floor, and the account can't be judged ok.
    pub unpriced: usize,
    /// Stopped pods whose disk still bills but whose disk size isn't listed: the sum is a
    /// floor (a small one).
    pub storage: usize,
    /// Stopped pods whose disk is priced at a stand-in storage rate (errs high).
    pub stopped: usize,
    /// Their part of `per_hr`.
    pub stopped_per_hr: f64,
    /// Running pods priced at a stand-in that errs high (the GPU's preset price, or
    /// `BALANCE_UNPRICED_PER_GPU` per GPU): the burn is shown `~`.
    pub guessed: usize,
    /// The guessed pods' part of `per_hr`.
    pub guessed_per_hr: f64,
    /// Of `guessed`, the pods of an unknown GPU type, priced at the ceiling.
    pub ceiling: usize,
    /// Running RunPod pods' disks at RunPod's storage rate
    /// ([`RUNPOD_RUNNING_DISK_PER_GB_MONTH`]) — billed on top of the listed $/h; part of
    /// `per_hr`.
    pub disk_per_hr: f64,
    /// The last recorded sample, standing in for a listing that failed: possibly out of date.
    pub stale: bool,
    /// A stale burn's age: how long ago it was last observed (seconds). Under
    /// [`STALE_BURN_OK`] it is taken as the burn; older, it is a floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_secs: Option<u64>,
}

impl Burn {
    /// The real burn may be higher than `per_hr` (shown `≥`, runway `≤`).
    pub fn floor(&self) -> bool {
        self.unpriced > 0 || self.storage > 0 || (self.stale && !self.recent())
    }

    /// A stale burn observed under [`STALE_BURN_OK`] ago: taken as the burn, not a floor.
    pub fn recent(&self) -> bool {
        self.stale && self.stale_secs.is_some_and(|s| s < STALE_BURN_OK)
    }

    /// Every pod in it was priced by the listing (or a remembered price): nothing stood in.
    pub fn exact(&self) -> bool {
        self.unpriced == 0 && self.storage == 0 && self.guessed == 0 && self.stopped == 0
    }

    /// The part priced at stand-ins (GPU guesses, stopped disks): what gives way to the
    /// provider's own rate, which covers the whole account.
    fn stand_in_per_hr(&self) -> f64 {
        self.guessed_per_hr + self.stopped_per_hr
    }
}

/// Each prepaid account's burn as the fleet listed it — `None` when its provider failed
/// to list (or wasn't asked): unknown, never zero — and the prices the listing showed per
/// pod id, which [`apply`] remembers so a pod listed later without one keeps its price.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Burns {
    by: BTreeMap<Account, Option<Burn>>,
    /// `(pod id, USD/h)` the listing carried a price for, per account (billing pods only).
    pub seen: BTreeMap<Account, Vec<(String, f64)>>,
    /// Every pod id an answering listing showed, per account: a remembered price stays
    /// alive while its pod keeps being listed, priced or not.
    pub listed_ids: BTreeMap<Account, Vec<String>>,
}

impl Burns {
    pub fn new() -> Self {
        Self::default()
    }

    /// The account's listed burn: `None` = not in these burns, `Some(None)` = unknown.
    pub fn get(&self, account: &Account) -> Option<&Option<Burn>> {
        self.by.get(account)
    }
}

impl std::ops::Index<&Account> for Burns {
    type Output = Option<Burn>;
    fn index(&self, a: &Account) -> &Option<Burn> {
        &self.by[a]
    }
}

impl FromIterator<(Account, Option<Burn>)> for Burns {
    fn from_iter<I: IntoIterator<Item = (Account, Option<Burn>)>>(iter: I) -> Self {
        Burns { by: iter.into_iter().collect(), seen: BTreeMap::new(), listed_ids: BTreeMap::new() }
    }
}

/// `2×RTX A4000` → (Some(2), "RTX A4000"): the count prefix the SSH probe writes.
fn split_count(label: &str) -> (Option<u32>, &str) {
    match label.split_once('×') {
        Some((n, rest)) if !n.is_empty() && n.trim().chars().all(|c| c.is_ascii_digit()) => (n.trim().parse().ok(), rest.trim()),
        _ => (None, label.trim()),
    }
}

/// What a known GPU costs at most per GPU-hour, by everything arena has seen: the dearer of
/// its placement presets (community or secure), RunPod's recorded catalog price
/// ([`RECORDED_PER_GPU`]) and the dearest live catalog/offer price seen for it (`seen`,
/// from `balance.json`). Matched by the provider's type string or a short label (`RTX
/// 3090`, as Vast and the probe say — resolved to RunPod's id). A MIG slice nobody lists is
/// priced as its parent (errs high). `None`: an unknown GPU.
fn known_per_gpu(gpu: &str, seen: &BTreeMap<String, f64>) -> Option<f64> {
    let lookup = |name: &str| -> Option<f64> {
        let name = name.trim();
        let id = crate::gpu::PRESETS
            .iter()
            .find(|p| p.api.eq_ignore_ascii_case(name) || p.label.eq_ignore_ascii_case(name))
            .map_or_else(|| crate::gpu::resolve(name), |p| p.api.to_string());
        let preset = crate::gpu::PRESETS.iter().find(|p| p.api.eq_ignore_ascii_case(&id)).map(|p| p.community.max(p.secure));
        let recorded = RECORDED_PER_GPU.iter().find(|(r, _)| r.eq_ignore_ascii_case(&id)).map(|r| r.1);
        let live = seen.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&id) || k.eq_ignore_ascii_case(name)).map(|(_, v)| *v).reduce(f64::max);
        [preset, recorded, live].into_iter().flatten().reduce(f64::max)
    };
    let g = gpu.trim();
    lookup(g).or_else(|| g.find(" MIG ").and_then(|i| lookup(&g[..i])))
}

/// How a stand-in price was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandIn {
    /// A known GPU: the most it is known to cost ([`RECORDED_PER_GPU`], the presets, live
    /// prices seen).
    Preset,
    /// The GPU type is unknown (or not given): the ceiling, `BALANCE_UNPRICED_PER_GPU`.
    Ceiling,
}

/// [`stand_in_with`] without any live price seen.
pub fn stand_in(gpu_type: Option<&str>, gpu_count: Option<u32>, policy: &BalancePolicy) -> Option<(f64, StandIn)> {
    stand_in_with(gpu_type, gpu_count, policy, &BTreeMap::new())
}

/// A stand-in $/h for a pod (or a planned one) nobody priced, erring high, and how it was
/// found: a known GPU at the most it is known to cost ([`known_per_gpu`] — never below
/// what RunPod's recorded catalog charges); an unknown GPU type (a MIG slice of an unknown
/// parent included), or a GPU count with no type, at the ceiling
/// [`BalancePolicy::unpriced_per_gpu`] per GPU. `None` when there is no GPU information at
/// all — then it can't be priced.
pub fn stand_in_with(
    gpu_type: Option<&str>,
    gpu_count: Option<u32>,
    policy: &BalancePolicy,
    seen: &BTreeMap<String, f64>,
) -> Option<(f64, StandIn)> {
    let (prefix, label) = match gpu_type.map(str::trim).filter(|t| !t.is_empty() && *t != "-") {
        Some(t) => {
            let (n, rest) = split_count(t);
            (n, Some(rest))
        }
        None => (None, None),
    };
    let count = f64::from(gpu_count.filter(|n| *n > 0).or(prefix).or(label.map(|_| 1))?);
    match label.and_then(|l| known_per_gpu(l, seen)) {
        Some(per_gpu) => Some((per_gpu * count, StandIn::Preset)),
        None => Some((policy.unpriced_per_gpu * count, StandIn::Ceiling)),
    }
}

/// [`stand_in`]'s price alone.
pub fn stand_in_price(gpu_type: Option<&str>, gpu_count: Option<u32>, policy: &BalancePolicy) -> Option<f64> {
    stand_in(gpu_type, gpu_count, policy).map(|(p, _)| p)
}

/// Sum the billing pods ([`bills_hourly`]) per prepaid account from a listing (`pods` are
/// its pods, enriched where the caller enriches; `listing` says which providers answered),
/// with no remembered prices and the default stand-in. Surfaces use [`burns_with`].
pub fn burns(listing: &Listing, pods: &[Pod]) -> Burns {
    burns_with(listing, pods, &BalanceState::default(), &BalancePolicy::default())
}

/// [`burns`], pricing each running pod the listing gave no price by, in order: the last
/// price seen for that pod (`state`), a stand-in that errs high ([`stand_in_price`]), else
/// leaving it unpriced (the burn a floor). A stopped pod still bills its disk at a rate the
/// listing doesn't carry: counted as `storage` (a floor).
pub fn burns_with(listing: &Listing, pods: &[Pod], state: &BalanceState, policy: &BalancePolicy) -> Burns {
    let mut out = Burns::new();
    for a in Account::ALL {
        let remembered = state.accounts.get(&a).map(|st| &st.prices);
        out.by.insert(a, listed_burn(listing, pods, a.name(), remembered, &state.gpu_seen, policy));
        if listing.status(a.name()) != ListingStatus::Ok {
            continue;
        }
        let mine = || pods.iter().filter(|p| p.provider == a.name() && !p.id.is_empty());
        // Remember a price only from a pod that bills by the hour (a stopped pod's figure,
        // if any, is its disk's, not what it costs once resumed).
        let seen: Vec<(String, f64)> = mine()
            .filter(|p| bills_hourly(&p.provider, &p.status))
            .filter_map(|p| p.cost_per_hr.filter(|c| c.is_finite() && *c > 0.0).map(|c| (p.id.clone(), c)))
            .collect();
        if !seen.is_empty() {
            out.seen.insert(a, seen);
        }
        out.listed_ids.insert(a, mine().map(|p| p.id.clone()).collect());
    }
    out
}

/// Whether a pod is gone for good (no disk left to bill).
pub(crate) fn gone_for_good(status: &str) -> bool {
    matches!(status.trim().to_ascii_lowercase().as_str(), "terminated" | "destroyed" | "deleted" | "dead")
}

/// GB at a $/GB-month rate, per hour; `None` when the size isn't listed.
pub(crate) fn disk_per_hr(gb: Option<f64>, per_gb_month: f64) -> Option<f64> {
    gb.filter(|g| g.is_finite() && *g >= 0.0).map(|g| g * per_gb_month / HOURS_PER_MONTH)
}

/// What a stopped pod's disk still bills, $/h: on RunPod its volume disk alone (the container
/// disk isn't billed while stopped) at RunPod's idle rate; on Vast the whole disk at a
/// ceiling. `None` when the listing doesn't give the size.
pub(crate) fn stopped_disk_per_hr(p: &Pod) -> Option<f64> {
    match p.provider.as_str() {
        "runpod" => disk_per_hr(p.volume_gb, RUNPOD_STOPPED_PER_GB_MONTH),
        _ => disk_per_hr(p.disk_gb, VAST_STOPPED_PER_GB_MONTH),
    }
}

fn listed_burn(
    listing: &Listing,
    pods: &[Pod],
    provider: &str,
    remembered: Option<&BTreeMap<String, (f64, u64)>>,
    gpu_seen: &BTreeMap<String, f64>,
    policy: &BalancePolicy,
) -> Option<Burn> {
    if listing.status(provider) != ListingStatus::Ok {
        return None;
    }
    let mut b = Burn::default();
    for p in pods.iter().filter(|p| p.provider == provider) {
        if !bills_hourly(&p.provider, &p.status) {
            if !gone_for_good(&p.status) && provider != "hetzner" {
                // A stopped pod still bills its disk: priced at a stand-in storage rate where
                // the listing gives its size (a RunPod pod with no volume: nothing); otherwise
                // counted as a floor.
                match stopped_disk_per_hr(p) {
                    Some(c) if c > 0.0 => {
                        b.per_hr += c;
                        b.stopped += 1;
                        b.stopped_per_hr += c;
                    }
                    Some(_) => {}
                    None => b.storage += 1,
                }
            }
            continue;
        }
        // A running RunPod pod's disk bills on top of its listed (or remembered, or stand-in
        // GPU) price.
        if provider == "runpod" {
            if let Some(d) = disk_per_hr(p.disk_gb, RUNPOD_RUNNING_DISK_PER_GB_MONTH) {
                b.per_hr += d;
                b.disk_per_hr += d;
            }
        }
        let price = p.cost_per_hr.filter(|c| c.is_finite() && *c >= 0.0);
        let remembered = || remembered.and_then(|m| m.get(&p.id)).map(|(c, _)| *c);
        // GPU stand-ins are for the prepaid GPU clouds; a Hetzner server type isn't a GPU.
        let stand_in = || (provider != "hetzner").then(|| stand_in_with(p.gpu_type.as_deref(), p.gpu_count, policy, gpu_seen)).flatten();
        match price.or_else(remembered) {
            Some(c) => b.per_hr += c,
            None => match stand_in() {
                Some((g, how)) => {
                    b.per_hr += g;
                    b.guessed += 1;
                    b.guessed_per_hr += g;
                    if how == StandIn::Ceiling {
                        b.ceiling += 1;
                    }
                }
                None => b.unpriced += 1,
            },
        }
    }
    Some(b)
}

/// The burn to judge by (`B` in the spec). With a listing and the provider's own rate:
/// the higher of the rate and the pods the listing priced (the rate covers everything on
/// the account, so stand-in guesses and floors give way to it). With a listing alone: its
/// sum — and where running pods couldn't be priced at all, at least the last recorded
/// burn (those pods aren't free). Without a listing: the provider's rate, else the burn
/// last observed (`last`: when, USD/h), marked stale — taken as the burn when observed
/// under [`STALE_BURN_OK`] ago (an idle account stays idle through one failed listing), a
/// floor when older; but an old $0 says nothing about now: unknown. Never 0 for pods that
/// run.
pub fn burn_now(listed: Option<Burn>, provider_rate: Option<f64>, last: Option<(u64, f64)>, now: u64) -> Option<Burn> {
    match (listed, provider_rate) {
        (Some(l), Some(p)) => Some(Burn { per_hr: (l.per_hr - l.stand_in_per_hr()).max(p), ..Default::default() }),
        (Some(l), None) if l.unpriced > 0 => Some(Burn { per_hr: l.per_hr.max(last.map_or(0.0, |x| x.1)), ..l }),
        (Some(l), None) => Some(l),
        (None, Some(p)) => Some(Burn { per_hr: p, ..Default::default() }),
        (None, None) => {
            let (at, b) = last?;
            let age = now.saturating_sub(at);
            (age < STALE_BURN_OK || b >= NO_BURN).then_some(Burn { per_hr: b, stale: true, stale_secs: Some(age), ..Default::default() })
        }
    }
}

/// The last burn in `samples`, and when it was last observed: its own time, or `seen_at`
/// — the last observation, which records no sample while the burn holds steady.
fn last_seen(samples: &[(u64, f64)], seen_at: Option<u64>) -> Option<(u64, f64)> {
    samples.last().map(|&(t, b)| (seen_at.map_or(t, |s| s.max(t)), b))
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
    /// Vast's owed amount, as read ([`Live::owed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owed: Option<f64>,
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

/// One fallback check: what the fallback — counting down from the check anchor by the
/// listing's burns alone, as it will once GraphQL is gone — predicted against the live
/// reading, over `horizon_secs`: with this run's billing history (`predicted`) and with
/// metering only (`predicted_metered`, what it says when billing is down too). The verdict
/// is the worse of the two.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRecord {
    pub at: u64,
    pub horizon_secs: u64,
    pub predicted: f64,
    #[serde(default)]
    pub predicted_metered: f64,
    pub live: f64,
    pub verdict: Verdict,
}

impl CheckRecord {
    /// The higher (worse) of the two predictions.
    pub fn worst(&self) -> f64 {
        self.predicted.max(self.predicted_metered)
    }
}

/// The most billing has ever said was spent in each hour since the anchor at `anchor_at`:
/// a later billing failure (or a short window, or a bucket read lower) can't make the
/// estimate rise again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BilledMax {
    pub anchor_at: u64,
    /// `(hour start, USD)`, ascending.
    pub buckets: Vec<(u64, f64)>,
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
    /// heartbeat — what metering integrates. The burn as judged: the provider's own rate
    /// while it is read, stand-in prices where nothing priced a pod.
    #[serde(default)]
    pub samples: Vec<(u64, f64)>,
    /// The same, from the listing alone — what the fallback will have once the provider's
    /// rate is gone: the fallback check meters with these.
    #[serde(default)]
    pub listed_samples: Vec<(u64, f64)>,
    /// When the burn (`samples`) was last observed — a sample is only recorded when it
    /// changes, so this says how fresh the last one still is ([`burn_now`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_seen_at: Option<u64>,
    /// The same for `listed_samples`: when a listing last answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed_seen_at: Option<u64>,
    /// Since when every live reading has shown the provider's rate above the listing's exact
    /// burn ([`GAP_CONFIRM`]): the first of an unbroken run of such readings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_since: Option<u64>,
    /// The last price seen for each pod (`pod id → (USD/h, when seen)`): a pod later listed
    /// without one (RunPod v1's REST list, no details) keeps it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, (f64, u64)>,
    /// When a sample last counted a pod at a stand-in price (estimates from then are marked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stand_in_at: Option<u64>,
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
    /// The dearest $/GPU-h a live catalog or offer search showed for each GPU type (`arena
    /// offers`, `create`/`up` placement): a known GPU's stand-in is never below it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_seen: BTreeMap<String, f64>,
    /// The latest live price per GPU-hour for each provider, GPU and tier ([`quote_key`]:
    /// `runpod|NVIDIA RTX A4000|COMMUNITY`) and when it was read — what a `create`/`up`
    /// preflight that can't read one itself goes by, for [`QUOTE_FRESH`]. `gpu_seen` is the
    /// dearest over every tier and time (a stand-in errs high); this is the price on the
    /// tier asked for, now.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_quotes: BTreeMap<String, (f64, u64)>,
}

impl Default for BalanceState {
    fn default() -> Self {
        BalanceState { version: STATE_VERSION, accounts: BTreeMap::new(), gpu_seen: BTreeMap::new(), gpu_quotes: BTreeMap::new() }
    }
}

impl BalanceState {
    /// `account`'s entry, if it was recorded with this key.
    pub fn get(&self, account: Account, key_fp: &str) -> Option<&AccountState> {
        self.accounts.get(&account).filter(|s| s.key_fp == key_fp)
    }

    /// The live price for `gpu` on `cloud` at `provider` read under [`QUOTE_FRESH`] ago:
    /// (USD per GPU-hour, its age in seconds). `cloud` is `None` on a provider without tiers.
    pub fn fresh_quote(&self, provider: &str, gpu: &str, cloud: Option<&str>, now: u64) -> Option<(f64, u64)> {
        let &(usd, at) = self.gpu_quotes.get(&quote_key(provider, gpu, cloud))?;
        let age = now.saturating_sub(at);
        (at <= now.saturating_add(CLOCK_SKEW) && age < QUOTE_FRESH).then_some((usd, age))
    }
}

/// A live price a catalog or offer search showed for one GPU on one tier (`cloud`: `None`
/// on a provider without tiers), per GPU-hour.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuQuote {
    pub provider: String,
    pub gpu: String,
    pub cloud: Option<String>,
    pub per_gpu: f64,
}

/// `gpu_quotes`' key: `provider|gpu|TIER` (`-` without a tier).
pub fn quote_key(provider: &str, gpu: &str, cloud: Option<&str>) -> String {
    let tier = cloud.map(|c| c.trim().to_ascii_uppercase()).filter(|c| !c.is_empty()).unwrap_or_else(|| "-".into());
    format!("{}|{}|{tier}", provider.trim(), crate::gpu::resolve(gpu.trim()))
}

// ---------------------------------------------------------------------------------------
// Estimates, runway, levels
// ---------------------------------------------------------------------------------------

/// Spend from `anchor_at` to `now` by arena's own metering, per UTC hour: a step integral
/// over the burn points — `(anchor_at, b_A)`, the samples strictly between, `(now, b_now)` —
/// each segment charged at the **higher** of its two ends (a change between two observations
/// is never undercounted) and split at hour boundaries. `b_A` is the last sample at/before
/// the anchor, else the first after, else `b_now`. A clock that went backwards adds nothing.
pub fn metered_by_hour(anchor_at: u64, samples: &[(u64, f64)], now: u64, b_now: f64) -> BTreeMap<u64, f64> {
    let b_a = samples
        .iter()
        .rev()
        .find(|(t, _)| *t <= anchor_at)
        .or_else(|| samples.iter().find(|(t, _)| *t > anchor_at))
        .map_or(b_now, |s| s.1);
    let mut points = vec![(anchor_at, b_a)];
    points.extend(samples.iter().filter(|(t, _)| *t > anchor_at && *t < now).copied());
    points.push((now, b_now));
    let mut out: BTreeMap<u64, f64> = BTreeMap::new();
    for w in points.windows(2) {
        let rate = w[0].1.max(w[1].1).max(0.0);
        let (mut t, end) = (w[0].0, w[1].0);
        while t < end {
            let hour = floor_hour(t);
            let stop = (hour + 3_600).min(end);
            *out.entry(hour).or_default() += (stop - t) as f64 / 3_600.0 * rate;
            t = stop;
        }
    }
    out
}

/// [`metered_by_hour`]'s total.
pub fn metered_spend(anchor_at: u64, samples: &[(u64, f64)], now: u64, b_now: f64) -> f64 {
    metered_by_hour(anchor_at, samples, now, b_now).values().sum()
}

/// The billing history per hour from `t`'s hour on (that first hour whole), summed per
/// bucket; `None` when the window starts after that hour (it can't say).
pub fn billed_hours(b: &Billed, t: u64) -> Option<BTreeMap<u64, f64>> {
    let h = floor_hour(t);
    if b.from > h {
        return None;
    }
    let mut out: BTreeMap<u64, f64> = BTreeMap::new();
    for (start, usd) in b.buckets.iter().filter(|(start, _)| *start >= h) {
        *out.entry(floor_hour(*start)).or_default() += usd;
    }
    Some(out)
}

/// The higher of two readings per hour.
fn max_per_hour(a: &BTreeMap<u64, f64>, b: &BTreeMap<u64, f64>) -> BTreeMap<u64, f64> {
    let mut out = a.clone();
    for (h, v) in b {
        let e = out.entry(*h).or_insert(*v);
        *e = e.max(*v);
    }
    out
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
    /// Billing's total over the same hours, when there was a billing history.
    pub billed: Option<f64>,
}

/// The anchor minus the spend since: per UTC hour the **higher** of what billing and
/// metering say (`billed` per hour from the anchor's hour, which billing counts whole), so
/// billing's lag is covered by metering hour by hour and spend arena doesn't list by
/// billing — never one total traded for the other. No billing: metered alone. Pure.
pub fn estimate(anchor: &Anchor, samples: &[(u64, f64)], billed: Option<&BTreeMap<u64, f64>>, b_now: f64, now: u64) -> Estimate {
    let metered_h = metered_by_hour(anchor.at, samples, now, b_now);
    let metered: f64 = metered_h.values().sum();
    let Some(billed_h) = billed else {
        return Estimate { usd: anchor.usd - metered, method: Method::Metered, metered, billed: None };
    };
    let first = floor_hour(anchor.at);
    let billed_h: BTreeMap<u64, f64> = billed_h.iter().filter(|(h, _)| **h >= first && **h <= now).map(|(h, v)| (*h, *v)).collect();
    let spend: f64 = max_per_hour(&metered_h, &billed_h).values().sum();
    let method = if spend > metered + 1e-9 { Method::Billed } else { Method::Metered };
    Estimate { usd: anchor.usd - spend, method, metered, billed: Some(billed_h.values().sum()) }
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
/// two); unknown: no balance while burning (or with the burn unknown, or a floor), and —
/// short of critical — running pods nobody could price (the runway is only an upper
/// bound); else ok, a known zero burn included. A floor is never "no burn".
pub fn level(
    balance: Option<f64>,
    under_balance: Option<bool>,
    burn: Option<&Burn>,
    runway_hours: Option<f64>,
    policy: &BalancePolicy,
) -> Level {
    let idle = burn.is_some_and(|b| b.per_hr < NO_BURN && !b.floor());
    let Some(b) = balance else {
        return if idle { Level::Ok } else { Level::Unknown };
    };
    if b <= 0.0 || under_balance == Some(true) {
        return Level::Critical;
    }
    // No burn known at all (its listing failed, no rate, no usable sample): no warning can
    // be judged — never "ok" on less information than a floor would give.
    let Some(burn) = burn else { return Level::Unknown };
    if let Some(h) = runway_hours.filter(|_| policy.warn_hours > 0.0) {
        if h < CRITICAL_HOURS {
            return Level::Critical;
        }
    }
    let floor_unjudged = burn.unpriced > 0 || (burn.floor() && burn.per_hr < NO_BURN);
    if floor_unjudged {
        return Level::Unknown;
    }
    match runway_hours {
        Some(h) if h < policy.warn_hours => Level::Low,
        _ => Level::Ok,
    }
}

/// The warning threshold and the stand-in price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BalancePolicy {
    pub warn_hours: f64,
    /// $/h per GPU for a running pod nobody priced and whose GPU has no preset price.
    pub unpriced_per_gpu: f64,
}

impl Default for BalancePolicy {
    fn default() -> Self {
        BalancePolicy { warn_hours: DEFAULT_WARN_HOURS, unpriced_per_gpu: DEFAULT_UNPRICED_PER_GPU }
    }
}

impl BalancePolicy {
    /// From `BALANCE_WARN_HOURS` and `BALANCE_UNPRICED_PER_GPU`. Never fails: a bad value
    /// uses its default and comes back in the one warning line to show (in `arena balance`;
    /// `config check` marks it ✗) — a typo must never break `pods list`.
    pub fn from_config(cfg: &Config) -> (Self, Option<String>) {
        let mut policy = BalancePolicy::default();
        let mut bad = Vec::new();
        match warn_hours(cfg) {
            Ok(h) => policy.warn_hours = h,
            Err(e) => bad.push(format!("{e} — using {DEFAULT_WARN_HOURS}")),
        }
        match unpriced_per_gpu(cfg) {
            Ok(p) => policy.unpriced_per_gpu = p,
            Err(e) => bad.push(format!("{e} — using {DEFAULT_UNPRICED_PER_GPU}")),
        }
        (policy, (!bad.is_empty()).then(|| format!("warning: {}", bad.join("; "))))
    }
}

/// `BALANCE_UNPRICED_PER_GPU` as written: unset/empty → [`DEFAULT_UNPRICED_PER_GPU`]; a
/// finite USD/h per GPU above 0 and at most 1000.
pub fn unpriced_per_gpu(cfg: &Config) -> Result<f64, String> {
    match cfg.get("BALANCE_UNPRICED_PER_GPU").map(str::trim) {
        None | Some("") => Ok(DEFAULT_UNPRICED_PER_GPU),
        Some(v) => match v.trim_start_matches('$').parse::<f64>() {
            Ok(p) if p.is_finite() && p > 0.0 && p <= 1_000.0 => Ok(p),
            _ => {
                let shown = if v.chars().count() <= 12 { format!("`{v}`") } else { "a long value".into() };
                Err(format!("BALANCE_UNPRICED_PER_GPU must be USD per GPU-hour, above 0 and at most 1000 (got {shown})"))
            }
        },
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
        let seen = burns.seen.get(&f.account).map(Vec::as_slice).unwrap_or_default();
        let listed = burns.listed_ids.get(&f.account).map(Vec::as_slice).unwrap_or_default();
        apply_one(st, f, burns.get(&f.account).copied().flatten(), seen, listed, mode, now);
    }
    state
}

/// The failure record after `f`: `since` kept from an ongoing failure.
fn fail_record(prev: Option<&FailRecord>, f: &Fail, now: u64) -> FailRecord {
    FailRecord { kind: f.kind, http: f.http, detail: f.detail.clone(), since: prev.map_or(now, |p| p.since.min(now)), last: now }
}

/// Forget what a clock that ran ahead left behind: samples dated past `now` (plus the
/// skew allowance) would block every later sample until real time caught up with them.
fn sanitize(st: &mut AccountState, now: u64) {
    let limit = now.saturating_add(CLOCK_SKEW);
    st.samples.retain(|(t, _)| *t <= limit);
    st.listed_samples.retain(|(t, _)| *t <= limit);
    for (_, at) in st.prices.values_mut() {
        *at = (*at).min(now);
    }
    for t in [&mut st.burn_seen_at, &mut st.listed_seen_at, &mut st.gap_since].into_iter().flatten() {
        *t = (*t).min(now);
    }
}

fn apply_one(st: &mut AccountState, f: &Fetched, listed: Option<Burn>, seen: &[(String, f64)], listed_ids: &[String], mode: Mode, now: u64) {
    sanitize(st, now);
    // A remembered price is the pod's own: it goes SAMPLE_KEEP after the pod was last
    // listed, and until then each listing of the pod — priced or not — refreshes it.
    st.prices.retain(|_, (_, at)| now.saturating_sub(*at) <= SAMPLE_KEEP);
    for id in listed_ids {
        if let Some((_, at)) = st.prices.get_mut(id) {
            *at = now;
        }
    }
    for (id, usd) in seen {
        st.prices.insert(id.clone(), (*usd, now));
    }

    let live = f.live_ok();
    let billed = f.billed_ok();
    let provider_rate = match live {
        Some(l) => l.provider_burn_per_hr,
        None => st.fresh_facts(now).and_then(|p| p.burn_per_hr),
    };
    let burn = burn_now(listed, provider_rate, last_seen(&st.samples, st.burn_seen_at), now);
    // What the fallback will have once the provider's rate is gone: the listing alone.
    let last_listed = last_seen(&st.listed_samples, st.listed_seen_at);
    let listed_burn = burn_now(listed, None, last_listed, now).filter(|b| !b.stale);

    // The fallback check (`arena balance` only): what the fallback would say now, counting
    // down from the check anchor by the listing's burns alone (the samples recorded at the
    // provider's own rate aren't what it will have) — with this run's billing, and with
    // metering only — against the truth just read. Either reading high is "overstates".
    if let (Mode::Full, Some(l)) = (mode, live) {
        let anchor = st.check_anchor.filter(|a| a.usable(now) && now.saturating_sub(a.at) >= CHECK_MIN);
        let b_now = listed_burn.or_else(|| burn_now(None, None, last_listed, now));
        if let (Some(ca), Some(b)) = (anchor, b_now) {
            let hours = billed.and_then(|bl| billed_hours(bl, ca.at));
            let with_billing = estimate(&ca, &st.listed_samples, hours.as_ref(), b.per_hr, now).usd;
            let metered_only = estimate(&ca, &st.listed_samples, None, b.per_hr, now).usd;
            let tolerance = (0.05 * l.usd.abs()).max(1.0);
            let verdict = if with_billing.max(metered_only) <= l.usd + tolerance { Verdict::Safe } else { Verdict::Overstates };
            st.check = Some(CheckRecord {
                at: now,
                horizon_secs: now.saturating_sub(ca.at),
                predicted: with_billing,
                predicted_metered: metered_only,
                live: l.usd,
                verdict,
            });
        }
    }

    match &f.live {
        Some(Ok(l)) => {
            // The provider's rate above an exact listing is spend arena doesn't list — once a
            // second reading GAP_CONFIRM later still shows it (the rate lags changes). A
            // reading with no listing (the preflight's) can't say either way.
            if let Some(b) = listed {
                let gap = l.provider_burn_per_hr.is_some_and(|p| b.exact() && p > b.per_hr + 0.01);
                st.gap_since = if gap { st.gap_since.or(Some(now)) } else { None };
            }
            let anchor = Anchor { usd: l.usd, at: now, source: AnchorSource::Live };
            st.anchor = Some(anchor);
            st.provider = Some(ProviderFacts {
                usd: l.usd,
                burn_per_hr: l.provider_burn_per_hr,
                spend_limit_per_hr: l.spend_limit_per_hr,
                under_balance: l.under_balance,
                at: now,
                owed: l.owed,
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
            if let Some((at, hours)) = st.anchor.and_then(|a| billed_hours(b, a.at).map(|h| (a.at, h))) {
                // Per hour, the most billing has ever said for this anchor.
                let seen: BTreeMap<u64, f64> =
                    st.billed_max.as_ref().filter(|m| m.anchor_at == at).map(|m| m.buckets.iter().copied().collect()).unwrap_or_default();
                let merged = max_per_hour(&seen, &hours);
                st.billed_max = Some(BilledMax { anchor_at: at, buckets: merged.into_iter().collect() });
            }
        }
        Some(Err(e)) => st.billing_fail = Some(fail_record(st.billing_fail.as_ref(), e, now)),
        None => {}
    }
    if st.billed_max.as_ref().is_some_and(|m| st.anchor.is_none_or(|a| a.at != m.anchor_at)) {
        st.billed_max = None;
    }

    // A sample only from a real reading (a listing, or the provider's rate this run) —
    // never the stale stand-in, which would only repeat itself. Stand-in prices are
    // recorded as they are (they err high), and flagged.
    if let Some(b) = burn.filter(|b| !b.stale) {
        push_sample(&mut st.samples, now, b.per_hr);
        st.burn_seen_at = Some(now);
        if b.guessed > 0 {
            st.stand_in_at = Some(now);
        }
    }
    if let Some(b) = listed_burn {
        push_sample(&mut st.listed_samples, now, b.per_hr);
        st.listed_seen_at = Some(now);
    }
    // Never thin what an anchor in use still needs.
    let keep_since = [st.anchor, st.check_anchor].into_iter().flatten().filter(|a| a.usable(now)).map(|a| a.at).min();
    prune_keeping(&mut st.samples, now, keep_since);
    prune_keeping(&mut st.listed_samples, now, keep_since);
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
        Some((t, _)) if now <= t => {} // same second (a clock from the future is sanitised first)
        Some((t, last)) => {
            let d = b - last;
            if d > BURN_NOISE || -d > 0.01 || -d > 0.01 * last.abs() || now - t >= SAMPLE_HEARTBEAT {
                samples.push((now, b));
            }
        }
    }
}

/// [`prune_keeping`] with no anchor to protect.
#[cfg(test)]
fn prune(samples: &mut Vec<(u64, f64)>, now: u64) {
    prune_keeping(samples, now, None);
}

/// Drop samples older than [`SAMPLE_KEEP`] — never one an anchor from `keep_since` still
/// needs, and always keeping the newest of the dropped as a baseline — then, over
/// [`SAMPLE_MAX`], **coalesce** the oldest neighbours pairwise into one sample at the
/// earlier time with the higher burn. Merging that way only ever raises the metered spend
/// (each merged segment is charged at the higher end), so thinning never raises an
/// estimate; dropping the oldest would meter an old anchor at today's burn.
fn prune_keeping(samples: &mut Vec<(u64, f64)>, now: u64, keep_since: Option<u64>) {
    let cutoff = now.saturating_sub(SAMPLE_KEEP).min(keep_since.unwrap_or(u64::MAX));
    if let Some(i) = samples.iter().rposition(|&(t, _)| t <= cutoff) {
        samples.drain(..i);
    }
    while samples.len() > SAMPLE_MAX {
        let n = (2 * (samples.len() - SAMPLE_MAX)).min(samples.len() & !1);
        let merged: Vec<(u64, f64)> = samples[..n].chunks(2).map(|c| (c[0].0, c[0].1.max(c[1].1))).collect();
        samples.splice(..n, merged);
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
    /// `runpod`, `vast`, `shadeform` or `hetzner`.
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
    /// The fleet's billing pods there, as listed — stand-in prices included (`None`: the
    /// listing failed).
    pub listed_per_hr: Option<f64>,
    /// The listed burn priced every pod from the listing (or a remembered price): no
    /// stand-in, nothing unpriced. Only then is the provider's rate above it spend arena
    /// doesn't list.
    pub listed_exact: bool,
    /// Of `listed_per_hr`, running RunPod pods' disks ([`Burn::disk_per_hr`]).
    pub listed_disk_per_hr: f64,
    pub provider_burn_per_hr: Option<f64>,
    pub spend_limit_per_hr: Option<f64>,
    pub under_balance: Option<bool>,
    /// When the provider's extras shown (spend limit, owed) were read, when that wasn't now:
    /// a cached reading, said with its age.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts_at: Option<u64>,
    /// The provider's rate has been above the listing for two live readings
    /// [`GAP_CONFIRM`] apart: the gap is spend arena doesn't list, not the rate's lag.
    pub gap_confirmed: bool,
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
    /// The estimate's metering counted pods at stand-in prices since its anchor.
    pub stand_in: bool,
    /// Vast's `balance` reads as this much owed (shown, never taken off).
    pub owed: Option<f64>,
    /// Hetzner: the backup archive's share of `burn` (EUR/h, its server and volumes — billed to
    /// the account, never a fleet pod: the fleet listing leaves it out), when `arena balance`
    /// read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_per_hr: Option<f64>,
    /// Hetzner: why the archive's cost couldn't be read (it then isn't in `burn`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_unread: Option<String>,
    /// Hetzner: the capacity spares' share of `burn` (`pods spare up`: servers in no fleet
    /// listing, billing like any), when `arena balance` read them and there are some.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spares_per_hr: Option<f64>,
    /// Hetzner: why the spares couldn't be read (they then aren't in `burn`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spares_unread: Option<String>,
    /// Hetzner: how many billing spares had no price listed (their share isn't in `burn`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spares_unpriced: Option<usize>,
    /// Hetzner: the fleet VMs' volumes' share of `burn` (`pods volume add`: billed by the
    /// GB-month whether attached or not), when `arena balance` read them and there are some.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes_per_hr: Option<f64>,
    /// Hetzner: why the volumes couldn't be read (they then aren't in `burn`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes_unread: Option<String>,
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
            listed_exact: false,
            listed_disk_per_hr: 0.0,
            provider_burn_per_hr: None,
            spend_limit_per_hr: None,
            under_balance: None,
            facts_at: None,
            gap_confirmed: false,
            runway_hours: None,
            runway_upper_bound: false,
            level: Level::Unknown,
            live_failing: None,
            noted: false,
            live_reduced: false,
            check: None,
            stand_in: false,
            owed: None,
            archive_per_hr: None,
            archive_unread: None,
            spares_per_hr: None,
            spares_unread: None,
            spares_unpriced: None,
            volumes_per_hr: None,
            volumes_unread: None,
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
            owed: l.owed,
        }),
        None if mode != Mode::NoLive => st.fresh_facts(now),
        None => None,
    };
    let burn = burn_now(listed, facts.and_then(|p| p.burn_per_hr), last_seen(&st.samples, st.burn_seen_at), now);
    let mut v = AccountView::blank(f.account.name(), "USD", Source::Unknown);
    v.listed_per_hr = listed.map(|b| b.per_hr);
    v.listed_exact = listed.is_some_and(|b| b.exact());
    v.listed_disk_per_hr = listed.map_or(0.0, |b| b.disk_per_hr);
    v.provider_burn_per_hr = facts.and_then(|p| p.burn_per_hr);
    // Vast's owed figure: this reading's, else the last one's.
    v.owed = facts.and_then(|p| p.owed).or(st.provider.and_then(|p| p.owed));
    v.spend_limit_per_hr = facts.and_then(|p| p.spend_limit_per_hr).or(st.provider.and_then(|p| p.spend_limit_per_hr));
    v.under_balance = facts.and_then(|p| p.under_balance);
    // Extras from an older reading than the one shown (or with none shown) carry its time.
    v.facts_at = match (facts, st.provider) {
        (Some(p), _) if p.at < now => Some(p.at),
        (None, Some(p)) => Some(p.at),
        _ => None,
    };
    // Two readings GAP_CONFIRM apart showed it: the first of the run, and the one shown.
    v.gap_confirmed = st.gap_since.zip(facts.map(|p| p.at)).is_some_and(|(t, at)| at.saturating_sub(t) >= GAP_CONFIRM);
    v.live_failing = if live_now.is_some() { None } else { st.live_fail.clone() };
    v.noted = v.live_failing.as_ref().is_some_and(|fr| noted(fr, now));
    v.live_reduced = live_now.map_or(st.live_reduced.is_some(), |l| l.reduced);
    v.check = st.check;
    if let Some(p) = facts {
        v.source = Source::Live;
        v.balance = Some(p.usd);
        v.as_of = Some(p.at);
    } else if let (Some(a), Some(b)) = (st.anchor.filter(|a| a.usable(now)), burn) {
        // Per hour, the most billing has said — this run, or ever before for this anchor.
        let billed_now = f.billed_ok().and_then(|bl| billed_hours(bl, a.at));
        let billed_seen: Option<BTreeMap<u64, f64>> =
            st.billed_max.as_ref().filter(|m| m.anchor_at == a.at).map(|m| m.buckets.iter().copied().collect());
        let billed = match (billed_now, billed_seen) {
            (Some(x), Some(y)) => Some(max_per_hour(&x, &y)),
            (x, y) => x.or(y),
        };
        let e = estimate(&a, &st.samples, billed.as_ref(), b.per_hr, now);
        v.source = Source::Estimate;
        v.balance = Some(e.usd);
        v.as_of = Some(a.at);
        v.anchor_source = Some(a.source);
        v.method = Some(e.method);
        v.stand_in = st.stand_in_at.is_some_and(|t| t >= a.at) || b.guessed > 0;
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
    v.burn = listed_burn(listing, pods, "hetzner", None, &BTreeMap::new(), &BalancePolicy::default());
    v.listed_per_hr = v.burn.map(|b| b.per_hr);
    v.level = Level::None;
    Some(v)
}

/// Add the backup archive's cost ([`BalanceApi::archive_eur_per_hr`]) to Hetzner's row: it
/// bills the same account, so it is in the burn — as its own named part, never as a fleet
/// pod. `Ok(None)` (no archive) changes nothing; a failed read is said and left out. A burn
/// already unknown (the listing failed) stays unknown.
pub fn add_archive(v: &mut AccountView, read: Result<Option<f64>, String>) {
    match read {
        Ok(Some(per_hr)) if per_hr.is_finite() && per_hr >= 0.0 => {
            v.archive_per_hr = Some(per_hr);
            if let Some(b) = v.burn.as_mut() {
                b.per_hr += per_hr;
            }
        }
        Ok(_) => {}
        Err(e) => v.archive_unread = Some(e),
    }
}

/// Add the fleet VMs' volumes' cost ([`BalanceApi::volumes_eur_per_hr`]) to Hetzner's row, as
/// [`add_archive`] does the archive's. None (or no cost) changes nothing; a failed read is said
/// and left out.
pub fn add_volumes(v: &mut AccountView, read: Result<f64, String>) {
    match read {
        Ok(per_hr) if per_hr.is_finite() && per_hr > 0.0 => {
            v.volumes_per_hr = Some(per_hr);
            if let Some(b) = v.burn.as_mut() {
                b.per_hr += per_hr;
            }
        }
        Ok(_) => {}
        Err(e) => v.volumes_unread = Some(e),
    }
}

/// Add the capacity spares' cost ([`BalanceApi::spares_eur_per_hr`]: the priced ones' sum, and
/// how many bill with no price listed) to Hetzner's row, as [`add_archive`] does the archive's:
/// billed by the same account, in no fleet listing. None (or no cost) changes nothing; unpriced
/// spares and a failed read are said, and left out.
pub fn add_spares(v: &mut AccountView, read: Result<(f64, usize), String>) {
    match read {
        Ok((per_hr, unpriced)) => {
            if per_hr.is_finite() && per_hr > 0.0 {
                v.spares_per_hr = Some(per_hr);
                if let Some(b) = v.burn.as_mut() {
                    b.per_hr += per_hr;
                }
            }
            v.spares_unpriced = (unpriced > 0).then_some(unpriced);
        }
        Err(e) => v.spares_unread = Some(e),
    }
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

/// Below half a cent, an amount or a rate is zero here ([`usd`], [`money`]): an empty `f64`
/// sum is `-0.0`, and a difference of exactly zero negated is too — live, `balance -v`
/// printed `billing: 0 hourly bucket(s), $-0.00 in all` and `the estimate read $-0.00 low`.
fn norm(x: f64) -> f64 {
    if x.abs() < 0.005 {
        0.0
    } else {
        x
    }
}

/// `$3.10`, `-$2.00` (never `$-2.00`, never `$-0.00`).
fn usd(v: f64) -> String {
    let v = norm(v);
    if v < 0.0 {
        format!("-{}", fmt_money("$", -v))
    } else {
        fmt_money("$", v)
    }
}

fn money(v: &AccountView, x: f64) -> String {
    if v.currency == "EUR" {
        fmt_money("€", norm(x))
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
/// burn` (a known zero only), or `?` when the balance or the burn is unknown — a floor
/// below a cent an hour included: pods nobody priced aren't free.
pub fn fmt_runway(v: &AccountView) -> String {
    match (v.balance, v.burn, v.runway_hours) {
        (Some(_), Some(b), _) if b.per_hr < NO_BURN && !b.floor() => "no burn".into(),
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
            if v.stand_in {
                why.push("stand-in prices".into());
            }
            format!("est. ({})", why.join(", "))
        }
        Source::Unknown => "?".into(),
        Source::Postpaid => "postpaid".into(),
    }
}

/// The BURN cell: `$0.47/h`, `~$3.47/h` (stand-in prices in it), `≥$0.47/h` (a floor), `?`.
pub fn burn_label(v: &AccountView) -> String {
    match v.burn {
        Some(b) => {
            let mark = if b.floor() { "≥" } else if b.guessed > 0 || b.stopped > 0 { "~" } else { "" };
            format!("{mark}{}/h", money(v, b.per_hr))
        }
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

/// An opt-in account ([`Account::opt_in`]: Shadeform) at $0 with **nothing billing there** —
/// a known zero burn, not an unknown one or a floor. Overflow is chosen per create, so its
/// wallet sits unfunded on purpose: the inline surfaces (the `pods list` footer, the TUI bar)
/// show it as `shadeform $0.00 unfunded` among the accounts that need nothing — no ‼, no lead,
/// no tone, no top-up advice (live, every `pods list` ended in `‼ shadeform $0.00 live no burn
/// — … keep Auto Top Up on`, and the TUI bar led with it). Once a pod bills there (burn > 0)
/// it alarms like any other. Never RunPod or Vast: at $0 RunPod stops (or terminates) the
/// pods, so their burn drops to zero exactly when the alarm matters. `arena balance`,
/// `balance --check`, `teardown --check` and the create/up preflight judge the account as is.
fn unfunded_overflow(v: &AccountView) -> bool {
    v.level == Level::Critical
        && Account::from_name(&v.account).is_some_and(Account::opt_in)
        && v.burn.is_some_and(|b| b.per_hr < NO_BURN && !b.floor())
}

/// The level an inline surface shows: the account's own, except an [`unfunded_overflow`] one
/// needs nothing.
fn inline_level(v: &AccountView) -> Level {
    if unfunded_overflow(v) {
        Level::Ok
    } else {
        v.level
    }
}

/// The runway an inline surface shows: [`fmt_runway`], or `unfunded` for an
/// [`unfunded_overflow`] account (its `no burn` is the point, its $0 is why it's listed).
fn inline_runway(v: &AccountView) -> String {
    if unfunded_overflow(v) {
        "unfunded".into()
    } else {
        fmt_runway(v)
    }
}

/// One prepaid account in a footer: `runpod $32.54 live ≈69h`, `vast ~$118.30 est.
/// ≈49d`; a low or critical one with its source spelled out, its threshold or its
/// consequence; an unknown one as `?` with the nudge and its burn; an opt-in one left
/// unfunded on purpose plainly (`shadeform $0.00 live unfunded`, [`unfunded_overflow`]).
fn footer_part(v: &AccountView, policy: &BalancePolicy, now: u64) -> String {
    let name = &v.account;
    if v.source == Source::Unknown {
        return format!("{name} ? {} {}", no_reading(v), burn_label(v));
    }
    let level = inline_level(v);
    let detailed = matches!(level, Level::Low | Level::Critical) || v.noted;
    let src = match v.source {
        Source::Live => live_label(v, now),
        _ if detailed => source_label(v, now),
        _ => "est.".into(),
    };
    let mut s = format!("{}{name} {} {src} {}", glyph(level), balance_label(v), inline_runway(v));
    match level {
        Level::Low => s.push_str(&format!(" < {}h", num(policy.warn_hours))),
        Level::Critical => s.push_str(&format!(" — {}", critical_reason(v))),
        _ => {}
    }
    if let Some(o) = v.owed {
        s.push_str(&format!(" ({} reports {} owed)", title_of(v), usd(o)));
    }
    s
}

/// `RunPod` / `Vast` for a view.
fn title_of(v: &AccountView) -> &'static str {
    Account::from_name(&v.account).map_or("the provider", Account::title)
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

/// The check's reading in words, by its worse variant: `read $0.40 low`, `read $3.10 HIGH`
/// (and which variant, when only metering reads high).
fn check_words(c: &CheckRecord) -> String {
    let d = c.worst() - c.live;
    let which = if c.predicted_metered > c.predicted + 0.005 && d > 0.0 { " (metering alone)" } else { "" };
    if d <= 0.0 {
        format!("read {} low", usd(-d))
    } else if c.verdict == Verdict::Safe {
        format!("read {} high{which} (within tolerance)", usd(d))
    } else {
        format!("read {} HIGH{which}", usd(d))
    }
}

/// A check in one phrase, with its age — a verdict from before live started failing must
/// not read as current: `fallback check ✓ over 1d, 3h ago (the estimate read $0.40 low)`.
fn check_phrase(c: &CheckRecord, failing: Option<&FailRecord>, now: u64) -> String {
    let mark = if c.verdict == Verdict::Safe { "✓" } else { "✗" };
    let before = if failing.is_some_and(|f| c.at < f.since) { ", before live started failing" } else { "" };
    format!(
        "fallback check {mark} over {}, checked {} ago{before} (the estimate {})",
        fmt_age(c.horizon_secs),
        fmt_age(now.saturating_sub(c.at)),
        check_words(c)
    )
}

/// The provider's rate over the listed burn by more than a cent — when the listing priced
/// every pod itself: a pod at a stand-in (or at nothing) isn't spend arena doesn't list,
/// it's spend arena couldn't price.
fn rate_gap(v: &AccountView) -> Option<f64> {
    match (v.provider_burn_per_hr, v.listed_per_hr) {
        (Some(p), Some(l)) if v.listed_exact && p > l + 0.01 => Some(p - l),
        _ => None,
    }
}

/// Provider spend arena doesn't list (volumes, stopped disks, serverless): a [`rate_gap`]
/// two live readings [`GAP_CONFIRM`] apart have shown — one reading isn't enough: RunPod's
/// rate still counted a terminated pod for minutes (live, `teardown --check` said "RunPod
/// reports $0.14/h more than arena lists" right after the last pod was gone).
fn unattributed(v: &AccountView) -> Option<f64> {
    rate_gap(v).filter(|_| v.gap_confirmed)
}

/// Why a row's burn can't be judged (a known balance, level unknown).
fn burn_unknown_why(v: &AccountView) -> String {
    match v.burn {
        None => "its listing didn't answer, and there is no rate or recent sample to go by".into(),
        Some(b) => {
            let mut why = Vec::new();
            if b.unpriced > 0 {
                why.push(format!("{} running pod(s) with no price and no GPU information", b.unpriced));
            }
            if b.storage > 0 {
                why.push(format!("{} stopped pod(s) whose disk size isn't listed (their storage still bills)", b.storage));
            }
            if why.is_empty() {
                why.push("the burn is only a floor".into());
            }
            why.join("; ")
        }
    }
}

/// What the live source's failure means for this row: an estimate is shown, the last
/// reading still is (under 5 min old), or nothing usable is.
fn failing_consequence(v: &AccountView, now: u64) -> String {
    match v.source {
        Source::Estimate => "estimates in use".into(),
        Source::Live => format!("showing the reading from {} ago", fmt_age(now.saturating_sub(v.as_of.unwrap_or(now)))),
        _ => format!("no usable reading — arena balance set {} <USD>", v.account),
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
            Level::Unknown if v.balance.is_none() => out.push(format!(
                "? {name}: no usable reading — read the balance off the console and run `arena balance set {name} <USD>`"
            )),
            Level::Unknown => out.push(format!("? {name}: burn unknown: {} — the runway can't be judged", burn_unknown_why(v))),
            _ => {}
        }
        if let Some(f) = v.live_failing.as_ref().filter(|_| v.noted) {
            out.push(format!("{name}: live balance failing since {} ({}) — {}", date(f.since), fail_text(f), failing_consequence(v, now)));
        }
        if v.live_reduced {
            out.push(format!(
                "{name}: RunPod rejected the full balance query (a field was dropped) — the balance is live via the \
                 minimal query; burn, spend limit and underBalance come from elsewhere"
            ));
        }
        if let Some(c) = &v.check {
            let phrase = check_phrase(c, v.live_failing.as_ref(), now);
            match c.verdict {
                Verdict::Safe => out.push(format!("{name}: {phrase}")),
                Verdict::Overstates => out.push(format!("⚠ {name}: {phrase} — don't rely on estimates")),
            }
        }
        if let Some(b) = v.burn.filter(|b| b.guessed > b.ceiling) {
            out.push(format!(
                "{name}: {} running pod(s) have no price from the listing — counted at their GPU's dearer preset price \
                 (a stand-in that errs high; stand-ins are {} of {}/h)",
                b.guessed - b.ceiling,
                usd(b.guessed_per_hr),
                burn_label(v)
            ));
        }
        if let Some(b) = v.burn.filter(|b| b.ceiling > 0) {
            out.push(format!(
                "{name}: {} running pod(s) have no price and an unknown GPU type — priced at the ceiling, \
                 {}/GPU-h (BALANCE_UNPRICED_PER_GPU; stand-ins are {} of {}/h)",
                b.ceiling,
                usd(policy.unpriced_per_gpu),
                usd(b.guessed_per_hr),
                burn_label(v)
            ));
        }
        if let Some(b) = v.burn.filter(|b| b.unpriced > 0) {
            out.push(format!(
                "{name}: {} running pod(s) have no price and no GPU to stand one in for — the burn is a floor, the runway can't be judged",
                b.unpriced
            ));
        }
        if let Some(b) = v.burn.filter(|b| b.storage > 0 && v.level != Level::Unknown) {
            out.push(format!(
                "{name}: {} stopped pod(s) still bill their disk, whose size isn't listed — not in the burn (a floor)",
                b.storage
            ));
        }
        if let Some(b) = v.burn.filter(|b| b.stopped > 0) {
            out.push(format!(
                "{name}: {} stopped pod(s) still bill their disk — counted at a stand-in storage rate ({}/h; {})",
                b.stopped,
                usd(b.stopped_per_hr),
                if name == "runpod" { "RunPod's $0.20/GB-month for an idle disk" } else { "a $0.50/GB-month ceiling" }
            ));
        }
        if let Some(o) = v.owed {
            let age = v.facts_at.map(|t| format!(", as read {} ago", fmt_age(now.saturating_sub(t)))).unwrap_or_default();
            out.push(format!(
                "{name}: {} reports {} owed (its `balance`{age}) — not taken off the credit here; compare with the console",
                title_of(v),
                usd(o)
            ));
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
            if let Some(a) = v.archive_per_hr {
                out.push(format!(
                    "{name}: includes the backup archive, {}/h (its server and volumes — not a fleet pod; `arena archive status`)",
                    money(v, a)
                ));
            }
            if let Some(e) = &v.archive_unread {
                out.push(format!("{name}: the backup archive's cost couldn't be read ({e}) — not included"));
            }
            if let Some(sp) = v.spares_per_hr {
                out.push(format!("{name}: includes the capacity spares, {}/h (nobody's machines yet; `arena pods spare list`)", money(v, sp)));
            }
            if let Some(n) = v.spares_unpriced {
                out.push(format!("{name}: {n} capacity spare(s) bill with no price listed — not included (the burn reads low)"));
            }
            if let Some(e) = &v.spares_unread {
                out.push(format!("{name}: the capacity spares couldn't be read ({e}) — not included"));
            }
            if let Some(x) = v.volumes_per_hr {
                out.push(format!("{name}: includes the VMs' volumes, {}/month (`arena pods volume list`)", money(v, x * 730.0)));
            }
            if let Some(e) = &v.volumes_unread {
                out.push(format!("{name}: the volumes couldn't be read ({e}) — not included"));
            }
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
            match v.archive_per_hr {
                Some(a) => out.push(format!(
                    "  postpaid {burn} — fleet servers {}/h + the backup archive {}/h (its server and volumes); the \
                     fleet's volumes not counted",
                    v.listed_per_hr.map_or("?".into(), |l| money(v, l)),
                    money(v, a)
                )),
                None => out.push(format!("  postpaid {burn} — servers only: volumes not counted")),
            }
            continue;
        }
        let gap = match (unattributed(v), rate_gap(v)) {
            (Some(g), _) => format!(" — gap {}/h not in the listing: volumes / stopped disks / serverless?", usd(g)),
            (None, Some(g)) => format!(
                " — gap {}/h, not yet confirmed (the provider's rate lags a create, stop or terminate: it counts once a \
                 reading {} later still shows it)",
                usd(g),
                fmt_age(GAP_CONFIRM)
            ),
            (None, None) if !v.listed_exact && v.listed_per_hr.is_some() => {
                " — the listing didn't price every pod (stand-ins / unpriced / stopped disks): no gap is inferred".into()
            }
            (None, None) => String::new(),
        };
        let listed = match v.burn.filter(|_| v.listed_per_hr.is_some()).map(|_| ()) {
            Some(()) if !v.listed_exact => format!("~{}", opt(v.listed_per_hr)),
            _ => opt(v.listed_per_hr),
        };
        let disk = match v.listed_disk_per_hr {
            d if d > 0.0 => format!(" (disks {}/h of it)", usd(d)),
            _ => String::new(),
        };
        out.push(format!("  burn: provider {} · listed {listed}{disk}{gap} · judged at {}", opt(v.provider_burn_per_hr), burn_label(v)));
        if let Some(b) = v.burn.filter(|b| b.stale) {
            let age = b.stale_secs.map_or_else(|| "?".to_string(), fmt_age);
            let how = if b.recent() {
                "taken as the burn".to_string()
            } else {
                format!("only a floor — older than {}", fmt_age(STALE_BURN_OK))
            };
            out.push(format!("  stale: its listing didn't answer — the burn last observed {age} ago stands in ({how})"));
        }
        let Some(f) = fetched.iter().find(|f| f.account.name() == v.account) else { continue };
        let st = state.get(f.account, &f.key_fp);
        // The extras as last read: this run's — or a cached reading's, said with its age.
        let cached = v.facts_at.and(st.and_then(|s| s.provider));
        let limit = v.spend_limit_per_hr.map_or("?".into(), |l| format!("{}/h", usd(l)));
        let under = v.under_balance.or(cached.and_then(|p| p.under_balance)).map_or("?".into(), |u| u.to_string());
        let age = v.facts_at.map(|t| format!(" (as read {} ago)", fmt_age(now.saturating_sub(t)))).unwrap_or_default();
        out.push(format!("  spendLimit {limit} · underBalance {under}{age}"));
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
        if let Some(c) = st.and_then(|s| s.check) {
            out.push(format!(
                "  check: predicted {} with billing / {} metered alone vs live {} — {}",
                usd(c.predicted),
                usd(c.predicted_metered),
                usd(c.live),
                check_phrase(&c, st.and_then(|s| s.live_fail.as_ref()), now)
            ));
        }
        if let Some(st) = st {
            out.push(format!("  samples: {} (listing-only {}), {} remembered pod price(s)", st.samples.len(), st.listed_samples.len(), st.prices.len()));
        }
    }
    match path {
        Some(p) => out.push(format!("state file: {}", p.display())),
        None => out.push("state file: none (no state directory) — live readings only".into()),
    }
    out
}

/// One prepaid account as the TUI words it, in pieces the top line lays out (and drops to
/// fit, `arena-tui`'s `state::top_parts`) and [`tui_parts`] joins: the level the inline
/// surfaces judge it at ([`inline_level`]: an opt-in account unfunded on purpose needs
/// nothing), the amount (`$32.54`, `~$118.30 est.` — an estimate says so —, `?`) and the
/// runway. A low or critical account always has one, beside its amount (`≈1.9d`, `no burn`,
/// `under its minimum`): it is the warning. The rest have [`inline_runway`]'s (`≈69h`,
/// `unfunded`), none without a reading.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineAccount {
    pub level: Level,
    pub amount: String,
    pub runway: Option<String>,
}

/// A critical account's runway when RunPod says it is under its minimum balance (with money
/// left): [`inline_account`]'s words for it.
pub const UNDER_MINIMUM: &str = "under its minimum";

/// [`InlineAccount`] for a prepaid account; `None` for a postpaid one (Hetzner: no balance).
pub fn inline_account(v: &AccountView) -> Option<InlineAccount> {
    if !v.prepaid() {
        return None;
    }
    let level = inline_level(v);
    let src = if v.source == Source::Estimate { " est." } else { "" };
    let runway = match level {
        // Live, `‼ shadeform balance no burn` said neither how much was left nor that it was $0:
        // the amount always, and what is wrong with it.
        Level::Critical if v.under_balance == Some(true) && v.balance.is_some_and(|b| b > 0.0) => Some(UNDER_MINIMUM.into()),
        Level::Low | Level::Critical => Some(fmt_runway(v)),
        _ if v.source == Source::Unknown => None,
        _ => Some(inline_runway(v)),
    };
    Some(InlineAccount { level, amount: format!("{}{src}", balance_label(v)), runway })
}

/// The info panel's balance (the summary bar as it used to read): `(first, last, worst)`. A
/// low or critical account goes **first**, with its amount (` ⚠ runpod $1.20 ≈1.9d ·`); the
/// rest last (`runpod $32.54 ≈69h · vast ~$118.30 est. ≈49d · shadeform $0.00 unfunded` — an
/// opt-in account unfunded on purpose is one of them, [`unfunded_overflow`]); `worst` picks
/// the tone (critical → bad, low → warn). Each account's words are [`inline_account`]'s.
pub fn tui_parts(views: &[AccountView]) -> (Option<String>, Option<String>, Level) {
    let mut first = Vec::new();
    let mut last = Vec::new();
    let mut worst = Level::Ok;
    for (v, a) in views.iter().filter_map(|v| inline_account(v).map(|a| (v, a))) {
        let runway = a.runway.map(|r| format!(" {r}")).unwrap_or_default();
        match a.level {
            Level::Low | Level::Critical => {
                first.push(format!("{}{} {}{runway}", glyph(a.level), v.account, a.amount));
                if worst != Level::Critical {
                    worst = a.level;
                }
            }
            _ => last.push(format!("{} {}{runway}", v.account, a.amount)),
        }
    }
    let first = (!first.is_empty()).then(|| format!(" {} ·", first.join(" · ")));
    let last = (!last.is_empty()).then(|| last.join(" · "));
    (first, last, worst)
}

/// The pods a `create`/`up` is about to make, for its preflight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewPods {
    pub count: usize,
    /// $/h per pod: the dearest placement option (an upper bound), or a stand-in for a GPU
    /// nobody priced; `None` when not even a stand-in could be named.
    pub per_pod: Option<f64>,
    /// `per_pod` is (or includes) a stand-in that errs high.
    pub stand_in: bool,
    /// The stand-in is the ceiling: the GPU type is unknown.
    pub ceiling: bool,
    /// `per_pod` is (or includes) a live price remembered from this long ago (seconds, under
    /// [`QUOTE_FRESH`]) — none could be read now.
    pub quoted: Option<u64>,
}

/// [`preflight_lines_with`] for an exact price (`None` = unknown).
pub fn preflight_lines(v: &AccountView, new_pods: usize, price_per_pod: Option<f64>, policy: &BalancePolicy, now: u64) -> Vec<String> {
    preflight_lines_with(v, NewPods { count: new_pods, per_pod: price_per_pod, stand_in: false, ceiling: false, quoted: None }, policy, now)
}

/// The `create`/`up` preflight for the target provider's account: the balance, the burn now
/// plus the new pods' (`n ×` the dearest option — an upper bound — or a stand-in price that
/// errs high, marked `~`) and the runway after — then RunPod's own refusals ahead of time:
/// under an hour's credit for the new pods (it needs "at least one hour's worth"), the
/// planned burn over the spend limit, `underBalance`. New pods nobody could price make the
/// runway `?`: never a runway that treats them as free. Advisory: it never blocks, and
/// `--yes` changes nothing.
pub fn preflight_lines_with(v: &AccountView, new: NewPods, policy: &BalancePolicy, now: u64) -> Vec<String> {
    let title = title_of(v);
    let head = match v.source {
        Source::Unknown => format!("balance: {} ? {}", v.account, no_reading(v)),
        _ => format!("balance: {} {} {}", v.account, balance_label(v), source_label(v, now)),
    };
    let n = new.count;
    let now_burn = v.burn.map(|b| b.per_hr);
    let added = new.per_pod.filter(|p| p.is_finite() && *p >= 0.0).map(|p| p * n as f64);
    let tilde = if new.stand_in || new.quoted.is_some() { "~" } else { "" };
    let now_part = match v.burn {
        Some(_) => format!("{}/h now", burn_label(v).trim_end_matches("/h")),
        None => "?/h now".into(),
    };
    let new_part = match added {
        Some(x) if new.ceiling => format!(
            "≤{tilde}{}/h for {n} new pod(s) (unknown GPU type, priced at the ceiling {}/GPU-h)",
            usd(x),
            usd(policy.unpriced_per_gpu)
        ),
        Some(x) if new.stand_in => format!("≤{tilde}{}/h for {n} new pod(s) (a stand-in that errs high — no live price)", usd(x)),
        Some(x) => match new.quoted {
            Some(age) => format!("≤{tilde}{}/h for {n} new pod(s) (the live price seen {} ago)", usd(x), fmt_age(age)),
            None => format!("≤{}/h for {n} new pod(s)", usd(x)),
        },
        None => format!("{n} new pod(s) (price unknown)"),
    };
    let unpriced_new = n > 0 && added.is_none();
    let burn_floor = v.burn.is_some_and(|b| b.floor());
    let after = v.balance.zip(now_burn).map(|(bal, b)| (bal, b + added.unwrap_or(0.0)));
    let (runway, flag) = match after {
        _ if unpriced_new => ("? (the new pods aren't priced — no warning can be judged)".to_string(), String::new()),
        Some((_, total)) if total < NO_BURN && burn_floor => ("?".into(), String::new()),
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
            let h = fmt_hours(h);
            let h = if burn_floor { h.replacen('≈', "≤", 1) } else { h };
            (h, flag)
        }
        None => ("?".into(), String::new()),
    };
    let mut out = vec![format!("{head} · {now_part} + {new_part} → {runway}{flag}")];
    let runpod = v.account == Account::Runpod.name();
    if let (true, Some(bal), Some(x)) = (runpod, v.balance, added) {
        if bal < x {
            out.push(format!(
                "⚠ {title} needs ≥1h of credit for the new pods (≈{tilde}{}); the account has {} — creates will likely fail (402)",
                usd(x),
                balance_label(v)
            ));
        }
    }
    if runpod && unpriced_new {
        out.push("⚠ the new pods' price is unknown — RunPod's \"≥1h of credit\" and spend-limit checks couldn't run".to_string());
    }
    if let (Some(limit), Some(b), Some(x)) = (v.spend_limit_per_hr, now_burn, added) {
        if b + x > limit {
            out.push(format!(
                "⚠ planned {tilde}{}/h exceeds {title}'s spend limit {}/h — creates past it will be refused",
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
        if let Some(o) = v.owed {
            out.push(format!("info: {} reports {} owed (its `balance`; not taken off the credit)", title_of(v), usd(o)));
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

/// The balance keys set but invalid, as `KEY (what it must be)` — exactly the keys
/// [`config_rows`] marks ✗ — for `config check`'s "missing or invalid keys" error: a ✗ row
/// fails the check, as any other invalid key does. Only `config check` fails on them: every
/// other surface uses the default (`arena balance` says so), so a typo never breaks `pods
/// list`. Pure.
pub fn invalid_keys(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if warn_hours(cfg).is_err() {
        out.push("BALANCE_WARN_HOURS (must be a number of hours ≥ 0)".to_string());
    }
    if unpriced_per_gpu(cfg).is_err() {
        out.push("BALANCE_UNPRICED_PER_GPU (must be USD per GPU-hour, above 0 and at most 1000)".to_string());
    }
    out
}

/// `config check`'s "Balance (`arena balance`)" rows, from the state file alone (no
/// request): the threshold, then per configured account how its sources stand. A ⚠ row
/// is advice, never a missing key; a ✗ row is a key set but invalid ([`invalid_keys`]).
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
    let key = "BALANCE_UNPRICED_PER_GPU";
    match unpriced_per_gpu(cfg) {
        Ok(p) => {
            let default = if cfg.get(key).is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" };
            out.push(format!("  ✓ {key:<26} {}{default} — ceiling per GPU-h for an unpriced pod of an unknown GPU type", usd(p)));
        }
        Err(e) => out.push(format!("  ✗ {key:<26} {e} — {} is used", usd(DEFAULT_UNPRICED_PER_GPU))),
    }
    for (a, fp) in accounts {
        let name = a.name();
        let Some(st) = state.get(*a, fp) else {
            out.push(format!("  · {name:<26} no reading yet — run `arena balance`"));
            continue;
        };
        // What a failing live source leaves on screen: the last reading (under FRESH), an
        // estimate (a usable anchor), or nothing usable.
        let consequence = if st.fresh_facts(now).is_some() {
            "the last reading is still shown".to_string()
        } else if st.anchor.is_some_and(|a| a.usable(now)) {
            "estimates in use".to_string()
        } else {
            format!("no usable reading (none, or older than {}) — arena balance set {name} <USD>", fmt_age(ANCHOR_MAX_AGE))
        };
        match (&st.live_fail, st.live_ok_at) {
            (Some(f), _) if noted(f, now) => out.push(format!(
                "  ⚠ {name:<26} live balance failing since {} ({}) — {consequence}",
                date(f.since),
                fail_text(f)
            )),
            (Some(f), _) => out.push(format!(
                "  · {name:<26} live balance unavailable for {} ({}) — {consequence}",
                fmt_age(now.saturating_sub(f.since)),
                f.kind.label()
            )),
            (None, Some(t)) => {
                let check = match &st.check {
                    Some(c) if c.verdict == Verdict::Safe => format!(" · {}", check_phrase(c, None, now)),
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
            out.push(format!("  ⚠ {name:<26} {} — don't rely on estimates", check_phrase(&c, st.live_fail.as_ref(), now)));
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
        for series in [&mut st.samples, &mut st.listed_samples] {
            series.retain(|(_, b)| b.is_finite() && *b >= 0.0);
            series.sort_by_key(|s| s.0);
            series.dedup_by_key(|s| s.0);
        }
        st.prices.retain(|_, (c, _)| c.is_finite() && *c >= 0.0);
    }
    Ok(s)
}

/// Whether the state file at `path` can be written now: its directory can be made, and a
/// file created in it. Inline surfaces ask before a live read — without a state that
/// keeps, nothing remembers the last attempt, and every look would read live again.
pub fn writable(path: &Path) -> bool {
    unsavable(path).is_none()
}

/// Why the state file at `path` can't be written now (`None`: it can) — [`writable`], with
/// the reason `arena balance` gives in its one warning.
pub fn unsavable(path: &Path) -> Option<String> {
    use std::io::Write as _;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if let Err(e) = crate::snapshot::create_private_dir(dir) {
        return Some(e.to_string());
    }
    // The file itself must be one a save can replace (not a directory) and write.
    let current = match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() => return Some("it isn't a regular file".into()),
        Ok(m) => {
            if let Err(e) = std::fs::OpenOptions::new().append(true).open(path) {
                return Some(e.to_string());
            }
            m.len()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Some(e.to_string()),
    };
    // Then write — and fsync — as much as a save would, and more: an empty file needs only
    // an inode, so a full disk or a spent block quota would pass while every save fails.
    let size = current.saturating_mul(2).max(64 * 1024) as usize;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let probe = dir.join(format!(".balance-probe-{}-{nanos}", std::process::id()));
    let wrote = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
        f.write_all(&vec![0u8; size])?;
        f.sync_all()
    })();
    let _ = std::fs::remove_file(&probe);
    wrote.err().map(|e| e.to_string())
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

/// Remember the live $/GPU-h a catalog or offer search showed: per GPU type the dearest ever
/// (`gpu_seen` — a known GPU's stand-in never falls below it), and per provider, GPU and
/// tier the latest with its time (`gpu_quotes` — a preflight's price for a day). Quotes older
/// than [`SAMPLE_KEEP`] are dropped.
pub fn remember_gpu_prices(path: &Path, quotes: &[GpuQuote], now: u64) -> std::io::Result<()> {
    let quotes: Vec<&GpuQuote> =
        quotes.iter().filter(|q| !q.gpu.trim().is_empty() && q.per_gpu.is_finite() && q.per_gpu > 0.0 && q.per_gpu <= 1_000.0).collect();
    if quotes.is_empty() {
        return Ok(());
    }
    record(path, |mut s| {
        s.version = STATE_VERSION;
        s.gpu_quotes.retain(|_, (_, at)| now.saturating_sub(*at) <= SAMPLE_KEEP && *at <= now.saturating_add(CLOCK_SKEW));
        for q in &quotes {
            let e = s.gpu_seen.entry(q.gpu.trim().to_string()).or_insert(q.per_gpu);
            *e = e.max(q.per_gpu);
            s.gpu_quotes.insert(quote_key(&q.provider, &q.gpu, q.cloud.as_deref()), (q.per_gpu, now));
        }
        s
    })
    .map(|_| ())
}

/// Remember the prices `pods` carry (a `create`/`up` result, say) for the configured
/// accounts they belong to, so the next listing that has no price for them — RunPod v1's
/// REST list — still does. Scoped like any reading: an entry for another key starts over.
pub fn remember_prices(path: &Path, accounts: &[(Account, String)], pods: &[Pod], now: u64) -> std::io::Result<()> {
    let priced: Vec<(Account, &str, &str, f64)> = pods
        .iter()
        .filter_map(|p| {
            let a = Account::from_name(&p.provider)?;
            let fp = accounts.iter().find(|(x, _)| *x == a)?.1.as_str();
            let c = p.cost_per_hr.filter(|c| c.is_finite() && *c > 0.0)?;
            (!p.id.is_empty()).then_some((a, fp, p.id.as_str(), c))
        })
        .collect();
    if priced.is_empty() {
        return Ok(());
    }
    record(path, |mut s| {
        s.version = STATE_VERSION;
        for (a, fp, id, c) in &priced {
            let st = s.accounts.entry(*a).or_insert_with(|| AccountState::new(fp));
            if st.key_fp != *fp {
                *st = AccountState::new(fp);
            }
            st.prices.insert(id.to_string(), (*c, now));
        }
        s
    })
    .map(|_| ())
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
    /// When this reader last tried `account` live, in this process — so a long-lived
    /// surface (the TUI) keeps to [`FRESH`] even when its state file can't be saved.
    fn last_live_attempt(&self, _account: Account) -> Option<u64> {
        None
    }
    /// Remember a live attempt (see [`BalanceApi::last_live_attempt`]).
    fn note_live_attempt(&self, _account: Account, _at: u64) {}
    /// The backup archive's cost (`arena archive`: EUR/h, its server and volumes) — postpaid
    /// Hetzner spend the fleet listing can't price (the archive is never a fleet pod). `None`:
    /// no archive token configured, not asked; `Ok(None)`: no archive. Reads only.
    async fn archive_eur_per_hr(&self) -> Option<Result<Option<f64>, String>> {
        None
    }
    /// The capacity spares' cost (`pods spare up`: EUR/h, summed over the ones billing, and how
    /// many bill with no price listed) — Hetzner spend in no fleet listing. `None`: no Hetzner
    /// key, not asked. Reads only.
    async fn spares_eur_per_hr(&self) -> Option<Result<(f64, usize), String>> {
        None
    }
    /// The fleet VMs' volumes' cost (`pods volume add`: EUR/h, this cohort's by their label)
    /// — Hetzner spend in no server listing. `None`: no Hetzner key, not asked. Reads only.
    async fn volumes_eur_per_hr(&self) -> Option<Result<f64, String>> {
        None
    }
}

/// A reader's memory of its own live attempts.
#[derive(Debug, Default)]
struct Tried(std::sync::Mutex<BTreeMap<Account, u64>>);

impl Tried {
    fn get(&self, a: Account) -> Option<u64> {
        self.0.lock().ok().and_then(|m| m.get(&a).copied())
    }
    fn note(&self, a: Account, at: u64) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(a, at);
        }
    }
}

/// The real reads: one GraphQL *query* (never a mutation) and two GETs, the keys in the
/// bearer header only, to fixed hosts — redirecting a key elsewhere is never configurable.
pub struct HttpBalanceApi {
    tried: Tried,
    client: Client,
    runpod_key: Option<String>,
    vast_key: Option<String>,
    shadeform_key: Option<String>,
    graphql_url: String,
    v2_base: String,
    vast_base: String,
    shadeform_base: String,
    /// The backup archive's own Hetzner project (its token, else the fleet's) and this
    /// cohort's names: priced for Hetzner's row only.
    archive: Option<(crate::archive::HetznerArchiveCloud, crate::archive::ArchiveNames)>,
    /// The fleet's Hetzner project, for its capacity spares (`HETZNER_API_KEY`).
    spares: Option<crate::provider::hetzner::HetznerProvider>,
    /// The fleet's Hetzner project, for this cohort's volumes (`HETZNER_API_KEY`; the prefix
    /// their label names).
    volumes: Option<(crate::archive::HetznerArchiveCloud, String)>,
}

impl HttpBalanceApi {
    pub fn from_config(cfg: &Config) -> Self {
        let key = |a: Account| cfg.get(a.key_name()).map(str::trim).filter(|k| !k.is_empty()).map(String::from);
        HttpBalanceApi {
            tried: Tried::default(),
            client: Client::new(),
            runpod_key: key(Account::Runpod),
            vast_key: key(Account::Vast),
            shadeform_key: key(Account::Shadeform),
            graphql_url: crate::provider::runpod::GRAPHQL.to_string(),
            v2_base: crate::provider::runpod_v2::BASE.to_string(),
            vast_base: crate::provider::vast::BASE.to_string(),
            shadeform_base: crate::provider::shadeform::BASE.to_string(),
            archive: crate::archive::archive_token(cfg).zip(crate::archive::ArchiveNames::from_config(cfg).ok()).map(
                |((token, _), names)| (crate::archive::HetznerArchiveCloud::new(token), names),
            ),
            spares: cfg.get("HETZNER_API_KEY").map(str::trim).filter(|k| !k.is_empty()).map(|k| {
                crate::provider::hetzner::HetznerProvider::new(k, crate::provider::hetzner::HetznerOpts::from_config(cfg))
            }),
            volumes: cfg
                .get("HETZNER_API_KEY")
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(|k| (crate::archive::HetznerArchiveCloud::new(k), cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string())),
        }
    }

    /// The same reads against loopback test servers (one per endpoint, so concurrent
    /// requests can't take each other's canned answers).
    #[cfg(test)]
    fn with_bases(graphql_url: &str, v2_base: &str, vast_base: &str, runpod_key: Option<&str>, vast_key: Option<&str>) -> Self {
        HttpBalanceApi {
            tried: Tried::default(),
            client: crate::http::test_server::client(),
            runpod_key: runpod_key.map(String::from),
            vast_key: vast_key.map(String::from),
            shadeform_key: None,
            graphql_url: graphql_url.into(),
            v2_base: v2_base.into(),
            vast_base: vast_base.into(),
            shadeform_base: String::new(),
            archive: None,
            spares: None,
            volumes: None,
        }
    }

    /// The same reads, with Shadeform's `/account` on a loopback server.
    #[cfg(test)]
    fn with_shadeform(mut self, base: &str, key: &str) -> Self {
        self.shadeform_base = base.into();
        self.shadeform_key = Some(key.into());
        self
    }

    fn key(&self, a: Account) -> Option<&str> {
        match a {
            Account::Runpod => self.runpod_key.as_deref(),
            Account::Vast => self.vast_key.as_deref(),
            Account::Shadeform => self.shadeform_key.as_deref(),
        }
    }

    async fn runpod_myself(&self, key: &str, minimal: bool) -> Result<Live, Fail> {
        let rb = crate::provider::runpod::balance_request(&self.client, &self.graphql_url, key, minimal);
        fetch(rb, &Limits::RUNPOD).await.and_then(|v| parse_runpod_myself(&v))
    }
}

/// Send (through the rate-limit-aware path, under `limits`), then [`classify`]. A transport
/// failure is transient, in our words (reqwest's own message names the URL — no key there,
/// but no reason to print it either); so is a read not sent because its quota is held.
async fn fetch(rb: reqwest::RequestBuilder, limits: &Limits) -> Result<Value, Fail> {
    match crate::http::send_text_with(rb, limits).await {
        Ok((status, text)) => classify(status.as_u16(), &text),
        Err(e) if e.kind() == Some(crate::error::ProviderErrorKind::RateLimited) => {
            Err(Fail::transient("not sent: rate-limited (the API's quota is held)"))
        }
        Err(_) => Err(Fail::transient("no answer (network error)")),
    }
}

#[async_trait]
impl BalanceApi for HttpBalanceApi {
    fn accounts(&self) -> Vec<(Account, String)> {
        Account::ALL.into_iter().filter_map(|a| self.key(a).map(|k| (a, key_fp(k)))).collect()
    }

    async fn archive_eur_per_hr(&self) -> Option<Result<Option<f64>, String>> {
        let (cloud, names) = self.archive.as_ref()?;
        Some(crate::archive::cloud::eur_per_hr(cloud, names).await.map_err(|e| e.to_string()))
    }

    async fn volumes_eur_per_hr(&self) -> Option<Result<f64, String>> {
        use crate::datavol::VolumeCloud;
        let (cloud, prefix) = self.volumes.as_ref()?;
        Some(match cloud.all_volumes().await {
            // This cohort's (by its label): a shared project's other volumes aren't its spend.
            Ok(all) => Ok(crate::datavol::per_hr(&crate::datavol::cohort_volumes(&all, prefix, &[]), crate::datavol::price_gb_month(cloud).await)),
            Err(e) => Err(e.to_string()),
        })
    }

    async fn spares_eur_per_hr(&self) -> Option<Result<(f64, usize), String>> {
        use crate::spare::SpareCloud;
        let cloud = self.spares.as_ref()?;
        Some(cloud.spares().await.map(|s| crate::spare::cost_per_hr(&s)).map_err(|e| e.to_string()))
    }

    fn last_live_attempt(&self, account: Account) -> Option<u64> {
        self.tried.get(account)
    }

    fn note_live_attempt(&self, account: Account, at: u64) {
        self.tried.note(account, at);
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
                fetch(rb, &Limits::OTHERS)
                    .await
                    .map_err(|f| match f.http {
                        Some(h) if !(200..300).contains(&h) => Fail { detail: None, ..f },
                        _ => f,
                    })
                    .and_then(|v| parse_vast_user(&v))
            }
            Account::Shadeform => {
                // Undocumented, and its body carries the account's email, phone, tax id and
                // address: read as text, typed at once into the money fields alone
                // (`shadeform::parse_account`), never quoted — an error status keeps its kind
                // and code only.
                let rb = crate::provider::shadeform::account_request(&self.client, &self.shadeform_base, key);
                match crate::http::send_text(rb).await {
                    Err(_) => Err(Fail::transient("no answer (network error)")),
                    Ok((status, _)) if !status.is_success() => Err(match classify(status.as_u16(), "") {
                        Err(f) => Fail { detail: None, ..f },
                        Ok(_) => Fail::new(FailKind::Drift, Some(status.as_u16()), "unexpected status"),
                    }),
                    Ok((_, text)) => match crate::provider::shadeform::parse_account(&text) {
                        Ok((usd, burn)) if in_range(usd) && in_range(burn) && burn >= 0.0 => Ok(Live {
                            usd,
                            provider_burn_per_hr: Some(burn),
                            spend_limit_per_hr: None,
                            under_balance: None,
                            reduced: false,
                            owed: None,
                        }),
                        Ok(_) => Err(Fail::new(FailKind::Drift, None, "GET /account: a balance or burn out of range")),
                        Err(why) => Err(Fail::new(FailKind::Drift, None, why)),
                    },
                }
            }
        }
    }

    async fn billed(&self, from_hour: u64, to: u64) -> Result<Billed, Fail> {
        let Some(key) = self.runpod_key.as_deref() else {
            return Err(Fail::new(FailKind::Auth, None, "no API key configured"));
        };
        let rb = crate::provider::runpod_v2::billing_request(&self.client, &self.v2_base, key, from_hour, to);
        fetch(rb, &Limits::RUNPOD).await.and_then(|v| parse_billing(&v))
    }
}

// ---------------------------------------------------------------------------------------
// Observing
// ---------------------------------------------------------------------------------------

/// Whether an inline surface should try live now: never tried, the last attempt (success
/// or failure — recorded in the state, or remembered by this reader) at least [`FRESH`]
/// ago, or one from the future (the clock went back).
fn live_due(st: Option<&AccountState>, remembered: Option<u64>, now: u64) -> bool {
    let recorded = st.and_then(AccountState::last_live_attempt).filter(|t| *t <= now.saturating_add(CLOCK_SKEW));
    let remembered = remembered.filter(|t| *t <= now.saturating_add(CLOCK_SKEW));
    match recorded.max(remembered) {
        None => true,
        Some(t) => now.saturating_sub(t) >= FRESH,
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
    // Its rate-limit waits too: a held quota fails the read as throttled, not as a timeout.
    match tokio::time::timeout_at(deadline, crate::ratelimit::waits_until(deadline, fut)).await {
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
    let (runpod, vast, shadeform) = tokio::join!(one(Account::Runpod), one(Account::Vast), one(Account::Shadeform));
    [runpod, vast, shadeform].into_iter().flatten().collect()
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
    let live = || {
        api.note_live_attempt(account, now);
        within(deadline, api.live(account))
    };
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
            match (live_due(st, api.last_live_attempt(account), now), from) {
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
        tried: Tried,
        panics: bool,
        /// What [`BalanceApi::archive_eur_per_hr`] answers (`None`: not configured).
        archive: Option<Result<Option<f64>, String>>,
    }

    impl FakeBalanceApi {
        /// The backup archive's cost this fake reports for Hetzner's row.
        pub fn archive(mut self, read: Result<Option<f64>, String>) -> Self {
            self.archive = Some(read);
            self
        }

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

        fn last_live_attempt(&self, account: Account) -> Option<u64> {
            self.tried.get(account)
        }

        fn note_live_attempt(&self, account: Account, at: u64) {
            self.tried.note(account, at);
        }

        async fn archive_eur_per_hr(&self) -> Option<Result<Option<f64>, String>> {
            assert!(!self.panics, "this surface must make no balance call (the archive's cost)");
            self.archive.clone()
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

    /// Capacity spares bill the Hetzner account in no fleet listing: `arena balance` adds their
    /// €/h to Hetzner's burn as its own named part; none adds nothing; a failed read is said.
    #[test]
    fn the_spares_cost_is_in_hetzners_burn_and_said() {
        let hz = Pod { id: "1".into(), name: "x-apple".into(), provider: "hetzner".into(), status: "RUNNING".into(), cost_per_hr: Some(0.01), ..Default::default() };
        let listing = Listing::from_results(vec![("hetzner".into(), Ok(vec![hz.clone()]))]);
        let mut v = postpaid_view(&listing, &[hz]).unwrap();
        add_spares(&mut v, Ok((0.031, 0)));
        assert!((v.burn.unwrap().per_hr - 0.041).abs() < 1e-9 && v.spares_per_hr == Some(0.031));
        let lines = notes(&[v.clone()], &BalancePolicy::default(), 0).join("\n");
        assert!(lines.contains("includes the capacity spares, €0.031/h"), "{lines}");
        let mut none = v.clone();
        none.spares_per_hr = None;
        add_spares(&mut none, Ok((0.0, 0)));
        assert_eq!(none.spares_per_hr, None);
        // Billing with no price listed: not a quiet zero — said, and the burn reads low.
        add_spares(&mut none, Ok((0.0, 2)));
        assert!(notes(&[none.clone()], &BalancePolicy::default(), 0).join("\n").contains("2 capacity spare(s) bill with no price listed"));
        // The VMs' volumes, likewise.
        let mut vv = none.clone();
        add_volumes(&mut vv, Ok(0.0006));
        assert!(vv.volumes_per_hr == Some(0.0006) && notes(&[vv.clone()], &BalancePolicy::default(), 0).join("\n").contains("includes the VMs' volumes"));
        add_volumes(&mut vv, Err("hetzner volumes HTTP 503".into()));
        assert!(notes(&[vv], &BalancePolicy::default(), 0).join("\n").contains("the volumes couldn't be read (hetzner volumes HTTP 503)"));
        add_spares(&mut none, Err("hetzner servers HTTP 503".into()));
        assert!(notes(&[none], &BalancePolicy::default(), 0).join("\n").contains("the capacity spares couldn't be read (hetzner servers HTTP 503)"));
    }

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
        Live { usd, provider_burn_per_hr: rate, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    fn fetched(account: Account, live: Option<Result<Live, Fail>>, billed: Option<Result<Billed, Fail>>) -> Fetched {
        Fetched { account, key_fp: FP.into(), live, billed }
    }

    fn burn(per_hr: f64) -> Option<Burn> {
        Some(Burn { per_hr, unpriced: 0, stale: false, ..Default::default() })
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
            Live { usd: 32.54, provider_burn_per_hr: Some(0.47), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None }
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
        type Case = (&'static str, Box<dyn Fn(&mut Value)>, &'static str);
        let bad: Vec<Case> = vec![
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
        type Case = (&'static str, u64, Vec<(u64, f64)>, u64, f64, f64);
        let cases: Vec<Case> = vec![
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
        let listed = Some(Burn { per_hr: 0.4, unpriced: 1, stale: false, ..Default::default() });
        // (case, listed, provider rate, samples, want)
        type Case = (&'static str, Option<Burn>, Option<f64>, Vec<(u64, f64)>, Option<Burn>);
        let cases: Vec<Case> = vec![
            ("listing + provider: the higher", listed, Some(0.6), vec![], Some(Burn { per_hr: 0.6, unpriced: 0, stale: false, ..Default::default() })),
            ("listing above provider", listed, Some(0.1), vec![], Some(Burn { per_hr: 0.4, unpriced: 0, stale: false, ..Default::default() })),
            ("listing alone keeps its unpriced floor", listed, None, vec![], listed),
            ("listed nothing: a real zero", burn(0.0), None, vec![(NOW - H, 2.0)], burn(0.0)),
            ("no listing: the provider rate", None, Some(0.3), vec![(NOW - H, 2.0)], burn(0.3)),
            ("no listing, no rate: last sample, stale", None, None, vec![(NOW - 2 * H, 1.0), (NOW - H, 2.0)], Some(Burn { per_hr: 2.0, unpriced: 0, stale: true, stale_secs: Some(H), ..Default::default() })),
            // Live: one throttled Vast listing, an idle account seen 15 min before — its $0
            // stands in (stale), as the burn; an hour-old $0 says nothing about now.
            ("no listing, no rate: a recent $0", None, None, vec![(NOW - 900, 0.0)], Some(Burn { per_hr: 0.0, stale: true, stale_secs: Some(900), ..Default::default() })),
            ("no listing, no rate: an old $0 is unknown", None, None, vec![(NOW - H, 0.0)], None),
            // Unknown is never zero.
            ("nothing at all: unknown", None, None, vec![], None),
        ];
        for (case, l, p, s, want) in cases {
            assert_eq!(burn_now(l, p, last_seen(&s, None), NOW), want, "{case}");
        }
        // A recent stale burn is the burn (not a floor); an old one is a floor.
        let recent = burn_now(None, None, Some((NOW - 900, 0.0)), NOW).unwrap();
        assert!(recent.recent() && !recent.floor(), "{recent:?}");
        let old = burn_now(None, None, Some((NOW - STALE_BURN_OK, 2.0)), NOW).unwrap();
        assert!(!old.recent() && old.floor(), "{old:?}");
        // A steady burn records no new sample: when it was last observed is what counts.
        assert_eq!(last_seen(&[(NOW - 5 * H, 0.0)], Some(NOW - 600)), Some((NOW - 600, 0.0)));
        assert_eq!(last_seen(&[(NOW - 5 * H, 0.0)], None), Some((NOW - 5 * H, 0.0)));
        assert_eq!(last_seen(&[], Some(NOW)), None);
        // From a listing: per provider; a failed provider is unknown, never $0/h.
        let l = listing(vec![
            ("runpod", Ok(vec![pod("runpod", "RUNNING", Some(0.25)), pod("runpod", "EXITED", Some(1.0)), pod("runpod", "RUNNING", None)])),
            ("vast", Err("HTTP 429")),
        ]);
        let pods = l.pods();
        let b = burns(&l, &pods);
        // The EXITED pod still bills its disk at a rate the listing doesn't carry: a floor.
        assert_eq!(b[&Account::Runpod], Some(Burn { per_hr: 0.25, unpriced: 1, storage: 1, ..Default::default() }));
        assert_eq!(b[&Account::Vast], None);
        let none = burns(&listing(vec![("runpod", Ok(vec![]))]), &[]);
        assert_eq!((none[&Account::Runpod], none[&Account::Vast]), (burn(0.0), None), "not queried is unknown too");
    }

    #[test]
    fn level_table() {
        let p = |h: f64| BalancePolicy { warn_hours: h, ..Default::default() };
        let b = |r: f64| Burn { per_hr: r, unpriced: 0, stale: false, ..Default::default() };
        let lv = |bal: Option<f64>, under: Option<bool>, burn: Option<Burn>, pol: f64| {
            let runway = bal.zip(burn).and_then(|(x, y)| runway(x, &y));
            level(bal, under, burn.as_ref(), runway, &p(pol))
        };
        // (case, balance, underBalance, burn/h, warn hours, want)
        type Case = (&'static str, Option<f64>, Option<bool>, Option<Burn>, f64, Level);
        let cases: Vec<Case> = vec![
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
            // B2-2: a known balance with no burn known at all can't be judged — never ok.
            ("burn unknown, balance known", Some(10.0), None, None, 72.0, Level::Unknown),
            // B2-6 (mutant m8): a floor that can't be judged reads unknown, short of critical —
            // a running pod nobody could price, or stopped disks of unknown size and nothing else.
            ("unpriced running pod, long runway", Some(500.0), None, Some(Burn { per_hr: 1.0, unpriced: 1, ..Default::default() }), 72.0, Level::Unknown),
            ("unpriced running pod, short runway", Some(5.0), None, Some(Burn { per_hr: 1.0, unpriced: 1, ..Default::default() }), 72.0, Level::Critical),
            ("stopped disks of unknown size only", Some(500.0), None, Some(Burn { storage: 3, ..Default::default() }), 72.0, Level::Unknown),
            ("stopped disks priced, nothing running", Some(500.0), None, Some(Burn { per_hr: 0.04, stopped: 3, stopped_per_hr: 0.04, ..Default::default() }), 72.0, Level::Ok),
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
        assert_eq!(v(Some(74.4), Some(Burn { per_hr: 1.0, unpriced: 2, stale: false, ..Default::default() })), "≤3.1d");
        assert_eq!(v(Some(0.2), Some(Burn { per_hr: 1.0, unpriced: 0, stale: true, ..Default::default() })), "<1h");
    }

    #[test]
    fn policy_from_config() {
        let p = |t: &str| BalancePolicy::from_config(&Config::parse(t));
        assert_eq!(p(""), (BalancePolicy { warn_hours: 72.0, ..Default::default() }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=\"\""), (BalancePolicy { warn_hours: 72.0, ..Default::default() }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=0"), (BalancePolicy { warn_hours: 0.0, ..Default::default() }, None));
        assert_eq!(p("BALANCE_WARN_HOURS=1.5"), (BalancePolicy { warn_hours: 1.5, ..Default::default() }, None));
        for bad in ["-1", "two days", "inf", "NaN"] {
            let (pol, warn) = p(&format!("BALANCE_WARN_HOURS=\"{bad}\""));
            assert_eq!(pol.warn_hours, 72.0, "{bad}: the default is used");
            assert!(warn.unwrap().starts_with("warning: BALANCE_WARN_HOURS must be a number of hours ≥ 0"), "{bad}");
        }
        assert!(p("BALANCE_WARN_HOURS=rpa_AAAAAAAAAAAAAAAA").1.unwrap().contains("a long value"), "a pasted key isn't echoed");
        // BALANCE_UNPRICED_PER_GPU is validated the same way: finite, above 0 (at most 1000);
        // a bad value uses the default ($8) and says so — never a failure.
        assert_eq!(p("").0.unpriced_per_gpu, 8.0);
        assert_eq!(p("BALANCE_UNPRICED_PER_GPU=12").0.unpriced_per_gpu, 12.0);
        assert_eq!(p("BALANCE_UNPRICED_PER_GPU=\"$5.5\"").0.unpriced_per_gpu, 5.5);
        for bad in ["0", "-1", "inf", "NaN", "lots", "1001"] {
            let (pol, warn) = p(&format!("BALANCE_UNPRICED_PER_GPU=\"{bad}\""));
            assert_eq!(pol.unpriced_per_gpu, 8.0, "{bad}: the default is used");
            let warn = warn.unwrap();
            assert!(warn.starts_with("warning: BALANCE_UNPRICED_PER_GPU must be USD per GPU-hour") && warn.ends_with("using 8"), "{bad}: {warn}");
        }
        // Both bad: one line, both said, both defaults used.
        let (pol, warn) = p("BALANCE_WARN_HOURS=soon\nBALANCE_UNPRICED_PER_GPU=free");
        assert_eq!((pol.warn_hours, pol.unpriced_per_gpu), (72.0, 8.0));
        let warn = warn.unwrap();
        assert!(warn.contains("BALANCE_WARN_HOURS must be") && warn.contains("BALANCE_UNPRICED_PER_GPU must be"), "{warn}");
    }

    /// The tier table: what each situation shows.
    #[test]
    fn estimate_tiers_table() {
        let b1 = || burn(1.0);
        let ok = |l: Live| fetched(Account::Runpod, Some(Ok(l)), None);
        let none = || fetched(Account::Runpod, None, None);
        let failing = || fetched(Account::Runpod, Some(Err(gone())), None);
        let with_facts = |at: u64| AccountState {
            provider: Some(ProviderFacts { usd: 40.0, burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, at, owed: None }),
            ..anchored(40.0, NOW - at, AnchorSource::Live)
        };
        let billing = |usd: f64| Billed { buckets: vec![(NOW - 2 * H, usd)], from: NOW - 3 * H, to: NOW + H };
        // (case, state, fetched, mode, want source, want balance, want method)
        type Case = (&'static str, AccountState, Fetched, Mode, Source, Option<f64>, Option<Method>);
        let cases: Vec<Case> = vec![
            ("live this run", AccountState::default(), ok(live(32.54, None)), Mode::Inline, Source::Live, Some(32.54), None),
            ("fresh cache", with_facts(NOW - 100), none(), Mode::Inline, Source::Live, Some(40.0), None),
            ("fresh cache, --no-live", with_facts(NOW - 100), none(), Mode::NoLive, Source::Estimate, Some(40.0 - 100.0 / 3_600.0), Some(Method::Metered)),
            ("cache too old", with_facts(NOW - FRESH), none(), Mode::Inline, Source::Estimate, Some(40.0 - FRESH as f64 / 3_600.0), Some(Method::Metered)),
            ("metered wins", anchored(50.0, 2 * H, AnchorSource::Live), fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing(0.5)))), Mode::Inline, Source::Estimate, Some(48.0), Some(Method::Metered)),
            // Per hour: the billed $5 for the first hour, metering's $1 for the unbilled second.
            ("billed wins", anchored(50.0, 2 * H, AnchorSource::Live), fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing(5.0)))), Mode::Inline, Source::Estimate, Some(44.0), Some(Method::Billed)),
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

    /// Scripted billing: its answer at `t`, given the true spend between two times
    /// (`spent(t0, t1)`); `None` = not fetched.
    type Billing = dyn Fn(u64, &dyn Fn(u64, u64) -> f64) -> Option<std::result::Result<Billed, Fail>>;

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
        billing: &Billing,
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

    fn scenarios() -> Vec<(&'static str, Box<Billing>)> {
        vec![
            ("complete", Box::new(|t, s| Some(Ok(billing_at(t, s, 0, 1.0))))),
            ("lagging 2h", Box::new(|t, s| Some(Ok(billing_at(t, s, 2, 1.0))))),
            ("partial in-progress", Box::new(|t, s| Some(Ok(billing_at(t, s, 0, 0.5))))),
            ("missing", Box::new(|_, _| Some(Err(Fail::new(FailKind::Auth, Some(403), "x"))))),
            ("not fetched", Box::new(|_, _| None)),
            ("flapping", Box::new(|t, s| if (t / 600) % 2 == 0 { Some(Ok(billing_at(t, s, 1, 1.0))) } else { Some(Err(Fail::transient("5xx"))) })),
        ]
    }

    /// (name, listed burn changes `(at, USD/h)`, observation times, failed-listing times).
    type History = (&'static str, Vec<(u64, f64)>, Vec<u64>, Vec<u64>);

    /// Burn histories: steps up and down between observations, idle spells, a failed
    /// listing (while the rate holds), irregular observation times.
    fn histories() -> Vec<History> {
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
        s = apply(s, std::slice::from_ref(&reduced), &b, Mode::Inline, NOW + 7 * H);
        s = apply(s, &[reduced], &b, Mode::Inline, NOW + 8 * H);
        assert_eq!(s.accounts[&Account::Runpod].live_reduced.as_ref().map(|r| (r.since, r.last)), Some((NOW + 7 * H, NOW + 8 * H)));
        s = apply(s, &[ok(28.0)], &b, Mode::Inline, NOW + 9 * H);
        assert_eq!(s.accounts[&Account::Runpod].live_reduced, None);

        // Billing: ok/fail recorded; billed_max follows the anchor.
        let billing = Billed { buckets: vec![(floor_hour(NOW + 9 * H), 0.3)], from: floor_hour(NOW + 9 * H), to: NOW + 11 * H };
        let with_billing = fetched(Account::Runpod, Some(Err(gone())), Some(Ok(billing)));
        s = apply(s, &[with_billing], &b, Mode::Inline, NOW + 10 * H);
        let st = &s.accounts[&Account::Runpod];
        assert_eq!(
            (st.billing_ok_at, st.billed_max.clone()),
            (Some(NOW + 10 * H), Some(BilledMax { anchor_at: NOW + 9 * H, buckets: vec![(floor_hour(NOW + 9 * H), 0.3)] }))
        );
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
            provider: Some(ProviderFacts { usd: 32.54, burn_per_hr: Some(0.47), spend_limit_per_hr: None, under_balance: None, at: NOW - 180, owed: None }),
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
        let line = footer_line(std::slice::from_ref(&v), &policy(), NOW).unwrap();
        assert_eq!(line, "balance: runpod ? (no reading — arena balance set runpod <USD>) ?");
        let table = super::render_table(std::slice::from_ref(&v), NOW);
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
        hz.burn = Some(Burn { per_hr: 0.006, unpriced: 0, stale: false, ..Default::default() });
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
        // Unconfirmed (one reading — the rate lags a terminate), the gap isn't said.
        assert!(!notes(std::slice::from_ref(&v), &policy(), NOW).iter().any(|l| l.contains("more than arena lists")));
        v.gap_confirmed = true; // two readings ≥ 5 min apart showed it
        v.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 32.14, predicted_metered: 32.14, live: 32.54, verdict: Verdict::Safe });
        let mut bad = lv(Account::Vast, 10.0, 0.1);
        bad.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 13.1, predicted_metered: 13.1, live: 10.0, verdict: Verdict::Overstates });
        bad.live_failing = Some(FailRecord { kind: FailKind::Drift, http: Some(400), detail: Some("Cannot query field \"credit\"".into()), since: NOW - 86_400, last: NOW });
        bad.noted = true;
        let unknown = one_view(&BalanceState::default(), &fetched(Account::Runpod, Some(Err(gone())), None), burn(0.5), Mode::Inline, NOW);
        let n = notes(&[v, bad, unknown], &policy(), NOW);
        let want = [
            "runpod: fallback check ✓ over 1d, checked 0s ago (the estimate read $0.40 low)",
            "runpod: RunPod reports $0.35/h more than arena lists (network volumes, stopped pods' disks, serverless?)",
            // The row is still the (cached) live reading: no estimate is in use.
            "vast: live balance failing since 2026-10-07 12:00 UTC (drift, HTTP 400: Cannot query field \"credit\") — showing the reading from 0s ago",
            "⚠ vast: fallback check ✗ over 1d, checked 0s ago (the estimate read $3.10 HIGH) — don't rely on estimates",
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
        // An unknown price is never treated as free: no runway, and RunPod's checks can't run.
        assert_eq!(
            preflight_lines(&rich, 1, None, &p, NOW),
            [
                "balance: runpod $500.00 live · $0.47/h now + 1 new pod(s) (price unknown) → ? (the new pods aren't priced — no warning can be judged)",
                "⚠ the new pods' price is unknown — RunPod's \"≥1h of credit\" and spend-limit checks couldn't run",
            ]
        );
        // A stand-in price (nothing priced the GPU) counts, marked.
        let ceiling = preflight_lines_with(&rich, NewPods { count: 1, per_pod: Some(8.0), stand_in: true, ceiling: true, quoted: None }, &p, NOW);
        assert_eq!(
            ceiling,
            ["balance: runpod $500.00 live · $0.47/h now + ≤~$8.00/h for 1 new pod(s) (unknown GPU type, priced at the ceiling $8.00/GPU-h) → ≈2.4d ⚠ below BALANCE_WARN_HOURS=72"]
        );
        let lines = preflight_lines_with(&rich, NewPods { count: 2, per_pod: Some(3.0), stand_in: true, ceiling: false, quoted: None }, &p, NOW);
        assert_eq!(
            lines,
            ["balance: runpod $500.00 live · $0.47/h now + ≤~$6.00/h for 2 new pod(s) (a stand-in that errs high — no live price) → ≈3.2d"]
        );
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
        assert_eq!(teardown_info(std::slice::from_ref(&rp), NOW).len(), 1, "one reading: no gap line yet");
        rp.gap_confirmed = true;
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
        // A lead always carries its amount (live: `‼ shadeform balance no burn` said neither).
        assert_eq!(tui_parts(std::slice::from_ref(&low)), (Some(" ⚠ runpod $31.20 ≈45h ·".into()), None, Level::Low));
        assert_eq!(tui_parts(&[low, crit]), (Some(" ⚠ runpod $31.20 ≈45h · ‼ vast $3.00 ≈6.0h ·".into()), None, Level::Critical));
        let mut low_est = lv(Account::Runpod, 2.5, 0.0625); // 40h
        low_est.source = Source::Estimate;
        // The same words in pieces, for the top line to lay out: postpaid has none, no reading no runway.
        let piece = |level, amount: &str, runway: Option<&str>| Some(InlineAccount { level, amount: amount.into(), runway: runway.map(Into::into) });
        assert_eq!(inline_account(&low_est), piece(Level::Low, "~$2.50 est.", Some("≈40h")));
        assert_eq!(inline_account(&ok), piece(Level::Ok, "$48.00", Some("≈4.0d")));
        let mut under = lv(Account::Runpod, 9.0, 0.01);
        under.under_balance = Some(true);
        under.level = Level::Critical;
        assert_eq!(inline_account(&under), piece(Level::Critical, "$9.00", Some("under its minimum")));
        let mut unread = AccountView::blank("vast", "USD", Source::Unknown);
        unread.level = Level::Unknown;
        assert_eq!(inline_account(&unread), piece(Level::Unknown, "?", None));
        assert_eq!(inline_account(&AccountView::blank("hetzner", "EUR", Source::Postpaid)), None);
        assert_eq!(tui_parts(&[low_est]).0.as_deref(), Some(" ⚠ runpod ~$2.50 est. ≈40h ·"));
        assert_eq!(tui_parts(&[]), (None, None, Level::Ok));
    }

    /// Live (2026-10-15): with no pods anywhere, the TUI bar led with a red `‼ shadeform balance
    /// no burn` and every `pods list` ended in `‼ shadeform $0.00 live 21s no burn — what
    /// Shadeform does at $0 is undocumented — keep Auto Top Up on` — for the opt-in overflow
    /// account, unfunded on purpose. With nothing billing there it is listed with its amount
    /// among the accounts that need nothing; with a billing pod it alarms as before. RunPod at
    /// $0 alarms whatever its burn (at $0 RunPod stops the pods — the burn drops to zero just
    /// then), and so does Vast; an opt-in account whose burn isn't known does too. `arena
    /// balance`'s table and notes, and `balance --check`, judge it as before.
    #[test]
    fn an_unfunded_opt_in_account_is_quiet_inline_until_something_bills_there() {
        let at = |account: Account, usd: f64, b: Option<Burn>| {
            let f = Fetched { account, key_fp: FP.into(), live: Some(Ok(live(usd, None))), billed: None };
            let burns: Burns = [(account, b)].into_iter().collect();
            views(&BalanceState::default(), &[f], &burns, &policy(), Mode::Inline, NOW).remove(0)
        };
        let p = policy();
        let idle = at(Account::Shadeform, 0.0, burn(0.0));
        assert!(Account::Shadeform.opt_in() && !Account::Runpod.opt_in() && !Account::Vast.opt_in());
        assert_eq!(idle.level, Level::Critical, "the account itself is judged as before");
        assert_eq!(tui_parts(std::slice::from_ref(&idle)), (None, Some("shadeform $0.00 unfunded".into()), Level::Ok));
        assert_eq!(footer_line(std::slice::from_ref(&idle), &p, NOW).unwrap(), "balance: shadeform $0.00 live unfunded");
        // Beside a healthy RunPod: in the tail, after it; nothing leads, the tone is RunPod's.
        let ok = lv(Account::Runpod, 48.0, 0.5);
        assert_eq!(tui_parts(&[ok.clone(), idle.clone()]), (None, Some("runpod $48.00 ≈4.0d · shadeform $0.00 unfunded".into()), Level::Ok));
        // `arena balance` and `balance --check` are unchanged.
        assert!(super::render_table(std::slice::from_ref(&idle), NOW).contains("‼ critical"));
        assert!(notes(std::slice::from_ref(&idle), &p, NOW).iter().any(|n| n.starts_with("‼ shadeform") && n.contains("Auto Top Up")));
        assert_eq!(check_exit(std::slice::from_ref(&idle)), 2);
        // A billing Shadeform pod: the alarm, with the amount, as before.
        let burning = at(Account::Shadeform, 0.0, burn(0.57));
        assert_eq!(tui_parts(std::slice::from_ref(&burning)), (Some(" ‼ shadeform $0.00 <1h ·".into()), None, Level::Critical));
        let line = footer_line(std::slice::from_ref(&burning), &p, NOW).unwrap();
        assert!(line.starts_with("balance: ‼ shadeform $0.00 live <1h — ") && line.contains("keep Auto Top Up on"), "{line}");
        // RunPod (and Vast) at $0 with no burn: still the alarm.
        for account in [Account::Runpod, Account::Vast] {
            let zero = at(account, 0.0, burn(0.0));
            let name = account.name();
            assert_eq!(tui_parts(std::slice::from_ref(&zero)), (Some(format!(" ‼ {name} $0.00 no burn ·")), None, Level::Critical), "{name}");
            let line = footer_line(std::slice::from_ref(&zero), &p, NOW).unwrap();
            assert!(line.starts_with(&format!("balance: ‼ {name} $0.00 live no burn — at $0 ")), "{line}");
        }
        // Shadeform at $0 whose burn isn't known (its listing failed, no rate): not known to be
        // idle — the alarm stands.
        let unknown = at(Account::Shadeform, 0.0, None);
        assert_eq!(tui_parts(std::slice::from_ref(&unknown)).2, Level::Critical);
        // Funded and idle: `no burn`, as for any account.
        assert_eq!(tui_parts(&[at(Account::Shadeform, 5.0, burn(0.0))]).1.as_deref(), Some("shadeform $5.00 no burn"));
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
            provider: Some(ProviderFacts { usd: 31.0, burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, at: NOW - 60, owed: None }),
            ..anchored(250.0, 0, AnchorSource::Manual)
        });
        let lines = set_lines(old, &s, Account::Runpod, FP, &p, NOW);
        assert_eq!(lines[1], "now: ⚠ runpod $31.00 live 1m ≈2.5d < 72h");
        assert_eq!(lines[2], "note: live is $31.00; a manual value is used only while live is unavailable");
    }

    /// A ✗ row in `config check` fails it: the keys [`config_rows`] marks ✗ are exactly the
    /// keys [`invalid_keys`] names — for every way a balance key can be wrong, and for none
    /// when they're fine. (⚠ rows — a failing source, a fallback check reading high — never
    /// count.)
    #[test]
    fn the_keys_marked_cross_are_exactly_the_invalid_keys() {
        let s = BalanceState::default();
        let cases = [
            "",
            "BALANCE_WARN_HOURS=0\nBALANCE_UNPRICED_PER_GPU=4.5",
            "BALANCE_WARN_HOURS=\nBALANCE_UNPRICED_PER_GPU=",
            "BALANCE_WARN_HOURS=soon",
            "BALANCE_WARN_HOURS=-1",
            "BALANCE_WARN_HOURS=nan",
            "BALANCE_WARN_HOURS=inf",
            "BALANCE_UNPRICED_PER_GPU=free",
            "BALANCE_UNPRICED_PER_GPU=0",
            "BALANCE_UNPRICED_PER_GPU=1001",
            "BALANCE_WARN_HOURS=-1\nBALANCE_UNPRICED_PER_GPU=-3",
        ];
        for text in cases {
            let cfg = Config::parse(text);
            let crossed: Vec<String> = config_rows(&cfg, &s, &[], NOW)
                .iter()
                .filter_map(|r| r.strip_prefix("  ✗ "))
                .map(|r| r.split_whitespace().next().unwrap_or_default().to_string())
                .collect();
            let failing: Vec<String> = invalid_keys(&cfg).iter().map(|e| e.split(" (").next().unwrap_or_default().to_string()).collect();
            assert_eq!(crossed, failing, "{text:?}");
        }
        assert!(invalid_keys(&Config::parse("")).is_empty());
        assert_eq!(invalid_keys(&Config::parse("BALANCE_WARN_HOURS=-1\nBALANCE_UNPRICED_PER_GPU=-3")).len(), 2);
    }

    #[test]
    fn config_rows_table() {
        let cfg = Config::parse("");
        let accounts = vec![(Account::Runpod, FP.to_string()), (Account::Vast, FP.to_string())];
        let mut s = state_with(AccountState {
            live_ok_at: Some(NOW - 240),
            check: Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 32.14, predicted_metered: 32.14, live: 32.54, verdict: Verdict::Safe }),
            ..anchored(32.54, 240, AnchorSource::Live)
        });
        assert_eq!(
            config_rows(&cfg, &s, &accounts, NOW),
            [
                "  ✓ BALANCE_WARN_HOURS         72 (default) — warn below 3d of runway",
                "  ✓ BALANCE_UNPRICED_PER_GPU   $8.00 (default) — ceiling per GPU-h for an unpriced pod of an unknown GPU type",
                "  ✓ runpod                     live 4m ago · fallback check ✓ over 1d, checked 0s ago (the estimate read $0.40 low)",
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
        st.check = Some(CheckRecord { at: NOW, horizon_secs: 26 * H, predicted: 13.1, predicted_metered: 13.1, live: 10.0, verdict: Verdict::Overstates });
        st.billing_fail = Some(FailRecord { kind: FailKind::Auth, http: Some(403), detail: None, since: NOW, last: NOW });
        st.live_reduced = Some(FailRecord { kind: FailKind::Drift, http: None, detail: None, since: NOW - H, last: NOW });
        let rows = config_rows(&Config::parse("BALANCE_WARN_HOURS=soon\nBALANCE_UNPRICED_PER_GPU=free"), &s, &accounts[..1], NOW);
        assert_eq!(
            rows,
            [
                "  ✗ BALANCE_WARN_HOURS         BALANCE_WARN_HOURS must be a number of hours ≥ 0 (got `soon`) — 72 is used",
                "  ✗ BALANCE_UNPRICED_PER_GPU   BALANCE_UNPRICED_PER_GPU must be USD per GPU-hour, above 0 and at most 1000 (got `free`) — $8.00 is used",
                "  ⚠ runpod                     live balance failing since 2026-12-03 00:00 UTC (drift, HTTP 400: Cannot query field \"clientBalance\" on type \"User\".) — estimates in use",
                "  ⚠ runpod                     full balance query rejected since 2026-10-08 11:00 UTC — live via the minimal query (no burn/limit/underBalance)",
                "  ⚠ runpod                     fallback check ✗ over 1d, checked 0s ago, before live started failing (the estimate read $3.10 HIGH) — don't rely on estimates",
                "  ⚠ runpod                     billing history failing (auth, HTTP 403) — estimates use arena's own metering only",
            ]
        );
        // Each ✗ row is a key `config check` fails on.
        assert_eq!(
            invalid_keys(&Config::parse("BALANCE_WARN_HOURS=soon\nBALANCE_UNPRICED_PER_GPU=free")),
            [
                "BALANCE_WARN_HOURS (must be a number of hours ≥ 0)",
                "BALANCE_UNPRICED_PER_GPU (must be USD per GPU-hour, above 0 and at most 1000)",
            ]
        );
        // BC10: past 14 days the anchor is unusable — the row says so, not "estimates in use".
        let st = s.accounts.get_mut(&Account::Runpod).unwrap();
        st.anchor = Some(Anchor { usd: 32.54, at: NOW - 15 * 86_400, source: AnchorSource::Live });
        let rows = config_rows(&Config::parse(""), &s, &accounts[..1], NOW);
        assert!(rows[2].ends_with("— no usable reading (none, or older than 14d) — arena balance set runpod <USD>"), "{rows:?}");
        assert_eq!(
            config_rows(&Config::parse("BALANCE_WARN_HOURS=0\nBALANCE_UNPRICED_PER_GPU=4.5"), &s, &[], NOW),
            [
                "  ✓ BALANCE_WARN_HOURS         0 — runway warnings off",
                "  ✓ BALANCE_UNPRICED_PER_GPU   $4.50 — ceiling per GPU-h for an unpriced pod of an unknown GPU type",
            ]
        );
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

    /// Shadeform's wallet, from its undocumented `GET /account`: the key in `X-API-KEY` (never
    /// bearer, never the URL); the balance and the account's own burn read. The endpoint gone
    /// (404), a changed shape, an outage: a failure of the right kind — and the body (the
    /// account's email, phone, tax id, address) is never in it.
    #[tokio::test]
    async fn shadeform_balance_is_read_from_its_account_endpoint() {
        let pii = r#""email":"someone@example.invalid","phone_number":"+1 555 0100","tax_id":"T-1","address":"1 Way""#;
        let srv = serve(vec![
            canned(200, "application/json", &format!(r#"{{"id":"x","tier":"free","balance":"12.50","hourly_burn":"0.57",{pii}}}"#)),
            canned(404, "text/plain", "404 page not found"),
            canned(200, "application/json", &format!("{{{pii}}}")),
            canned(503, "text/plain", "someone@example.invalid"),
            canned(401, "text/plain", "Unauthorized"),
        ]);
        let (gql, v2, vast) = (serve(vec![]), serve(vec![]), serve(vec![]));
        let api = api_at(&gql, &v2, &vast).with_shadeform(&srv.base, "sf-key-material");
        assert!(api.accounts().iter().any(|(a, _)| *a == Account::Shadeform));
        let l = api.live(Account::Shadeform).await.unwrap();
        assert_eq!((l.usd, l.provider_burn_per_hr), (12.5, Some(0.57)));
        for (kind, http) in [(FailKind::Gone, Some(404)), (FailKind::Drift, None), (FailKind::Transient, Some(503)), (FailKind::Auth, Some(401))] {
            let f = api.live(Account::Shadeform).await.unwrap_err();
            assert_eq!((f.kind, f.http), (kind, http), "{f:?}");
            assert!(!format!("{f:?}").contains("someone") && !format!("{f:?}").contains("555"), "{f:?}");
        }
        assert!(srv.requests.lock().unwrap().iter().all(|l| l == "GET /account HTTP/1.1"));
        for h in srv.headers.lock().unwrap().iter() {
            assert!(h.iter().any(|l| l == "x-api-key: sf-key-material"), "{h:?}");
            assert!(!h.iter().any(|l| l.to_ascii_lowercase().starts_with("authorization")), "{h:?}");
        }
        // Not configured: not an account, no request.
        assert!(!api_at(&gql, &v2, &vast).accounts().iter().any(|(a, _)| *a == Account::Shadeform));
        assert_eq!(Account::from_name("Shadeform"), Some(Account::Shadeform));
        assert!(Account::Shadeform.at_zero().contains("undocumented"));
    }

    /// A failed `/account` read is "balance unknown" — never $0, never a failure of what shows
    /// it (the view is still made, with no balance).
    #[test]
    fn a_failed_shadeform_balance_is_unknown_not_zero() {
        let v = views(&BalanceState::default(), &[fetched(Account::Shadeform, Some(Err(gone())), None)], &Burns::new(), &policy(), Mode::Inline, NOW)
            .remove(0);
        assert_eq!(v.account, "shadeform");
        assert_eq!(v.balance, None, "unknown, not 0");
        assert_eq!(v.source, Source::Unknown);
        let ok = views(&BalanceState::default(), &[fetched(Account::Shadeform, Some(Ok(live(8.0, Some(0.57)))), None)], &Burns::new(), &policy(), Mode::Inline, NOW)
            .remove(0);
        assert_eq!(ok.balance, Some(8.0));
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
        assert!(!req.url().as_str().contains("SECRET") && req.body().is_none());
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
            let lf = fail.then_some(FailRecord { kind: FailKind::Gone, http: Some(410), detail: None, since: NOW - 3 * H, last: NOW - H });
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
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(Account::ALL.len()));
        let writers: Vec<_> = Account::ALL
            .into_iter()
            .map(|a| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..25u64 {
                        let f = Fetched { account: a, key_fp: FP.into(), live: Some(Ok(live(100.0 + i as f64, None))), billed: None };
                        record(&path, |s| apply(s, &[f], &Burns::new(), Mode::Inline, NOW + i)).unwrap();
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

    // ---- Review fixes: the fallback check, `billed_max`, stand-in prices, writability ----

    /// One step of [`check_run`]: `(when, live USD, mode, billing)`.
    type Step = (u64, f64, Mode, Option<std::result::Result<Billed, Fail>>);

    /// RunPod lists $1/h of pods but reports $1.50/h (volumes, stopped disks); live $100 at
    /// t0. Returns the account after `steps` of `(when, live USD, mode, billing)`.
    fn check_run(steps: &[Step]) -> AccountState {
        let listed = burns_of(burn(1.0), None);
        let mut s = BalanceState::default();
        for (t, usd, mode, billed) in steps {
            let f = fetched(Account::Runpod, Some(Ok(live(*usd, Some(1.5)))), billed.clone());
            s = apply(s, &[f], &listed, *mode, *t);
        }
        s.accounts[&Account::Runpod].clone()
    }

    /// Mutant `checkrate`: the check must end its metering at the listing's burn, not at
    /// RunPod's own rate (which the fallback won't have). One segment over the whole
    /// horizon makes the end point decide it.
    #[test]
    fn check_ends_at_the_listing_burn_not_the_provider_rate() {
        let t0 = NOW - 24 * H;
        let st = check_run(&[(t0, 100.0, Mode::Full, None), (NOW, 64.0, Mode::Full, None)]);
        let c = st.check.unwrap();
        assert_eq!((c.predicted_metered, c.verdict), (76.0, Verdict::Overstates), "{c:?}");
    }

    /// Mutant `checknobill`: the check runs with this run's billing too — that variant (what
    /// RunPod's fallback shows while billing works) is recorded — and with metering alone;
    /// the worse decides. Here billing sees the $1.50/h, so only metering alone reads high.
    #[test]
    fn check_runs_with_billing_and_with_metering_alone() {
        let t0 = NOW - 24 * H;
        let buckets: Vec<(u64, f64)> = (0..24u64).map(|i| (t0 + i * H, 1.5)).collect();
        let billing = Billed { buckets, from: t0, to: NOW + H };
        let st = check_run(&[(t0, 100.0, Mode::Full, None), (NOW, 64.0, Mode::Full, Some(Ok(billing)))]);
        let c = st.check.unwrap();
        assert!((c.predicted - 64.0).abs() < 1e-9, "with billing: {c:?}");
        assert!((c.predicted_metered - 76.0).abs() < 1e-9, "metering alone: {c:?}");
        assert_eq!(c.verdict, Verdict::Overstates, "metering alone would read $12 high");
        // Notes say which variant read high.
        let mut v = lv(Account::Runpod, 64.0, 1.5);
        v.check = Some(c);
        assert!(notes(&[v], &policy(), NOW).iter().any(|n| n.contains("HIGH (metering alone)")));
    }

    /// Mutant `checkfromanchor`: an inline live read moves the anchor, not the check
    /// anchor — the check still counts down from the check anchor, a day back.
    #[test]
    fn check_counts_down_from_the_check_anchor_not_the_anchor() {
        let t0 = NOW - 24 * H;
        let st = check_run(&[(t0, 100.0, Mode::Full, None), (NOW - H, 65.5, Mode::Inline, None), (NOW, 64.0, Mode::Full, None)]);
        assert_eq!(st.anchor.map(|a| a.at), Some(NOW));
        let c = st.check.unwrap();
        assert_eq!((c.horizon_secs, c.verdict), (24 * H, Verdict::Overstates), "{c:?}");
    }

    /// Mutant `nomaxseen`: `billed_max` keeps, per hour, the most billing ever said for the
    /// anchor — a later read of a bucket that comes back lower (or a billing failure) can't
    /// make the estimate rise.
    #[test]
    fn billed_max_keeps_the_highest_reading_per_hour() {
        let a = NOW - 3 * H;
        let st = AccountState { samples: vec![(a, 0.5)], ..anchored(50.0, 3 * H, AnchorSource::Live) };
        let mut s = state_with(st);
        let read = |usd: f64| Billed { buckets: vec![(a, usd), (a + H, 0.5)], from: a, to: NOW + H };
        let b = burns_of(burn(0.5), None);
        let at = |billed| fetched(Account::Runpod, Some(Err(gone())), Some(billed));
        s = apply(s, &[at(Ok(read(3.0)))], &b, Mode::Inline, NOW - H);
        s = apply(s, &[at(Ok(read(1.0)))], &b, Mode::Inline, NOW - H + 60);
        assert_eq!(s.accounts[&Account::Runpod].billed_max.clone().unwrap().buckets, vec![(a, 3.0), (a + H, 0.5)]);
        let down = at(Err(Fail::transient("503")));
        s = apply(s, std::slice::from_ref(&down), &b, Mode::Inline, NOW);
        let v = one_view(&s, &down, burn(0.5), Mode::Inline, NOW);
        // Hour 1: billed $3 (seen earlier); hour 2: max($0.50, metered $0.50); hour 3: metered $0.50.
        assert!((v.balance.unwrap() - 46.0).abs() < 1e-9, "{v:?}");
    }

    #[test]
    fn stand_in_price_table() {
        let p = policy();
        // (gpu type, count, want price, how) — a known preset at its dearer preset price; an
        // unknown type (or a MIG slice of an unknown parent, or a count with no type) at the
        // ceiling; no GPU information at all: no price.
        // Known GPUs at the most they are known to cost: the presets and RunPod's recorded
        // catalog (H100 SXM $3.49, H200 $4.59, B200 $6.79, B300 $7.89 — the recorded price of
        // its MIG slice too); a MIG slice nobody lists at its parent's.
        type Case = (Option<&'static str>, Option<u32>, Option<(f64, StandIn)>);
        let cases: Vec<Case> = vec![
            (Some("NVIDIA RTX A4000"), Some(1), Some((0.32, StandIn::Preset))),
            (Some("RTX 3090"), Some(2), Some((1.0, StandIn::Preset))),
            (Some("2×RTX A4000"), None, Some((0.64, StandIn::Preset))),
            (Some("NVIDIA H100 80GB HBM3"), Some(1), Some((3.49, StandIn::Preset))),
            (Some("NVIDIA H100 80GB HBM3 MIG 1g.10gb"), Some(1), Some((3.49, StandIn::Preset))),
            (Some("NVIDIA H200"), Some(1), Some((4.59, StandIn::Preset))),
            (Some("NVIDIA B200"), Some(8), Some((54.32, StandIn::Preset))),
            (Some("NVIDIA B300 SXM6 AC"), Some(1), Some((7.89, StandIn::Preset))),
            (Some("NVIDIA B300 SXM6 AC MIG 1g.34gb"), Some(1), Some((0.5, StandIn::Preset))),
            (Some("NVIDIA B999 Mystery MIG 1g.10gb"), Some(1), Some((8.0, StandIn::Ceiling))),
            (Some("Mystery GPU"), None, Some((8.0, StandIn::Ceiling))),
            (None, Some(4), Some((32.0, StandIn::Ceiling))),
            (Some("-"), None, None),
            (None, None, None),
            (None, Some(0), None),
        ];
        for (ty, n, want) in cases {
            let got = stand_in(ty, n, &p);
            let same = match (got, want) {
                (Some((g, gk)), Some((w, wk))) => (g - w).abs() < 1e-9 && gk == wk,
                (g, w) => g.is_none() && w.is_none(),
            };
            assert!(same, "{ty:?} ×{n:?}: {got:?} != {want:?}");
            assert_eq!(stand_in_price(ty, n, &p), got.map(|g| g.0));
        }
        let dear = BalancePolicy { unpriced_per_gpu: 12.0, ..Default::default() };
        assert_eq!(stand_in(Some("NVIDIA B999 Mystery"), Some(2), &dear), Some((24.0, StandIn::Ceiling)));
        assert_eq!(stand_in(Some("NVIDIA RTX A4000"), Some(1), &dear), Some((0.32, StandIn::Preset)), "a known GPU ignores the ceiling");
    }

    /// The ceiling sits above the dearest pod price RunPod's recorded catalog lists (and
    /// every preset): an unknown GPU is never priced below what a known one can cost.
    #[test]
    fn the_ceiling_is_above_every_recorded_gpu_price() {
        let catalog: Value = serde_json::from_str(include_str!("provider/fixtures/runpod_v2/catalog_community_cuda13.json")).unwrap();
        let dearest = catalog["body"]["gpus"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| ["community", "secure"].map(|k| g["price"][k].as_f64().unwrap_or(0.0)))
            .fold(0.0, f64::max);
        assert!((dearest - 7.89).abs() < 1e-9, "the B300's secure price, as recorded: {dearest}");
        let presets = crate::gpu::PRESETS.iter().map(|g| g.community.max(g.secure)).fold(0.0, f64::max);
        assert!(DEFAULT_UNPRICED_PER_GPU > dearest && DEFAULT_UNPRICED_PER_GPU > presets);
    }

    /// B2-1: the compiled table is at or above what RunPod's recorded catalog charges for
    /// every GPU it lists (its dearest cloud) — and so is every known-GPU stand-in.
    #[test]
    fn recorded_table_is_at_or_above_the_fixture() {
        let catalog: Value = serde_json::from_str(include_str!("provider/fixtures/runpod_v2/catalog_community_cuda13.json")).unwrap();
        let p = policy();
        for g in catalog["body"]["gpus"].as_array().unwrap() {
            let id = g["id"].as_str().unwrap();
            let dearest = ["community", "secure"].iter().filter_map(|k| g["price"][*k].as_f64()).fold(0.0, f64::max);
            if dearest <= 0.0 {
                continue; // an unpriced catalog row ("unknown")
            }
            let row = RECORDED_PER_GPU.iter().find(|(r, _)| *r == id).unwrap_or_else(|| panic!("{id} missing from RECORDED_PER_GPU"));
            assert!(row.1 >= dearest, "{id}: table ${} < RunPod ${dearest}", row.1);
            let (s, how) = stand_in(Some(id), Some(1), &p).unwrap();
            assert!(how == StandIn::Preset && s >= dearest, "{id}: stand-in ${s} < RunPod ${dearest}");
        }
    }

    /// B2-1: a live catalog/offer price seen for a GPU raises its stand-in above the table;
    /// remembering keeps the dearest ever seen.
    #[test]
    fn live_prices_seen_raise_a_known_stand_in() {
        let dir = tmp("gpu-seen");
        let path = dir.0.join("devtest").join(crate::snapshot::BALANCE_FILE);
        let q = |gpu: &str, cloud: &str, per_gpu: f64| GpuQuote { provider: "runpod".into(), gpu: gpu.into(), cloud: Some(cloud.into()), per_gpu };
        remember_gpu_prices(&path, &[q("NVIDIA H100 80GB HBM3", "SECURE", 3.99), q("NVIDIA RTX A4000", "COMMUNITY", 0.2)], NOW - H).unwrap();
        remember_gpu_prices(&path, &[q("NVIDIA H100 80GB HBM3", "SECURE", 3.59), q("bogus", "SECURE", f64::NAN)], NOW).unwrap();
        let (s, _) = load(&path);
        assert_eq!(s.gpu_seen.get("NVIDIA H100 80GB HBM3"), Some(&3.99), "the dearest seen is kept");
        assert!(!s.gpu_seen.contains_key("bogus"));
        // Per tier, the latest is kept, with its time — what a preflight goes by for a day.
        assert_eq!(s.fresh_quote("runpod", "NVIDIA H100 80GB HBM3", Some("secure"), NOW), Some((3.59, 0)));
        assert_eq!(s.fresh_quote("runpod", "A4000", Some("COMMUNITY"), NOW), Some((0.2, H)), "a short name is the same GPU");
        assert_eq!(s.fresh_quote("runpod", "NVIDIA RTX A4000", Some("SECURE"), NOW), None, "another tier");
        assert_eq!(s.fresh_quote("runpod", "NVIDIA RTX A4000", Some("COMMUNITY"), NOW - H + QUOTE_FRESH), None, "a day old");
        // Old quotes go on the next write.
        remember_gpu_prices(&path, &[q("NVIDIA L4", "COMMUNITY", 0.39)], NOW + SAMPLE_KEEP).unwrap();
        let (later, _) = load(&path);
        assert_eq!(later.gpu_quotes.keys().collect::<Vec<_>>(), ["runpod|NVIDIA H100 80GB HBM3|SECURE", "runpod|NVIDIA L4|COMMUNITY"]);
        let p = policy();
        let (h100, how) = stand_in_with(Some("NVIDIA H100 80GB HBM3"), Some(10), &p, &s.gpu_seen).unwrap();
        assert!((h100 - 39.9).abs() < 1e-9 && how == StandIn::Preset, "{h100} {how:?}");
        // A cheaper live price never lowers a known GPU below the table or presets.
        assert_eq!(stand_in_with(Some("NVIDIA RTX A4000"), Some(1), &p, &s.gpu_seen), Some((0.32, StandIn::Preset)));
        // And the listing's stand-ins use it too.
        let l = listing(vec![("runpod", Ok(vec![Pod { id: "h".into(), provider: "runpod".into(), status: "RUNNING".into(), gpu_type: Some("NVIDIA H100 80GB HBM3".into()), gpu_count: Some(1), ..Default::default() }]))]);
        assert!((burns_with(&l, &l.pods(), &s, &p)[&Account::Runpod].unwrap().per_hr - 3.99).abs() < 1e-9);
    }

    /// B2-3: stopped pods' disks are priced at a stand-in storage rate where the listing gives
    /// their size — after a nightly `pods stop --all` a healthy account reads ok with a long
    /// runway (`--check` 0); only a disk of unknown size leaves the burn a floor.
    #[test]
    fn stopped_disks_are_priced_and_a_stopped_fleet_reads_ok() {
        let stopped = |provider: &str, disk: Option<f64>| Pod { provider: provider.into(), status: "EXITED".into(), disk_gb: disk, ..Default::default() };
        // RunPod bills a stopped pod's volume disk alone: here the whole disk is volume.
        let volume = |gb: f64| Pod { volume_gb: Some(gb), ..stopped("runpod", Some(gb)) };
        let l = listing(vec![
            ("runpod", Ok(vec![volume(120.0), volume(60.0)])),
            ("vast", Ok(vec![stopped("vast", Some(72.0))])),
        ]);
        let b = burns(&l, &l.pods());
        let rp = b[&Account::Runpod].unwrap();
        // 180 GB × $0.20/GB-month ÷ 720 h = $0.05/h.
        assert_eq!((rp.stopped, rp.storage, rp.floor()), (2, 0, false));
        assert!((rp.per_hr - 0.05).abs() < 1e-12 && (rp.stopped_per_hr - 0.05).abs() < 1e-12, "{rp:?}");
        let vast = b[&Account::Vast].unwrap();
        assert!((vast.per_hr - 0.05).abs() < 1e-12, "72 GB × $0.50 ÷ 720: {vast:?}"); // 72 × 0.5 / 720
        let f = Fetched { account: Account::Runpod, key_fp: FP.into(), live: Some(Ok(live(300.0, None))), billed: None };
        let v = views(&BalanceState::default(), &[f], &b, &policy(), Mode::Inline, NOW).remove(0);
        assert_eq!((v.level, check_exit(std::slice::from_ref(&v))), (Level::Ok, 0));
        assert_eq!((burn_label(&v).as_str(), fmt_runway(&v).as_str()), ("~$0.050/h", ">60d"));
        assert!(notes(&[v], &policy(), NOW).iter().any(|n| n.contains("2 stopped pod(s) still bill their disk — counted at a stand-in storage rate")));
        // RunPod's own rate, when read, covers the disks: the stand-in gives way.
        assert_eq!(burn_now(Some(rp), Some(0.01), None, NOW).unwrap().per_hr, 0.01);
        // A disk of unknown size: a floor — and with nothing else, unknown, said by cause.
        let l = listing(vec![("vast", Ok(vec![stopped("vast", None)]))]);
        let f = Fetched { account: Account::Vast, key_fp: FP.into(), live: Some(Ok(live(300.0, None))), billed: None };
        let v = views(&BalanceState::default(), &[f], &burns(&l, &l.pods()), &policy(), Mode::Inline, NOW).remove(0);
        assert_eq!(v.level, Level::Unknown);
        let n = notes(&[v], &policy(), NOW);
        assert!(n.iter().any(|l| l == "? vast: burn unknown: 1 stopped pod(s) whose disk size isn't listed (their storage still bills) — the runway can't be judged"), "{n:#?}");
        assert!(!n.iter().any(|l| l.contains("balance set")), "{n:#?}");
    }

    /// B2-5: a price is remembered only from a pod that bills by the hour, and kept while
    /// the pod keeps being listed — 15 days after its last listing it goes.
    #[test]
    fn prices_are_remembered_from_billing_pods_and_kept_while_listed() {
        let pod = |status: &str, cost: Option<f64>| Pod { id: "p1".into(), provider: "runpod".into(), status: status.into(), cost_per_hr: cost, gpu_count: Some(1), ..Default::default() };
        let p = policy();
        let f = fetched(Account::Runpod, Some(Err(gone())), None);
        // A stopped pod's figure (its disk's) is never remembered as its price.
        let l = listing(vec![("runpod", Ok(vec![pod("EXITED", Some(0.006))]))]);
        let s = apply(BalanceState::default(), std::slice::from_ref(&f), &burns_with(&l, &l.pods(), &BalanceState::default(), &p), Mode::Inline, NOW - 21 * 86_400);
        assert!(s.accounts[&Account::Runpod].prices.is_empty());
        // A billing pod's is; the pod listed (unpriced) daily for 20 days keeps it…
        let l = listing(vec![("runpod", Ok(vec![pod("RUNNING", Some(0.17))]))]);
        let mut s = apply(s, std::slice::from_ref(&f), &burns_with(&l, &l.pods(), &BalanceState::default(), &p), Mode::Inline, NOW - 20 * 86_400);
        let bare = listing(vec![("runpod", Ok(vec![pod("RUNNING", None)]))]);
        for d in 1..=20u64 {
            let b = burns_with(&bare, &bare.pods(), &s, &p);
            s = apply(s, std::slice::from_ref(&f), &b, Mode::Inline, NOW - 20 * 86_400 + d * 86_400);
        }
        assert_eq!(s.accounts[&Account::Runpod].prices.get("p1").map(|x| x.0), Some(0.17));
        // …and 16 days after it was last listed at all, it's gone.
        let none = listing(vec![("runpod", Ok(vec![]))]);
        let s = apply(s, std::slice::from_ref(&f), &burns(&none, &[]), Mode::Inline, NOW + 16 * 86_400);
        assert!(s.accounts[&Account::Runpod].prices.is_empty());
    }

    /// Every running pod gets a price that errs high: the listing's, the last one seen for
    /// that pod, a stand-in; only a pod with no GPU at all stays unpriced. Hetzner (a CPU VM,
    /// EUR) never gets a GPU stand-in.
    #[test]
    fn burns_price_running_pods_by_memory_or_stand_in() {
        let gpu = |id: &str, cost: Option<f64>, n: Option<u32>| Pod {
            id: id.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            cost_per_hr: cost,
            gpu_count: n,
            ..Default::default()
        };
        // A priced look (enriched): p1 at $0.50 is remembered.
        let l1 = listing(vec![("runpod", Ok(vec![gpu("p1", Some(0.5), Some(1))]))]);
        let b1 = burns_with(&l1, &l1.pods(), &BalanceState::default(), &policy());
        assert_eq!(b1.seen[&Account::Runpod], vec![("p1".to_string(), 0.5)]);
        let s = apply(BalanceState::default(), &[fetched(Account::Runpod, Some(Ok(live(50.0, None))), None)], &b1, Mode::Inline, NOW - H);
        assert_eq!(s.accounts[&Account::Runpod].prices["p1"], (0.5, NOW - H));
        // An unpriced look (v1 REST, no details): p1 from memory, p2 (two GPUs of no known
        // type) at the ceiling, p3 unpriced.
        let l2 = listing(vec![("runpod", Ok(vec![gpu("p1", None, Some(1)), gpu("p2", None, Some(2)), gpu("p3", None, None)]))]);
        let b2 = burns_with(&l2, &l2.pods(), &s, &policy());
        let rp = b2[&Account::Runpod].unwrap();
        assert_eq!((rp.per_hr, rp.guessed, rp.guessed_per_hr, rp.ceiling, rp.unpriced), (0.5 + 16.0, 1, 16.0, 1, 1));
        // With RunPod's own rate known, the guesses give way to it (it covers the account).
        assert_eq!(burn_now(Some(rp), Some(0.9), None, NOW).unwrap().per_hr, 0.9);
        assert_eq!(burn_now(Some(rp), Some(0.1), None, NOW).unwrap().per_hr, 0.5, "the remembered price still counts");
        // The guess is recorded as a sample (it errs high) and flags the estimate.
        let f = fetched(Account::Runpod, Some(Err(gone())), None);
        let s = apply(s, std::slice::from_ref(&f), &b2, Mode::Inline, NOW);
        assert!(s.accounts[&Account::Runpod].samples.iter().any(|(t, b)| *t == NOW && *b >= 16.5));
        let v = views(&s, std::slice::from_ref(&f), &b2, &policy(), Mode::Inline, NOW + 600).remove(0);
        assert!(source_label(&v, NOW + 600).contains("stand-in prices"), "{v:?}");
        assert_eq!(burn_label(&v), "≥$16.50/h", "unpriced p3 makes it a floor");
        assert!(
            notes(&[v], &policy(), NOW + 600).iter().any(|n| n.contains("1 running pod(s) have no price and an unknown GPU type — priced at the ceiling, $8.00/GPU-h")),
            "the ceiling is named"
        );
        // Hetzner: no GPU stand-in for a server type.
        let l3 = listing(vec![("hetzner", Ok(vec![Pod { provider: "hetzner".into(), status: "running".into(), gpu_type: Some("cx23".into()), ..Default::default() }]))]);
        assert_eq!(postpaid_view(&l3, &l3.pods()).unwrap().burn.unwrap().unpriced, 1);
    }

    #[test]
    fn a_guessed_burn_is_judged_and_shown_with_a_tilde() {
        // A known preset GPU's stand-in (no ceiling).
        let guessed = Burn { per_hr: 3.0, guessed: 1, guessed_per_hr: 3.0, ..Default::default() };
        let mut v = lv(Account::Runpod, 30.0, 3.0);
        v.burn = Some(guessed);
        v.runway_hours = runway(30.0, &guessed);
        v.level = level(Some(30.0), None, Some(&guessed), v.runway_hours, &policy());
        assert_eq!((burn_label(&v).as_str(), fmt_runway(&v).as_str(), v.level), ("~$3.00/h", "≈10h", Level::Critical));
        let n = notes(&[v], &policy(), NOW);
        assert!(n.iter().any(|n| n.contains("1 running pod(s) have no price from the listing — counted at their GPU's dearer preset price")), "{n:?}");
        assert!(!n.iter().any(|n| n.contains("ceiling")), "{n:?}");
    }

    #[test]
    fn writable_probes_the_state_dir() {
        let dir = tmp("writable");
        assert!(writable(&dir.0.join("devtest").join("balance.json")));
        assert!(dir.0.join("devtest").is_dir(), "the state dir is made");
        let names: Vec<_> = std::fs::read_dir(dir.0.join("devtest")).unwrap().collect();
        assert!(names.is_empty(), "the probe file is removed");
        std::fs::write(dir.0.join("file"), "").unwrap();
        assert!(!writable(&dir.0.join("file").join("balance.json")), "a file where the dir should be");
    }

    // ---- Adversarial review (balance safety): regression tests, expected to FAIL on d2cbe31 ----

    /// A RunPod v1 fleet as REST lists it — no `$/h` (that comes from the GraphQL details
    /// query, down whenever the GraphQL balance is) — `n` RUNNING pods, unpriced. `arena
    /// balance` and `teardown --check` never enrich, so on v1 this is what they always see.
    fn review_unpriced_runpod(n: usize) -> Listing {
        listing(vec![("runpod", Ok((0..n).map(|_| pod("runpod", "RUNNING", None)).collect()))])
    }

    /// BS1a: live failing + an unpriced floor burn (`≥$0.00/h`, 10 pods unpriced) reads as
    /// "no burn" and level ok — though 10 pods are running and 4 h of money is left.
    #[test]
    fn review_bs1a_unpriced_floor_is_never_no_burn() {
        let t0 = NOW - H;
        let lst = review_unpriced_runpod(10);
        let b = burns(&lst, &lst.pods());
        assert_eq!(b[&Account::Runpod], Some(Burn { per_hr: 0.0, unpriced: 10, stale: false, ..Default::default() }), "fixture: a $0 floor");
        // `arena balance` an hour ago: live $15, RunPod's own rate $3/h (10 pods × $0.30).
        let ok = fetched(Account::Runpod, Some(Ok(live(15.0, Some(3.0)))), None);
        let s = apply(BalanceState::default(), &[ok], &b, Mode::Full, t0);
        // GraphQL down now (live and billing transient).
        let down = fetched(Account::Runpod, Some(Err(Fail::transient("503"))), Some(Err(Fail::transient("503"))));
        let s = apply(s, std::slice::from_ref(&down), &b, Mode::Full, NOW);
        let v = views(&s, std::slice::from_ref(&down), &b, &policy(), Mode::Full, NOW).remove(0);
        assert_eq!(v.source, Source::Estimate);
        // Truth: $12 left at $3/h = 4 h → critical (`balance --check` exit 2).
        assert_ne!(fmt_runway(&v), "no burn", "10 running pods, unpriced, is not 'no burn': {v:?}");
        assert_ne!(v.level, Level::Ok, "4 h of runway reads ok (check_exit {}): {v:?}", check_exit(std::slice::from_ref(&v)));
    }

    /// BS1b: the same floor is recorded as a $0 *sample*, so metering then charges the time
    /// after it at $0/h and, with billing unavailable (403 scope / same outage), the estimate
    /// overstates the money left.
    #[test]
    fn review_bs1b_unpriced_floor_is_not_recorded_as_zero_burn() {
        let t0 = NOW - 2 * H;
        let lst = review_unpriced_runpod(10);
        let b = burns(&lst, &lst.pods());
        let ok = fetched(Account::Runpod, Some(Ok(live(15.0, Some(3.0)))), None);
        let s = apply(BalanceState::default(), &[ok], &b, Mode::Full, t0);
        let down = fetched(Account::Runpod, Some(Err(Fail::transient("503"))), Some(Err(Fail::transient("503"))));
        let s = apply(s, std::slice::from_ref(&down), &b, Mode::Full, t0 + H);
        let s = apply(s, std::slice::from_ref(&down), &b, Mode::Full, NOW);
        let v = views(&s, std::slice::from_ref(&down), &b, &policy(), Mode::Full, NOW).remove(0);
        // Truth at NOW: 15 − 2 h × $3 = $9.
        assert!(
            v.balance.unwrap() <= 9.0 + 1e-9,
            "estimate ${:.2} > truth $9.00 (errs HIGH); samples {:?}",
            v.balance.unwrap(),
            s.accounts[&Account::Runpod].samples
        );
    }

    /// BS3: Vast bills storage for stopped instances, but `bills_hourly` leaves them out and
    /// Vast has neither a provider rate nor a billing history — 20 stopped instances read
    /// "no burn" / ok, and an estimate would never count that storage down.
    #[test]
    fn review_bs3_vast_stopped_instances_are_not_no_burn() {
        let lst = listing(vec![("vast", Ok((0..20).map(|_| pod("vast", "exited", Some(0.35))).collect()))]);
        let b = burns(&lst, &lst.pods());
        let f = Fetched { account: Account::Vast, key_fp: FP.into(), live: Some(Ok(live(5.0, None))), billed: None };
        let v = views(&BalanceState::default(), &[f], &b, &policy(), Mode::Inline, NOW).remove(0);
        assert_ne!(fmt_runway(&v), "no burn", "20 stopped Vast instances still pay storage: {v:?}");
    }

    /// BS4: the SAMPLE_MAX cap drops the oldest samples — the baseline an older anchor needs
    /// with them — so pruning *raises* the estimate (invariant 6 broken; Vast has no billing
    /// to cover it).
    #[test]
    fn review_bs4_sample_cap_never_raises_the_estimate() {
        let anchor = Anchor { usd: 500.0, at: NOW - 10 * 86_400, source: AnchorSource::Live };
        // Five days at $3/h, then a low burn flickering between two views (e.g. the TUI's
        // priced listing vs a surface that sees unpriced pods) — past SAMPLE_MAX change points.
        let mut samples = vec![(anchor.at, 3.0)];
        let low_from = NOW - 5 * 86_400;
        let n = SAMPLE_MAX + 10;
        let step = (5 * 86_400) / n as u64;
        for i in 0..n {
            samples.push((low_from + i as u64 * step, if i % 2 == 0 { 0.10 } else { 0.12 }));
        }
        let before = estimate(&anchor, &samples, None, 0.1, NOW).usd;
        let mut pruned = samples.clone();
        prune(&mut pruned, NOW);
        assert_eq!(pruned.len(), SAMPLE_MAX);
        let after = estimate(&anchor, &pruned, None, 0.1, NOW).usd;
        assert!(after <= before + 1e-9, "pruning raised the estimate: ${before:.2} → ${after:.2}");
    }

    /// BS5: a sample from the future (a clock that ran ahead, since corrected) blocks every
    /// later sample until real time passes it — a burn seen meanwhile is never recorded, and
    /// the estimate overstates.
    #[test]
    fn review_bs5_future_sample_does_not_freeze_metering() {
        let mut st = anchored(50.0, 2 * H, AnchorSource::Live);
        st.samples = vec![(NOW - 2 * H, 0.0), (NOW + 86_400, 0.0)];
        let s = state_with(st);
        let down = fetched(Account::Runpod, Some(Err(Fail::transient("503"))), None);
        // An hour ago the listing showed pods at $5/h …
        let s = apply(s, std::slice::from_ref(&down), &burns_of(burn(5.0), None), Mode::Inline, NOW - H);
        // … now they're gone.
        let s = apply(s, std::slice::from_ref(&down), &burns_of(burn(0.0), None), Mode::Inline, NOW);
        let v = views(&s, std::slice::from_ref(&down), &burns_of(burn(0.0), None), &policy(), Mode::Inline, NOW).remove(0);
        assert!(
            v.balance.unwrap() <= 45.0 + 1e-9,
            "estimate ${:.2} > truth ≤ $45 — the $5/h hour was never recorded: {:?}",
            v.balance.unwrap(),
            s.accounts[&Account::Runpod].samples
        );
    }

    /// BS6: with a state file that can't be written (disk full, a root-owned or stray-file
    /// state dir, no HOME), FRESH no longer rate-limits: every inline look (each TUI details
    /// round, each `pods list` under `watch`) makes a live call.
    #[tokio::test]
    async fn review_bs6_unwritable_state_still_bounds_live_calls() {
        let dir = tmp("review-unwritable");
        let blocker = dir.0.join("devtest");
        std::fs::write(&blocker, "").unwrap(); // the state "directory" is a file
        let path = blocker.join(crate::snapshot::BALANCE_FILE);
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(40.0, Some(0.5)));
        for i in 0..3 {
            let now = NOW + i * 60; // the TUI's details round, once a minute
            let (state, _) = load(&path);
            let fetched = observe(&api, &state, Mode::Inline, INLINE_TIMEOUT, now).await;
            let _ = commit(Some(&path), state, &fetched, &burns_of(burn(0.5), None), Mode::Inline, now);
        }
        assert!(api.live_calls() <= 1, "{} live calls in 3 minutes (FRESH = 5 min)", api.live_calls());
    }

    /// BS7 (semantics unverified): Vast's `/users/current/` also has `balance`, negative when
    /// charges are owed; the previous implementation surfaced it ("possibly owed"), this one
    /// reads `credit` alone — if `balance` is pending charges, the live figure is high.
    #[test]
    fn review_bs7_vast_owed_balance_is_not_ignored() {
        // ADAPTED (fix direction 9): no arithmetic on `balance` until the live console
        // comparison settles what it means — the credit stays the balance, and the owed
        // figure is carried and said ("Vast reports $8.00 owed") wherever the balance is.
        let l = parse_vast_user(&json!({"credit": 10.0, "balance": -8.0})).unwrap();
        assert_eq!((l.usd, l.owed), (10.0, Some(8.0)));
        let f = Fetched { account: Account::Vast, key_fp: FP.into(), live: Some(Ok(l)), billed: None };
        let v = views(&BalanceState::default(), &[f], &burns_of(None, burn(0.5)), &policy(), Mode::Inline, NOW).remove(0);
        assert!(footer_line(std::slice::from_ref(&v), &policy(), NOW).unwrap().contains("(Vast reports $8.00 owed)"));
        assert!(notes(std::slice::from_ref(&v), &policy(), NOW).iter().any(|n| n.contains("Vast reports $8.00 owed")));
    }

    /// BS9: the single-spec `create`/`up` path (one GPU, no `--max-price` — the common one)
    /// can't price the new pods, and the preflight then judges the runway as if they cost
    /// nothing: `→ no burn` (or the pre-create runway, unflagged) for pods about to bill.
    #[test]
    fn review_bs9_preflight_with_unpriced_new_pods_never_says_no_burn() {
        let f = fetched(Account::Runpod, Some(Ok(live(1.0, Some(0.0)))), None);
        let v = views(&BalanceState::default(), &[f], &burns_of(burn(0.0), None), &policy(), Mode::Inline, NOW).remove(0);
        let lines = preflight_lines(&v, 10, None, &policy(), NOW);
        assert!(!lines[0].ends_with("no burn"), "10 new pods of unknown price, $1 left: {lines:?}");
    }

    /// BS10: the fallback check integrates samples recorded as max(listed, RunPod's own
    /// rate) — but after GraphQL goes the fallback only has the listing. Spend arena doesn't
    /// list (stopped pods' disks, volumes) is in every sample the check uses and in none the
    /// fallback will, so with billing unavailable (403) the check says "safe" for a fallback
    /// that will overstate by that spend. False assurance from the early-warning mechanism.
    #[test]
    fn review_bs10_fallback_check_judges_the_metering_the_fallback_will_have() {
        // $1/h of listed pods; RunPod reports $1.50/h (+$0.50/h of disks/volumes arena can't list).
        let listed = burns_of(burn(1.0), None);
        let t0 = NOW - 24 * H;
        let mut s = BalanceState::default();
        for i in 0..24u64 {
            // The hourly inline looks (footer, TUI) while GraphQL works.
            let f = fetched(Account::Runpod, Some(Ok(live(100.0 - 1.5 * i as f64, Some(1.5)))), None);
            s = apply(s, &[f], &listed, Mode::Inline, t0 + i * H);
        }
        // `arena balance` 24 h on: live $64; billing refused (key scope).
        let f = fetched(Account::Runpod, Some(Ok(live(64.0, Some(1.5)))), Some(Err(Fail::new(FailKind::Auth, Some(403), "billing"))));
        let s = apply(s, &[f], &listed, Mode::Full, NOW);
        let c = s.accounts[&Account::Runpod].check.expect("a check ran");
        // What the fallback would have said from the listing alone: 100 − 24 × $1 = $76 vs $64.
        assert_eq!(c.verdict, Verdict::Overstates, "check {c:?}: predicted from provider-rate samples, not the listing");
    }

    // ---- live follow-ups (2026-10-13) ----

    /// Follow-up 1: never a signed zero (live: `$-0.00 in all`, `read $-0.00 low` — an empty
    /// sum, a zero difference negated); under half a cent is zero; the sign stays outside.
    #[test]
    fn money_is_never_a_signed_zero() {
        let cases = [
            (0.0, "$0.00"),
            (-0.0, "$0.00"),
            (0.004, "$0.00"),
            (-0.004, "$0.00"),
            (0.005, "$0.005"),
            (-0.005, "-$0.005"),
            (0.017, "$0.017"),
            (-2.0, "-$2.00"),
            (30.7709, "$30.77"),
            ([0.0f64; 0].iter().sum(), "$0.00"),
        ];
        for (x, want) in cases {
            assert_eq!(usd(x), want, "{x:?}");
        }
        let mut hz = AccountView::blank("hetzner", "EUR", Source::Postpaid);
        hz.burn = Some(Burn { per_hr: -0.0, ..Default::default() });
        assert_eq!(burn_label(&hz), "€0.00/h");
        // Where it showed live: no billing buckets, and a check that read exactly right.
        let st = state_with(AccountState {
            check: Some(CheckRecord { at: NOW, horizon_secs: 1_980, predicted: 30.77, predicted_metered: 30.77, live: 30.77, verdict: Verdict::Safe }),
            ..anchored(30.77, 60, AnchorSource::Live)
        });
        let empty = Billed { buckets: Vec::new(), from: floor_hour(NOW - 60), to: floor_hour(NOW) + H };
        let f = fetched(Account::Runpod, Some(Ok(live(30.77, Some(0.0)))), Some(Ok(empty)));
        let v = one_view(&st, &f, burn(0.0), Mode::Full, NOW);
        let text = [verbose_lines(std::slice::from_ref(&v), &st, std::slice::from_ref(&f), None, NOW), notes(&[v], &policy(), NOW)].concat().join("\n");
        assert!(text.contains("billing: 0 hourly bucket(s), $0.00 in all, $0.00 since the anchor's hour"), "{text}");
        assert!(text.contains("(the estimate read $0.00 low)"), "{text}");
        assert!(!text.contains("$-"), "{text}");
    }

    /// A RunPod pod with these disks.
    fn disks(status: &str, cost: Option<f64>, disk: Option<f64>, volume: Option<f64>) -> Pod {
        Pod { id: format!("p-{status}-{disk:?}-{volume:?}"), disk_gb: disk, volume_gb: volume, ..pod("runpod", status, cost) }
    }

    /// Follow-up 3: a running RunPod pod bills its disks — container and volume, $0.10/GB-month
    /// — on top of the listed $/h (live: a listed $0.13/h 3070 with a 60 GB container disk
    /// drained $0.138/h). Counted in the listed burn, it is no stand-in and no gap; a Vast
    /// instance's price already includes its disk.
    #[test]
    fn running_runpod_disks_are_priced_on_top_of_the_listed_price() {
        let disk = 60.0 * RUNPOD_RUNNING_DISK_PER_GB_MONTH / 720.0;
        let l = listing(vec![
            ("runpod", Ok(vec![disks("RUNNING", Some(0.13), Some(60.0), Some(0.0))])),
            ("vast", Ok(vec![Pod { disk_gb: Some(60.0), ..pod("vast", "RUNNING", Some(0.1)) }])),
        ]);
        let b = burns(&l, &l.pods());
        let rp = b[&Account::Runpod].unwrap();
        assert!((rp.per_hr - (0.13 + disk)).abs() < 1e-12 && (rp.disk_per_hr - disk).abs() < 1e-12, "{rp:?}");
        assert!(rp.exact() && (rp.per_hr - 0.138).abs() < 0.001, "{rp:?}");
        assert_eq!(b[&Account::Vast].unwrap().per_hr, 0.1, "Vast's dph_total has the disk in it");
        // RunPod's own rate ($0.138) and the listing now agree: no gap, and -v says where it went.
        let f = fetched(Account::Runpod, Some(Ok(live(30.77, Some(0.138)))), None);
        let v = views(&BalanceState::default(), std::slice::from_ref(&f), &b, &policy(), Mode::Inline, NOW).remove(0);
        assert_eq!(unattributed(&v), None);
        let lines = verbose_lines(std::slice::from_ref(&v), &BalanceState::default(), &[f], None, NOW);
        assert!(lines.iter().any(|l| l == "  burn: provider $0.14/h · listed $0.14/h (disks $0.008/h of it) · judged at $0.14/h"), "{lines:#?}");
        // Live, the fallback check over 63 min read $0.003 HIGH by the listing alone; with the
        // disk counted it errs low.
        let t0 = NOW - 63 * 60;
        let s = apply(BalanceState::default(), &[fetched(Account::Runpod, Some(Ok(live(30.77094, Some(0.138)))), None)], &b, Mode::Full, t0);
        let s = apply(s, &[fetched(Account::Runpod, Some(Ok(live(30.63117, Some(0.138)))), None)], &b, Mode::Full, NOW);
        let c = s.accounts[&Account::Runpod].check.unwrap();
        assert!(c.worst() <= c.live, "the fallback reads high: {c:?}");
    }

    /// Follow-up 3: a stopped RunPod pod bills its volume disk alone ($0.20/GB-month) — live,
    /// one with a 60 GB container disk and no volume drained $0.00/h while the stand-in said
    /// $0.017/h. A volume of unknown size leaves a floor.
    #[test]
    fn a_stopped_runpod_pod_bills_its_volume_alone() {
        let l = listing(vec![("runpod", Ok(vec![disks("EXITED", None, Some(60.0), Some(0.0))]))]);
        let rp = burns(&l, &l.pods())[&Account::Runpod].unwrap();
        assert_eq!((rp.per_hr, rp.stopped, rp.storage, rp.floor()), (0.0, 0, 0, false), "no volume: nothing bills");
        let f = fetched(Account::Runpod, Some(Ok(live(30.63, Some(0.0)))), None);
        let v = views(&BalanceState::default(), std::slice::from_ref(&f), &burns(&l, &l.pods()), &policy(), Mode::Inline, NOW).remove(0);
        assert_eq!((burn_label(&v).as_str(), fmt_runway(&v).as_str(), v.level), ("$0.00/h", "no burn", Level::Ok));
        let l = listing(vec![("runpod", Ok(vec![disks("EXITED", None, Some(80.0), Some(20.0))]))]);
        let rp = burns(&l, &l.pods())[&Account::Runpod].unwrap();
        assert!((rp.per_hr - 20.0 * 0.20 / 720.0).abs() < 1e-12 && rp.stopped == 1, "the 20 GB volume only: {rp:?}");
        let l = listing(vec![("runpod", Ok(vec![disks("EXITED", None, Some(80.0), None)]))]);
        let rp = burns(&l, &l.pods())[&Account::Runpod].unwrap();
        assert_eq!((rp.storage, rp.floor()), (1, true), "a volume of unknown size: a floor");
    }

    /// Follow-up 4: one throttled Vast listing turned an idle account (its $0 seen 15 min
    /// before) `unknown`. A burn observed under 30 min ago stands in, marked stale — the
    /// estimate and the level stand; `-v` says so. Older, a $0 is unknown again (and a burn
    /// above zero only a floor).
    #[test]
    fn a_recent_burn_stands_in_for_a_failed_listing() {
        let vast = |live_: Option<Result<Live, Fail>>| fetched(Account::Vast, live_, None);
        let idle = burns_of(None, burn(0.0));
        let t0 = NOW - 15 * 60;
        let s = apply(BalanceState::default(), &[vast(Some(Ok(live(136.84, None))))], &idle, Mode::Full, t0);
        // A steady $0 records no new sample — yet the burn was observed at each look.
        let s = apply(s, &[vast(Some(Ok(live(136.84, None))))], &idle, Mode::Full, t0 + 60);
        assert_eq!(s.accounts[&Account::Vast].burn_seen_at, Some(t0 + 60));
        let failed = burns_of(None, None);
        let at = |now: u64| views(&s, &[vast(None)], &failed, &policy(), Mode::NoLive, now).remove(0);
        let v = at(NOW);
        assert_eq!((v.source, v.level, check_exit(std::slice::from_ref(&v))), (Source::Estimate, Level::Ok, 0), "{v:?}");
        let b = v.burn.unwrap();
        assert!(b.stale && b.recent() && !b.floor() && b.stale_secs == Some(14 * 60), "{b:?}");
        let lines = verbose_lines(std::slice::from_ref(&v), &s, &[vast(None)], None, NOW);
        assert!(lines.iter().any(|l| l == "  stale: its listing didn't answer — the burn last observed 14m ago stands in (taken as the burn)"), "{lines:#?}");
        // Half an hour on, still failing: unknown, as before.
        let v = at(t0 + 60 + STALE_BURN_OK);
        assert_eq!((v.source, v.level, v.burn), (Source::Unknown, Level::Unknown, None));
        // A burn above zero, old: a floor, said so.
        let busy = burns_of(None, burn(0.5));
        let s = apply(BalanceState::default(), &[vast(Some(Ok(live(136.84, None))))], &busy, Mode::Full, NOW - 2 * H);
        let v = views(&s, &[vast(None)], &failed, &policy(), Mode::NoLive, NOW).remove(0);
        assert_eq!((burn_label(&v).as_str(), v.burn.unwrap().floor()), ("≥$0.50/h", true));
        let lines = verbose_lines(std::slice::from_ref(&v), &s, &[vast(None)], None, NOW);
        assert!(lines.iter().any(|l| l.ends_with("the burn last observed 2h ago stands in (only a floor — older than 30m)")), "{lines:#?}");
    }

    /// Follow-up 5: RunPod's rate kept counting a terminated pod for 1–4 minutes, so
    /// `teardown --check` said "RunPod reports $0.14/h more than arena lists" right after the
    /// last pod was gone. The gap counts once two live readings 5 min apart both show it; a
    /// reading without it starts over.
    #[test]
    fn a_rate_gap_counts_once_two_readings_five_minutes_apart_show_it() {
        let empty = burns_of(burn(0.0), None);
        let read = |s: BalanceState, rate: f64, b: &Burns, now: u64| {
            let f = fetched(Account::Runpod, Some(Ok(live(30.63, Some(rate)))), None);
            let s = apply(s, std::slice::from_ref(&f), b, Mode::Inline, now);
            let v = views(&s, std::slice::from_ref(&f), b, &policy(), Mode::Inline, now).remove(0);
            (s, v)
        };
        let said = |v: &AccountView| {
            let n = notes(std::slice::from_ref(v), &policy(), NOW).iter().any(|l| l.contains("RunPod reports $0.14/h more than arena lists"));
            let t = teardown_info(std::slice::from_ref(v), NOW).iter().any(|l| l.contains("reports $0.14/h more"));
            assert_eq!(n, t, "notes and teardown agree");
            n
        };
        let t0 = NOW - H;
        // Just after the terminate: the rate lags — one reading, then another 2 min later.
        let (s, v) = read(BalanceState::default(), 0.14, &empty, t0);
        assert!(!said(&v) && rate_gap(&v).is_some());
        let lines = verbose_lines(std::slice::from_ref(&v), &s, &[fetched(Account::Runpod, None, None)], None, t0);
        assert!(lines.iter().any(|l| l.contains("gap $0.14/h, not yet confirmed")), "{lines:#?}");
        let (s, v) = read(s, 0.14, &empty, t0 + 120);
        assert!(!said(&v), "2 min apart");
        // The rate catches up: over.
        let (s, v) = read(s, 0.0, &empty, t0 + 240);
        assert!(!said(&v) && s.accounts[&Account::Runpod].gap_since.is_none());
        // A real gap (a network volume, say): shown once a reading 5 min after the first has it.
        let (s, v) = read(s, 0.14, &empty, t0 + 600);
        assert!(!said(&v));
        let (s, v) = read(s, 0.14, &empty, t0 + 600 + GAP_CONFIRM);
        assert!(said(&v));
        // A reading with no listing (the preflight's) neither ends it nor starts it over.
        let preflight = fetched(Account::Runpod, Some(Ok(live(30.63, Some(0.0)))), None);
        let s = apply(s, &[preflight], &Burns::new(), Mode::Inline, t0 + 1_000);
        assert_eq!(s.accounts[&Account::Runpod].gap_since, Some(t0 + 600));
        // A listing it can't judge by (a stand-in) starts it over.
        let guessed = burns_of(Some(Burn { per_hr: 0.32, guessed: 1, guessed_per_hr: 0.32, ..Default::default() }), None);
        let (s, _) = read(s, 0.46, &guessed, t0 + 1_200);
        assert_eq!(s.accounts[&Account::Runpod].gap_since, None);
    }

    /// Follow-up 6: under `--no-live -v` a cached `spendLimit` sat next to `underBalance ?` —
    /// cached facts are shown together, with their age (and Vast's owed figure too).
    #[test]
    fn cached_provider_facts_are_said_with_their_age() {
        let mut st = anchored(30.77, 33 * 60, AnchorSource::Live);
        st.provider = Some(ProviderFacts { usd: 30.77, burn_per_hr: Some(0.0), spend_limit_per_hr: Some(80.0), under_balance: Some(false), at: NOW - 33 * 60, owed: None });
        let s = state_with(st);
        let f = fetched(Account::Runpod, None, None);
        let v = one_view(&s, &f, burn(0.0), Mode::NoLive, NOW);
        assert_eq!(v.under_balance, None, "never judged by a cached underBalance");
        let lines = verbose_lines(std::slice::from_ref(&v), &s, &[f], None, NOW);
        assert!(lines.iter().any(|l| l == "  spendLimit $80.00/h · underBalance false (as read 33m ago)"), "{lines:#?}");
        // Read this run: no age.
        let f = fetched(Account::Runpod, Some(Ok(Live { spend_limit_per_hr: Some(80.0), under_balance: Some(false), ..live(30.77, Some(0.0)) })), None);
        let v = one_view(&s, &f, burn(0.0), Mode::Full, NOW);
        let lines = verbose_lines(std::slice::from_ref(&v), &s, &[f], None, NOW);
        assert!(lines.iter().any(|l| l == "  spendLimit $80.00/h · underBalance false"), "{lines:#?}");
        // Vast's owed figure from an older reading carries its age too.
        let mut vs = AccountState { key_fp: FP.into(), ..Default::default() };
        vs.provider = Some(ProviderFacts { usd: 10.0, burn_per_hr: None, spend_limit_per_hr: None, under_balance: None, at: NOW - 2 * H, owed: Some(1.5) });
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Vast, vs);
        let v = views(&s, &[fetched(Account::Vast, None, None)], &burns_of(None, burn(0.0)), &policy(), Mode::CacheOnly, NOW).remove(0);
        assert!(notes(&[v], &policy(), NOW).iter().any(|l| l.contains("reports $1.50 owed (its `balance`, as read 2h ago)")));
    }
}

/// Adversarial review (balance correctness): one regression test per finding. Each asserts
/// what the operator should be told; on the reviewed lane commit each one FAILS.
#[cfg(test)]
mod review_balance_correct {
    use super::*;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z, on the hour
    const H: u64 = 3_600;
    const FP: &str = "9c3e01ab";

    fn live(usd: f64, rate: Option<f64>) -> Live {
        Live { usd, provider_burn_per_hr: rate, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    fn rp(live: Option<std::result::Result<Live, Fail>>, billed: Option<std::result::Result<Billed, Fail>>) -> Fetched {
        Fetched { account: Account::Runpod, key_fp: FP.into(), live, billed }
    }

    fn burns(runpod: Option<Burn>) -> Burns {
        [(Account::Runpod, runpod), (Account::Vast, None)].into_iter().collect()
    }

    fn priced(r: f64) -> Option<Burn> {
        Some(Burn { per_hr: r, unpriced: 0, stale: false, ..Default::default() })
    }

    /// Running pods the listing has no price for (RunPod v1's REST list has no $/h; `arena
    /// balance` and `teardown --check` don't enrich, and after GraphQL nothing does).
    fn unpriced(n: usize) -> Option<Burn> {
        Some(Burn { per_hr: 0.0, unpriced: n, stale: false, ..Default::default() })
    }

    fn gone() -> Fail {
        Fail { kind: FailKind::Gone, http: Some(404), detail: None }
    }

    fn view_at(s: &BalanceState, f: &Fetched, b: Option<Burn>, mode: Mode, now: u64) -> AccountView {
        views(s, std::slice::from_ref(f), &burns(b), &BalancePolicy::default(), mode, now).remove(0)
    }

    /// BC2: `max(billed, metered)` of the TOTALS. When billing has seen spend metering
    /// can't (a pod created and destroyed while the operator's machine was off, a volume),
    /// billed wins overall — and then the listed spend inside billing's lag, which metering
    /// DID see, is dropped. A per-hour max keeps both.
    #[test]
    fn bc2_max_of_totals_drops_the_metered_spend_inside_the_billing_lag() {
        let t0 = NOW - 10 * H;
        // Live $100 at t0, the fleet listing $1/h — then the machine is off for 10 h.
        let s = apply(BalanceState::default(), &[rp(Some(Ok(live(100.0, None))), None)], &burns(priced(1.0)), Mode::Inline, t0);
        // Meanwhile a staff pod ran t0+2h..t0+5h at $2/h and was terminated. Truth: 10 × $1 +
        // 3 × $2 = $16 → $84. Billing (available!) lags 2 h: hours 8 and 9 still read $0.
        let buckets: Vec<(u64, f64)> = (0..=10u64)
            .map(|i| {
                let usd = match i {
                    8.. => 0.0,
                    2..=4 => 3.0,
                    _ => 1.0,
                };
                (t0 + i * H, usd)
            })
            .collect();
        let billed = Billed { buckets, from: t0, to: NOW + H };
        let f = rp(Some(Err(gone())), Some(Ok(billed)));
        let s = apply(s, std::slice::from_ref(&f), &burns(priced(1.0)), Mode::Inline, NOW);
        let v = view_at(&s, &f, priced(1.0), Mode::Inline, NOW);
        assert_eq!(v.source, Source::Estimate);
        let truth = 100.0 - 16.0;
        let est = v.balance.unwrap();
        assert!(
            est <= truth + 1e-9,
            "estimate ${est:.2} > truth ${truth:.2}: max(billed $14, metered $10) forgets the $2 metering saw in the 2 unbilled hours"
        );
    }

    /// BC1a: two running pods with no price, and no provider rate (live failing, or the
    /// minimal query): the burn is a floor of $0 — shown as "no burn", level ok.
    #[test]
    fn bc1a_unpriced_running_pods_read_as_no_burn() {
        let t0 = NOW - H;
        let s = apply(BalanceState::default(), &[rp(Some(Ok(live(30.0, None))), None)], &burns(priced(1.0)), Mode::Inline, t0);
        let v = view_at(&s, &rp(Some(Err(gone())), None), unpriced(2), Mode::Inline, NOW);
        assert_eq!(v.source, Source::Estimate);
        assert_ne!(fmt_runway(&v), "no burn", "two pods running at unknown prices is not 'no burn': {v:?}");
    }

    /// BC1b: no balance while unpriced pods run is "unknown while burning" (`--check` 3),
    /// not ok (0) — the spec's Unknown level is "balance ? while B > 0 or B is unknown".
    #[test]
    fn bc1b_unknown_balance_with_unpriced_pods_is_ok_instead_of_unknown() {
        let v = view_at(&BalanceState::default(), &rp(Some(Err(gone())), None), unpriced(2), Mode::Inline, NOW);
        assert_eq!(v.balance, None);
        assert_eq!((v.level, check_exit(std::slice::from_ref(&v))), (Level::Unknown, 3), "{v:?}");
    }

    /// BC1c: invariant 7 ("a failed listing never yields … runway ∞/no burn"): the stale
    /// stand-in for a failed listing is the last sample — and when that was $0 the row
    /// reads "no burn", level ok.
    #[test]
    fn bc1c_failed_listing_with_a_stale_zero_sample_reads_no_burn() {
        let t0 = NOW - 2 * H;
        let s = apply(BalanceState::default(), &[rp(Some(Ok(live(30.0, None))), None)], &burns(priced(0.0)), Mode::Inline, t0);
        let v = view_at(&s, &rp(Some(Err(gone())), None), None, Mode::Inline, NOW);
        assert_ne!(fmt_runway(&v), "no burn", "the listing failed: the burn is unknown, never 'no burn': {v:?}");
    }

    /// BC4: a floor burn (unpriced pods) is stored as an exact sample, so metering charges
    /// the pods $0 between two such looks.
    #[test]
    fn bc4_a_floor_burn_is_recorded_as_an_exact_sample() {
        let t0 = NOW - 3 * H;
        // One pod at $1/h all along: priced at t0 (pods list, enriched) …
        let mut s = apply(BalanceState::default(), &[rp(Some(Ok(live(100.0, None))), None)], &burns(priced(1.0)), Mode::Inline, t0);
        // … then two looks that list it without a price (teardown --check, arena balance) while live fails.
        let f = rp(Some(Err(gone())), None);
        s = apply(s, std::slice::from_ref(&f), &burns(unpriced(1)), Mode::Inline, t0 + H);
        s = apply(s, std::slice::from_ref(&f), &burns(unpriced(1)), Mode::Inline, t0 + 2 * H);
        // The next priced look.
        let v = view_at(&s, &f, priced(1.0), Mode::Inline, NOW);
        let truth = 100.0 - 3.0;
        assert!(
            v.balance.unwrap() <= truth + 1e-9,
            "estimate {:?} > truth {truth}: samples {:?}",
            v.balance,
            s.accounts[&Account::Runpod].samples
        );
    }

    /// BC3: the fallback check integrates samples that were recorded at the provider's own
    /// rate (GraphQL `currentSpendPerHr`) — which the real fallback won't have once GraphQL
    /// is gone. With $0.50/h arena doesn't list and billing down, the check says ✓ while the
    /// fallback-to-be would read $10 HIGH.
    #[test]
    fn bc3_fallback_check_meters_with_the_rate_the_fallback_will_not_have() {
        let t0 = NOW - 20 * H;
        let listed = burns(priced(1.0));
        let billing_down = || Some(Err(Fail { kind: FailKind::Auth, http: Some(403), detail: None }));
        let mut s = apply(BalanceState::default(), &[rp(Some(Ok(live(100.0, Some(1.5)))), billing_down())], &listed, Mode::Full, t0);
        // The TUI looks every hour, live working (RunPod's rate $1.50/h: $0.50/h volumes).
        for i in 1..20u64 {
            s = apply(s, &[rp(Some(Ok(live(100.0 - 1.5 * i as f64, Some(1.5)))), None)], &listed, Mode::Inline, t0 + i * H);
        }
        // `arena balance` 20 h on: live $70.
        s = apply(s, &[rp(Some(Ok(live(70.0, Some(1.5)))), billing_down())], &listed, Mode::Full, NOW);
        let c = s.accounts[&Account::Runpod].check.expect("a check ran");
        // What the fallback would estimate from the same check anchor once GraphQL is gone:
        // $100 − 20 h × the listed $1/h = $80 — $10 over the truth.
        let fallback_world: Vec<(u64, f64)> = (0..20u64).map(|i| (t0 + i * H, 1.0)).collect();
        let would = estimate(&Anchor { usd: 100.0, at: t0, source: AnchorSource::Live }, &fallback_world, None, 1.0, NOW).usd;
        assert!((would - 80.0).abs() < 1e-6);
        assert_eq!(
            c.verdict,
            Verdict::Overstates,
            "check predicted ${:.2} (provider-rate samples) vs the fallback's ${would:.2}; live ${:.2}",
            c.predicted,
            c.live
        );
    }

    /// BC5: `arena balance` / `teardown --check` list without details; RunPod v1's list has
    /// no $/h, so every running pod is unpriced and `listed_per_hr` is $0 — and the whole
    /// provider rate is then blamed on "network volumes, stopped pods' disks, serverless?".
    #[test]
    fn bc5_unpriced_pods_are_reported_as_spend_arena_does_not_list() {
        let f = rp(Some(Ok(live(32.54, Some(0.47)))), None);
        let v = view_at(&BalanceState::default(), &f, unpriced(1), Mode::Full, NOW);
        let lines: Vec<String> = notes(std::slice::from_ref(&v), &BalancePolicy::default(), NOW).into_iter().chain(teardown_info(&[v], NOW)).collect();
        for l in &lines {
            assert!(
                !l.contains("more than arena lists (network volumes, stopped pods' disks, serverless?)"),
                "the $0.47/h is arena's own unpriced pod, not unlisted spend: {l}"
            );
        }
    }

    /// BC6: SAMPLE_MAX drops the OLDEST samples, whatever the anchor needs: an anchor older
    /// than the kept samples is then metered at the first kept sample's rate.
    #[test]
    fn bc6_the_sample_cap_drops_samples_the_anchor_still_needs() {
        let t0 = NOW - 10 * 86_400;
        let anchor = Anchor { usd: 10_000.0, at: t0, source: AnchorSource::Live };
        // A busy cohort with GraphQL gone: the burn swings $5 ↔ $0.10 every 6 min (each a
        // change point), 2 400 samples over the anchor's 10 days.
        let samples: Vec<(u64, f64)> = (0..2_400u64).map(|i| (t0 + i * 360, if i % 2 == 1 { 0.1 } else { 5.0 })).collect();
        let full = estimate(&anchor, &samples, None, 5.0, NOW).usd;
        let st = AccountState {
            key_fp: FP.into(),
            anchor: Some(anchor),
            live_fail: Some(FailRecord { kind: FailKind::Gone, http: Some(404), detail: None, since: t0 + 60, last: NOW - H }),
            samples,
            ..Default::default()
        };
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Runpod, st);
        let f = rp(Some(Err(gone())), None);
        let s = apply(s, std::slice::from_ref(&f), &burns(priced(5.0)), Mode::Inline, NOW);
        let v = view_at(&s, &f, priced(5.0), Mode::Inline, NOW);
        assert!(
            v.balance.unwrap() <= full + 1e-6,
            "after the cap the estimate rose from ${full:.2} to ${:.2} ({} samples kept, first at +{}h)",
            v.balance.unwrap(),
            s.accounts[&Account::Runpod].samples.len(),
            (s.accounts[&Account::Runpod].samples[0].0 - t0) / H
        );
    }

    /// BC7: the single-spec preflight ("+ new pods (price unknown)") still states a runway
    /// "after" them as if they were free — `≈`, and no warning even for 20 new pods.
    #[test]
    fn bc7_preflight_with_unknown_price_states_a_runway_that_ignores_the_new_pods() {
        let v = view_at(&BalanceState::default(), &rp(Some(Ok(live(100.0, Some(1.0)))), None), priced(1.0), Mode::Inline, NOW);
        let lines = preflight_lines(&v, 20, None, &BalancePolicy::default(), NOW);
        assert!(!lines[0].contains("→ ≈4.1d"), "the runway after 20 pods of unknown price can't be ≈100 h: {}", lines[0]);
    }

    /// BC8: a stopped Vast instance still bills its disk (as `teardown` itself says), Vast has
    /// no billing history, so metering is all there is — and stopped instances are $0 there:
    /// shown "no burn", and the estimate never counts it (an undercount the docs don't list).
    #[test]
    fn bc8_stopped_vast_instances_disk_is_never_counted() {
        let pod = Pod { provider: "vast".into(), status: "EXITED".into(), cost_per_hr: Some(0.3), ..Default::default() };
        let l = Listing::from_results(vec![("vast".into(), Ok(vec![pod]))]);
        let b = super::burns(&l, &l.pods())[&Account::Vast].unwrap();
        assert!(b.per_hr > 0.0 || b.floor(), "a stopped Vast instance's disk is spend the estimate must count (or at least call the burn a floor): {b:?}");
    }

    /// BC9: a fallback-check verdict is shown with no age: after GraphQL died ten days ago the
    /// old ✓ still reads as current next to "live balance failing".
    #[test]
    fn bc9_an_old_fallback_check_reads_as_current() {
        let t0 = NOW - 10 * 86_400;
        let st = AccountState {
            key_fp: FP.into(),
            anchor: Some(Anchor { usd: 100.0, at: t0, source: AnchorSource::Live }),
            check: Some(CheckRecord { at: t0, horizon_secs: 26 * H, predicted: 99.6, predicted_metered: 99.6, live: 100.0, verdict: Verdict::Safe }),
            live_fail: Some(FailRecord { kind: FailKind::Gone, http: Some(404), detail: None, since: t0 + 60, last: NOW }),
            samples: vec![(t0, 0.1)],
            ..Default::default()
        };
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Runpod, st);
        let v = view_at(&s, &rp(None, None), priced(0.1), Mode::Inline, NOW);
        let n = notes(&[v], &BalancePolicy::default(), NOW);
        let check = n.iter().find(|l| l.contains("fallback check")).expect("check note");
        assert!(check.contains("10d ago") || check.contains("2026-09-28"), "a 10-day-old verdict must say how old it is: {check}");
    }

    /// BC10: a live reading under FRESH old is still shown as **live** after a newer
    /// failure (per spec) — but the note under it says "estimates in use"; and `config check`
    /// says the same when the anchor is past 14 days and the balance is `?`.
    #[test]
    fn bc10_estimates_in_use_is_said_where_none_is() {
        let st = AccountState {
            key_fp: FP.into(),
            provider: Some(ProviderFacts { usd: 40.0, burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, at: NOW - 120, owed: None }),
            anchor: Some(Anchor { usd: 40.0, at: NOW - 120, source: AnchorSource::Live }),
            live_ok_at: Some(NOW - 120),
            live_fail: Some(FailRecord { kind: FailKind::Auth, http: Some(403), detail: None, since: NOW - 60, last: NOW - 60 }),
            ..Default::default()
        };
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Runpod, st);
        let auth = Fail { kind: FailKind::Auth, http: Some(403), detail: None };
        let v = view_at(&s, &rp(Some(Err(auth)), None), priced(0.5), Mode::Full, NOW);
        assert_eq!(v.source, Source::Live);
        let n = notes(&[v], &BalancePolicy::default(), NOW);
        assert!(!n.iter().any(|l| l.contains("estimates in use")), "the row is live, yet: {n:?}");
    }
}

/// Adversarial re-review of the review fixes (9147170 + 05f6614): one test per new finding.
/// Each asserts what the operator should be told; on 05f6614 each one FAILS.
#[cfg(test)]
mod review_balance_2 {
    use super::*;
    use crate::proxy::Listing;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z, on the hour
    const H: u64 = 3_600;
    const D: u64 = 86_400;
    const FP: &str = "9c3e01ab";

    fn live(usd: f64, rate: Option<f64>) -> Live {
        Live { usd, provider_burn_per_hr: rate, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    fn fetched(account: Account, live: Option<std::result::Result<Live, Fail>>) -> Fetched {
        Fetched { account, key_fp: FP.into(), live, billed: None }
    }

    fn listing(provider: &str, pods: Vec<Pod>) -> Listing {
        Listing::from_results(vec![(provider.to_string(), Ok(pods))])
    }

    /// B2-1: the "dearer preset" stand-in is below what RunPod's own recorded catalog (the
    /// fixture the ceiling test reads) charges for the same GPU — H100 SXM secure $3.49 vs
    /// the $2.79 stand-in, RTX 4090 $0.74 vs $0.69, RTX 3090 $0.50 vs $0.43, L40S, A40. The
    /// stand-in is documented and shown as "errs high" (`≤~$X/h`), and the preflight's
    /// "≥1h of credit" check then misses a create RunPod will refuse.
    #[test]
    fn b2_1_preset_stand_in_is_never_below_runpods_recorded_price() {
        let catalog: Value =
            serde_json::from_str(include_str!("provider/fixtures/runpod_v2/catalog_community_cuda13.json")).unwrap();
        let p = BalancePolicy::default();
        let below: Vec<String> = catalog["body"]["gpus"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| {
                let id = g["id"].as_str()?;
                let dearest = ["community", "secure"].iter().filter_map(|k| g["price"][*k].as_f64()).fold(0.0, f64::max);
                match stand_in(Some(id), Some(1), &p) {
                    Some((s, StandIn::Preset)) if s + 1e-9 < dearest => Some(format!("{id}: stand-in ${s:.2} < RunPod ${dearest:.2}")),
                    _ => None,
                }
            })
            .collect();
        // The preflight consequence: 10 × H100 SXM on secure cost $34.90/h; $30 is under an
        // hour of it, so RunPod will refuse the create (402) — the stand-in says $27.90.
        let f = fetched(Account::Runpod, Some(Ok(live(30.0, Some(0.0)))));
        let v = views(&BalanceState::default(), &[f], &Burns::new(), &p, Mode::Inline, NOW).remove(0);
        let (per_pod, _) = stand_in(Some("NVIDIA H100 80GB HBM3"), Some(1), &p).unwrap();
        let lines = preflight_lines_with(&v, NewPods { count: 10, per_pod: Some(per_pod), stand_in: true, ceiling: false, quoted: None }, &p, NOW);
        assert!(
            below.is_empty() && lines.iter().any(|l| l.contains("needs ≥1h of credit")),
            "stand-ins below RunPod's price: {below:#?}\npreflight: {lines:#?}"
        );
    }

    /// B2-2: a sub-cent floor (a listing of stopped pods) with a known balance is now level
    /// Unknown (`--check` 3) — but an UNKNOWN burn (the listing failed; the last sample was $0,
    /// which the fix rightly no longer trusts) with the same balance is level Ok (`--check` 0):
    /// strictly less information reads better. Vast live $3, its instance listing down.
    #[test]
    fn b2_2_an_unknown_burn_with_a_known_balance_is_not_ok() {
        let mut s = BalanceState::default();
        s.accounts.insert(Account::Vast, AccountState { key_fp: FP.into(), samples: vec![(NOW - 2 * H, 0.0)], ..Default::default() });
        let f = fetched(Account::Vast, Some(Ok(live(3.0, None))));
        let failed: Burns = [(Account::Runpod, None), (Account::Vast, None)].into_iter().collect();
        let v = views(&s, std::slice::from_ref(&f), &failed, &BalancePolicy::default(), Mode::Inline, NOW).remove(0);
        assert_eq!((v.burn, fmt_runway(&v).as_str()), (None, "?"), "fixture: the burn is unknown");
        // The same account with its listing up and three stopped instances: Unknown.
        let stopped = listing("vast", (0..3).map(|_| Pod { provider: "vast".into(), status: "exited".into(), ..Default::default() }).collect());
        let w = views(&s, std::slice::from_ref(&f), &burns(&stopped, &stopped.pods()), &BalancePolicy::default(), Mode::Inline, NOW).remove(0);
        assert_eq!(w.level, Level::Unknown, "fixture: a sub-cent floor is Unknown");
        assert_ne!(
            (v.level, check_exit(std::slice::from_ref(&v))),
            (Level::Ok, 0),
            "$3 left, burn unknown, reads ok / `--check` 0 — while stopped disks read unknown / 3"
        );
    }

    /// B2-3: level Unknown is now also reached WITH a live balance (a sub-cent floor: only
    /// stopped pods, e.g. after the nightly `pods stop --all`; or a running pod with no GPU
    /// info) — and the note still says "no usable reading — … arena balance set", a dead end
    /// (`set` can't change a burn-driven Unknown) that contradicts the live row above it.
    #[test]
    fn b2_3_a_burn_driven_unknown_does_not_say_no_usable_reading() {
        let stopped = listing("vast", (0..3).map(|_| Pod { provider: "vast".into(), status: "exited".into(), ..Default::default() }).collect());
        let f = fetched(Account::Vast, Some(Ok(live(500.0, None))));
        let v = views(&BalanceState::default(), &[f], &burns(&stopped, &stopped.pods()), &BalancePolicy::default(), Mode::Inline, NOW).remove(0);
        assert_eq!((v.source, v.balance, v.level), (Source::Live, Some(500.0), Level::Unknown), "fixture");
        let n = notes(std::slice::from_ref(&v), &BalancePolicy::default(), NOW);
        assert!(!n.iter().any(|l| l.contains("no usable reading")), "a live $500 reading, yet: {n:#?}");
    }

    /// B2-4 (BS6, CLI half): the writability probe creates an EMPTY file; the save writes the
    /// state and renames it over `balance.json`. Whatever lets the first and not the second
    /// — a full disk or a block quota (an inode, no blocks), a `balance.json` that can't be
    /// replaced — passes the probe, and every short-lived `pods list` reads live again
    /// (no in-process memory survives between commands). Here `balance.json` is a directory.
    #[tokio::test]
    async fn b2_4_a_probe_that_passes_while_the_save_fails_still_bounds_live_calls() {
        let dir = std::env::temp_dir().join(format!("arena-review-b2-4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devtest").join(crate::snapshot::BALANCE_FILE);
        std::fs::create_dir_all(&path).unwrap(); // the probe's file fits beside it; the state can't replace it
        let burns: Burns = [(Account::Runpod, Some(Burn { per_hr: 0.5, ..Default::default() }))].into_iter().collect();
        let mut calls = 0;
        for i in 0..3 {
            let now = NOW + i * 60; // `watch -n60 arena pods list`: a new process each time
            let api = FakeBalanceApi::new().live_ok(Account::Runpod, live(40.0, Some(0.5)));
            let mode = if writable(&path) { Mode::Inline } else { Mode::CacheOnly }; // BalanceCtx::inline_mode
            let (state, _) = load(&path);
            let fetched = observe(&api, &state, mode, INLINE_TIMEOUT, now).await;
            let (_, warning) = commit(Some(&path), state, &fetched, &burns, mode, now);
            assert!(mode != Mode::Inline || warning.is_some(), "fixture: the save fails");
            calls += api.live_calls();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(calls <= 1, "{calls} live calls in 3 minutes (FRESH = 5 min)");
    }

    /// B2-5: a remembered price expires 15 days after the pod was last seen WITH a price —
    /// even while the same pod keeps being listed (unpriced) every day. On RunPod v1 after
    /// GraphQL (nothing prices a pod any more: the fallback's whole purpose), every pod of a
    /// long cohort then jumps to the ceiling ($8/GPU-h for a $0.17 A4000: 47×), the estimate
    /// collapses and every surface reads critical "probably a top-up".
    #[test]
    fn b2_5_a_pod_still_listed_keeps_its_remembered_price() {
        let pod = |cost: Option<f64>| Pod {
            id: "p1".into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            cost_per_hr: cost,
            gpu_count: Some(1),
            ..Default::default()
        };
        let p = BalancePolicy::default();
        let down = fetched(Account::Runpod, Some(Err(Fail::transient("503"))));
        // Day 0: an enriched look prices p1 at $0.17.
        let t0 = NOW - 16 * D;
        let priced = listing("runpod", vec![pod(Some(0.17))]);
        let mut s = apply(BalanceState::default(), std::slice::from_ref(&down), &burns_with(&priced, &priced.pods(), &BalanceState::default(), &p), Mode::Inline, t0);
        // Then daily looks at the REST list alone: the same pod, no price.
        let bare = listing("runpod", vec![pod(None)]);
        for d in 1..=16 {
            let b = burns_with(&bare, &bare.pods(), &s, &p);
            s = apply(s, std::slice::from_ref(&down), &b, Mode::Inline, t0 + d * D);
        }
        let b = burns_with(&bare, &bare.pods(), &s, &p)[&Account::Runpod].unwrap();
        assert_eq!((b.per_hr, b.guessed), (0.17, 0), "the same pod, listed every day, is now priced at {b:?}");
    }
}
