//! Money left on the provider accounts, and how long it lasts at the current burn.
//!
//! RunPod and Vast are **prepaid**: when the balance hits zero the provider stops every pod
//! (Vast: "your instances are stopped automatically"; ops playbook §9: "an empty balance
//! stops every pod"). Mid-cohort that is the outage nobody sees coming, so `arena balance`,
//! the `pods list` footer, `teardown --check` and the TUI's summary bar all say how much is
//! left and roughly when it runs out. Hetzner bills monthly in arrears — nothing to run out.
//!
//! Where the numbers come from (field names verified live, 2026-10-08):
//! - **RunPod**: GraphQL `myself { clientBalance currentSpendPerHr spendLimit underBalance }`.
//!   REST v2 has billing *history* only, so this stays on GraphQL; when RunPod retires it the
//!   query fails, the balance reads "unavailable", and nothing else breaks.
//! - **Vast**: `GET /api/v0/users/current/` → `credit` (the prepaid amount, what the console
//!   shows). Its docs only say `balance` is "the current balance of the user"; a *negative*
//!   `balance` is shown as possibly owed, never folded into the runway on a guess. That
//!   response also carries the account's email and **API key**, so only `credit`/`balance`
//!   are read and the body is never quoted — not even in a shape error.
//!
//! **Burn** is the higher of the provider's own spend rate (RunPod's covers everything on the
//! account: storage, volumes, pods we don't list) and this fleet's billing pods on that
//! provider (`status::bills_hourly`, `cost_per_hr`), so neither view can make the runway look
//! longer than the other says. **Runway** = balance ÷ burn. Below `BALANCE_WARN_HOURS`
//! (default 48) — or at/under zero, or RunPod's own `underBalance` flag — a row is a ⚠.
//! Unknown is never zero: a failed read, or a provider whose pods couldn't be listed (and
//! that reports no rate of its own), says "?" rather than "not burning".
//!
//! Pure judging/rendering here (table-tested on fixture-shaped bodies); the two fetches are
//! status-first ([`crate::http`]) and each bounded, so a stalled API costs a footer at most
//! its budget.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fleet::{currency_symbol, fmt_money};
use crate::http::send_json;
use crate::pod::Pod;
use crate::provider::runpod::{graphql_errors, loose_f64};
use crate::status::bills_hourly;
use crate::table::{self, Align};

/// `BALANCE_WARN_HOURS` when unset: two days is enough to notice and top up over a weekend.
pub const DEFAULT_WARN_HOURS: f64 = 48.0;

/// Budget for one provider's balance read. Best-effort surfaces (a footer, the TUI) must not
/// wait on a stalled API; `arena balance` uses the same budget.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The RunPod account query. Only these four fields: each was checked live, and GraphQL
/// rejects a whole query over one unknown field. No id, no email.
pub const RUNPOD_QUERY: &str = "{ myself { clientBalance currentSpendPerHr spendLimit underBalance } }";

/// What one provider's account says.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Account {
    /// Money paid in advance (RunPod, Vast).
    Prepaid {
        /// What's left, in the provider's currency.
        balance: f64,
        /// The provider's own current spend rate per hour (RunPod `currentSpendPerHr`): every
        /// charge on the account, not just the pods we list. `None` = not reported.
        provider_per_hr: Option<f64>,
        /// RunPod's account spend limit per hour (`spendLimit`), shown as a note.
        spend_limit_per_hr: Option<f64>,
        /// RunPod's `underBalance` flag: the account is below what it needs to keep running.
        under_balance: Option<bool>,
        /// Vast: the magnitude of a negative `balance`, shown (not used) — see the module doc.
        owed: Option<f64>,
    },
    /// Billed in arrears (Hetzner): there is no balance to run out.
    Postpaid,
}

/// One configured provider's account read: the account, or why it couldn't be read.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountProbe {
    pub provider: String,
    pub account: std::result::Result<Account, String>,
}

/// A finite, non-negative rate (a negative or NaN "rate" is noise, not a credit).
fn rate(v: Option<&Value>) -> Option<f64> {
    v.and_then(loose_f64).filter(|r| r.is_finite() && *r >= 0.0)
}

/// Parse RunPod's `{ data: { myself: { … } } }`. Fails closed: without a numeric
/// `clientBalance` there is no balance (a GraphQL error is quoted; the body never is). With
/// one, GraphQL errors about the other fields leave just those fields unknown.
pub fn parse_runpod(v: &Value) -> Result<Account> {
    let me = v.pointer("/data/myself").filter(|m| m.is_object());
    let balance = me.and_then(|m| m.get("clientBalance")).and_then(loose_f64).filter(|b| b.is_finite());
    let Some(balance) = balance else {
        return Err(Error::provider(match graphql_errors(v) {
            Some(e) => format!("runpod balance: {e}"),
            None => "runpod balance: no numeric data.myself.clientBalance in the response".to_string(),
        }));
    };
    let me = me.expect("a balance was read from it");
    Ok(Account::Prepaid {
        balance,
        provider_per_hr: rate(me.get("currentSpendPerHr")),
        spend_limit_per_hr: rate(me.get("spendLimit")).filter(|l| *l > 0.0),
        under_balance: me.get("underBalance").and_then(Value::as_bool),
        owed: None,
    })
}

/// Parse Vast's `/users/current/` object. Only `credit` (required) and `balance` are read;
/// the error for anything else never shows the body — it holds the email and the API key.
pub fn parse_vast(v: &Value) -> Result<Account> {
    let Some(credit) = v.get("credit").and_then(loose_f64).filter(|c| c.is_finite()) else {
        return Err(Error::provider(
            "vast balance: no numeric `credit` in /users/current/ (the response isn't shown: it holds the account's \
             email and API key)",
        ));
    };
    let owed = v.get("balance").and_then(loose_f64).filter(|b| b.is_finite() && *b < 0.0).map(|b| -b);
    Ok(Account::Prepaid { balance: credit, provider_per_hr: None, spend_limit_per_hr: None, under_balance: None, owed })
}

/// RunPod's account over GraphQL at `url` (bearer auth — the key never goes in the URL).
pub async fn fetch_runpod_at(client: &Client, url: &str, api_key: &str) -> Result<Account> {
    let body = json!({ "query": RUNPOD_QUERY });
    let v = send_json(client.post(url).bearer_auth(api_key).json(&body), "runpod balance").await?;
    parse_runpod(&v)
}

/// Vast's account at `{base}/users/current/`.
pub async fn fetch_vast_at(client: &Client, base: &str, api_key: &str) -> Result<Account> {
    let url = format!("{}/users/current/", base.trim_end_matches('/'));
    let v = send_json(client.get(url).bearer_auth(api_key), "vast balance").await?;
    parse_vast(&v)
}

/// `fut` within `limit`, a stall becoming that provider's error.
async fn bounded<F: std::future::Future<Output = Result<Account>>>(what: &str, limit: Duration, fut: F) -> Result<Account> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::provider(format!("{what} balance timed out after {}s", limit.as_secs_f64()))),
    }
}

/// A configured (non-empty) key.
fn key<'a>(cfg: &'a Config, name: &str) -> Option<&'a str> {
    cfg.get(name).map(str::trim).filter(|k| !k.is_empty())
}

/// Read every configured provider's account at once, each within `limit`: RunPod and Vast
/// when their key is set, Hetzner (no call — postpaid) when its key is set. Providers with
/// no key are left out. Never fails: a provider that couldn't be read carries its reason.
pub async fn fetch_all(cfg: &Config, limit: Duration) -> Vec<AccountProbe> {
    let client = Client::new();
    let runpod = async {
        match key(cfg, "RUNPOD_API_KEY") {
            Some(k) => Some(bounded("runpod", limit, fetch_runpod_at(&client, crate::provider::runpod::GRAPHQL, k)).await),
            None => None,
        }
    };
    let vast = async {
        match key(cfg, "VAST_API_KEY") {
            Some(k) => Some(bounded("vast", limit, fetch_vast_at(&client, crate::provider::vast::BASE, k)).await),
            None => None,
        }
    };
    let (runpod, vast) = tokio::join!(runpod, vast);
    let mut out = Vec::new();
    for (provider, read) in [("runpod", runpod), ("vast", vast)] {
        if let Some(r) = read {
            out.push(AccountProbe { provider: provider.into(), account: r.map_err(|e| e.to_string()) });
        }
    }
    if key(cfg, "HETZNER_API_KEY").is_some() {
        out.push(AccountProbe { provider: "hetzner".into(), account: Ok(Account::Postpaid) });
    }
    out
}

/// `BALANCE_WARN_HOURS`: unset/empty → [`DEFAULT_WARN_HOURS`]; a number of hours ≥ 0 (`0`
/// turns the runway warning off; zero balance and `underBalance` still warn); anything else
/// is an error rather than a silent default — a typo must not quietly drop the alarm.
pub fn warn_hours(cfg: &Config) -> std::result::Result<f64, String> {
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

/// `BALANCE_WARN_HOURS` for a best-effort surface (the `pods list` footer, `teardown
/// --check`, the TUI): a bad value never costs the surface its output — the default is
/// used, and the second value is the line saying so. `arena balance` itself refuses it
/// ([`warn_hours`]).
pub fn warn_hours_or_default(cfg: &Config) -> (f64, Option<String>) {
    match warn_hours(cfg) {
        Ok(h) => (h, None),
        Err(e) => (DEFAULT_WARN_HOURS, Some(format!("warning: {e} — using {DEFAULT_WARN_HOURS}"))),
    }
}

/// This fleet's hourly cost on one provider: its billing pods, and how many had no price
/// (then the sum is a floor).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Burn {
    pub per_hr: f64,
    pub billing: usize,
    pub unpriced: usize,
}

/// Per-provider fleet cost from a listing. A provider in `failed` (its listing errored) is
/// *unknown*, never "no pods"; every other provider not seen in `pods` had none billing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Burns {
    known: HashMap<String, Burn>,
    failed: HashSet<String>,
    /// Nothing is known about any provider (e.g. `teardown --check`, which reads no costs).
    all_unknown: bool,
}

impl Burns {
    /// No fleet cost known anywhere: runways rest on the providers' own rates.
    pub fn unknown() -> Self {
        Burns { all_unknown: true, ..Default::default() }
    }

    /// The fleet's burn on `provider`, `None` when unknown.
    pub fn of(&self, provider: &str) -> Option<Burn> {
        if self.all_unknown || self.failed.contains(provider) {
            return None;
        }
        Some(self.known.get(provider).copied().unwrap_or_default())
    }
}

/// Sum the billing pods per provider ([`bills_hourly`], as the `pods list` footer counts).
pub fn burns(pods: &[Pod], failed: &[String]) -> Burns {
    let mut known: HashMap<String, Burn> = HashMap::new();
    for p in pods.iter().filter(|p| bills_hourly(&p.provider, &p.status)) {
        let b = known.entry(p.provider.clone()).or_default();
        b.billing += 1;
        match p.cost_per_hr.filter(|c| c.is_finite() && *c >= 0.0) {
            Some(c) => b.per_hr += c,
            None => b.unpriced += 1,
        }
    }
    Burns { known, failed: failed.iter().cloned().collect(), all_unknown: false }
}

/// How long a balance lasts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Runway {
    /// Runs out in this many hours at the current burn.
    Hours { hours: f64 },
    /// The provider itself reports no spend, and no billing pod of ours says otherwise.
    NotBurning,
    /// The provider reports no rate of its own, and none of our pods there bills (storage of
    /// stopped instances may still cost a little).
    NothingBilling,
    /// Postpaid: nothing to run out.
    Postpaid,
    /// Couldn't tell (balance unreadable, or neither a provider rate nor a listing).
    Unknown,
}

/// One provider's line of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub provider: String,
    pub currency: &'static str,
    /// `prepaid`, `postpaid` or `unknown` (couldn't read it).
    pub billing: &'static str,
    pub balance: Option<f64>,
    pub provider_per_hr: Option<f64>,
    pub spend_limit_per_hr: Option<f64>,
    pub under_balance: Option<bool>,
    pub owed: Option<f64>,
    /// This fleet's billing pods there (`None` = their listing failed).
    pub fleet: Option<Burn>,
    /// What the runway is computed at: the higher of the provider's rate and the fleet's.
    pub burn_per_hr: Option<f64>,
    pub runway: Runway,
    /// The fleet's sum left out unpriced pods and the provider gave no rate of its own: the
    /// runway may be shorter than shown.
    pub runway_upper_bound: bool,
    /// When it runs out, RFC 3339 UTC (only for a runway under a year).
    pub runs_out_at: Option<String>,
    /// Below `BALANCE_WARN_HOURS`, at/under zero, or flagged by the provider.
    pub warn: bool,
    pub error: Option<String>,
}

/// Every configured provider's row, plus the threshold used.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub warn_hours: f64,
    pub generated_at: String,
    pub rows: Vec<Row>,
    /// Rows that are a ⚠.
    pub warnings: usize,
    /// Rows whose balance couldn't be read.
    pub unknown: usize,
}

/// Runways past this are "over a year": no date is worth printing.
const YEAR_HOURS: f64 = 24.0 * 365.0;

/// Judge one account against the fleet's burn on that provider. Pure.
fn row(probe: &AccountProbe, fleet: Option<Burn>, warn_hours: f64, now: u64) -> Row {
    let mut r = Row {
        provider: probe.provider.clone(),
        currency: currency_symbol(&probe.provider),
        billing: "unknown",
        balance: None,
        provider_per_hr: None,
        spend_limit_per_hr: None,
        under_balance: None,
        owed: None,
        fleet,
        burn_per_hr: None,
        runway: Runway::Unknown,
        runway_upper_bound: false,
        runs_out_at: None,
        warn: false,
        error: None,
    };
    match &probe.account {
        Err(e) => r.error = Some(e.clone()),
        Ok(Account::Postpaid) => {
            r.billing = "postpaid";
            r.runway = Runway::Postpaid;
        }
        Ok(Account::Prepaid { balance, provider_per_hr, spend_limit_per_hr, under_balance, owed }) => {
            r.billing = "prepaid";
            r.balance = Some(*balance);
            r.provider_per_hr = *provider_per_hr;
            r.spend_limit_per_hr = *spend_limit_per_hr;
            r.under_balance = *under_balance;
            r.owed = *owed;
            let fleet_rate = fleet.map(|b| b.per_hr);
            r.burn_per_hr = match (*provider_per_hr, fleet_rate) {
                (Some(p), Some(f)) => Some(p.max(f)),
                (p, f) => p.or(f),
            };
            let unpriced = fleet.is_some_and(|b| b.unpriced > 0);
            r.runway = match r.burn_per_hr {
                Some(b) if b > 0.0 => Runway::Hours { hours: balance.max(0.0) / b },
                // Pods billing at a price nobody reported: the burn isn't really zero.
                Some(_) if unpriced && provider_per_hr.is_none() => Runway::Unknown,
                Some(_) if provider_per_hr.is_some() => Runway::NotBurning,
                Some(_) => Runway::NothingBilling,
                None => Runway::Unknown,
            };
            r.runway_upper_bound = unpriced && provider_per_hr.is_none() && matches!(r.runway, Runway::Hours { .. });
            if let Runway::Hours { hours } = r.runway {
                if hours < YEAR_HOURS {
                    r.runs_out_at = Some(crate::snapshot::rfc3339(now + (hours * 3600.0) as u64));
                }
            }
            r.warn = *balance <= 0.0
                || *under_balance == Some(true)
                || matches!(r.runway, Runway::Hours { hours } if hours < warn_hours);
        }
    }
    r
}

/// The report for these account reads against the fleet's burns. Pure (`now` is passed in).
pub fn report(probes: &[AccountProbe], burns: &Burns, warn_hours: f64, now: u64) -> Report {
    let rows: Vec<Row> = probes.iter().map(|p| row(p, burns.of(&p.provider), warn_hours, now)).collect();
    Report {
        warn_hours,
        generated_at: crate::snapshot::rfc3339(now),
        warnings: rows.iter().filter(|r| r.warn).count(),
        unknown: rows.iter().filter(|r| r.error.is_some()).count(),
        rows,
    }
}

/// `~45m`, `~65h`, `~5.4d`, `~23d`, `over a year`.
pub fn fmt_runway_hours(hours: f64) -> String {
    if hours >= YEAR_HOURS {
        "over a year".into()
    } else if hours < 1.0 {
        format!("~{}m", (hours * 60.0).floor() as u64)
    } else if hours < 72.0 {
        format!("~{}h", hours.floor() as u64)
    } else if hours < 240.0 {
        format!("~{:.1}d", hours / 24.0)
    } else {
        format!("~{}d", (hours / 24.0).floor() as u64)
    }
}

/// `2026-10-11 05:00` from an RFC 3339 UTC time.
fn short_time(rfc: &str) -> String {
    rfc.get(..16).map(|s| s.replacen('T', " ", 1)).unwrap_or_else(|| rfc.to_string())
}

/// The runway as words: `~65h`, `not burning`, `no billing pods`, `postpaid`, `?`.
fn runway_label(r: &Row) -> String {
    match r.runway {
        Runway::Hours { hours } => {
            let bound = if r.runway_upper_bound { "≤" } else { "" };
            format!("{bound}{}", fmt_runway_hours(hours))
        }
        Runway::NotBurning => "not burning".into(),
        Runway::NothingBilling => "no billing pods".into(),
        Runway::Postpaid => "postpaid".into(),
        Runway::Unknown => "?".into(),
    }
}

fn money(r: &Row, v: f64) -> String {
    fmt_money(r.currency, v)
}

/// The BALANCE cell: `$32.54`, `postpaid`, `?`.
fn balance_label(r: &Row) -> String {
    match (r.billing, r.balance) {
        ("postpaid", _) => "postpaid".into(),
        (_, Some(b)) => money(r, b),
        _ => "?".into(),
    }
}

/// The FLEET cell: `$0.25/h`, `$0.25/h +2 unpriced`, `?` (listing failed).
fn fleet_label(r: &Row) -> String {
    match r.fleet {
        None => "?".into(),
        Some(b) if b.unpriced > 0 => format!("{}/h +{} unpriced", money(r, b.per_hr), b.unpriced),
        Some(b) => format!("{}/h", money(r, b.per_hr)),
    }
}

/// The ⚠ lines: one per row that needs a top-up, saying why and by when.
pub fn warn_lines(report: &Report) -> Vec<String> {
    let mut out = Vec::new();
    for r in report.rows.iter().filter(|r| r.warn) {
        let left = r.balance.map(|b| money(r, b)).unwrap_or_else(|| "?".into());
        let mut why = Vec::new();
        if r.balance.is_some_and(|b| b <= 0.0) {
            why.push(format!("{left} left — a prepaid provider stops pods at zero"));
        } else if let Runway::Hours { hours } = r.runway {
            let at = r.runs_out_at.as_deref().map(|t| format!(" (≈ {} UTC)", short_time(t))).unwrap_or_default();
            let burn = r.burn_per_hr.map(|b| format!(" at {}/h", money(r, b))).unwrap_or_default();
            why.push(format!(
                "{left} left{burn} — runs out in {}{at}, under BALANCE_WARN_HOURS={}",
                fmt_runway_hours(hours),
                report.warn_hours
            ));
        }
        if r.under_balance == Some(true) {
            why.push("RunPod flags the account as under balance".into());
        }
        out.push(format!("⚠ {}: {} — top up (or set up auto-pay) before it stops every pod", r.provider, why.join("; ")));
    }
    out
}

/// Notes under the table: unreadable balances, spend limits, a Vast negative balance.
fn notes(report: &Report) -> Vec<String> {
    let mut out = Vec::new();
    for r in &report.rows {
        if let Some(e) = &r.error {
            out.push(format!("{}: balance unavailable — {e}", r.provider));
        }
        if let Some(l) = r.spend_limit_per_hr {
            out.push(format!("{}: account spend limit {}/h", r.provider, money(r, l)));
        }
        if let Some(o) = r.owed {
            out.push(format!(
                "{}: also reports balance −{} — possibly charges not yet taken from credit; check the console (not in the runway)",
                r.provider,
                money(r, o)
            ));
        }
        if r.runway_upper_bound {
            out.push(format!("{}: some billing pods have no price — the runway may be shorter", r.provider));
        }
        if r.billing == "postpaid" {
            out.push(format!("{}: postpaid — billed monthly in arrears, no balance to run out", r.provider));
        }
    }
    out
}

/// `arena balance`: the table, notes, then the ⚠ lines.
pub fn render(report: &Report) -> String {
    if report.rows.is_empty() {
        return "(no provider keys configured — nothing to check)\n".into();
    }
    let headers = ["PROVIDER", "BALANCE", "PROVIDER RATE", "FLEET", "RUNWAY", "RUNS OUT (UTC)"];
    let align = [Align::Left, Align::Right, Align::Right, Align::Right, Align::Left, Align::Left];
    let rows: Vec<Vec<String>> = report
        .rows
        .iter()
        .map(|r| {
            let warn = if r.warn { "⚠ " } else { "" };
            vec![
                r.provider.clone(),
                balance_label(r),
                r.provider_per_hr.map(|p| format!("{}/h", money(r, p))).unwrap_or_else(|| "-".into()),
                fleet_label(r),
                format!("{warn}{}", runway_label(r)),
                r.runs_out_at.as_deref().map(short_time).unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut out = table::render(&headers, &align, &rows);
    out.push_str(
        "burn = the higher of the provider's own rate and this fleet's billing pods there; runway = balance ÷ burn\n",
    );
    for n in notes(report) {
        out.push_str(&format!("  {n}\n"));
    }
    for w in warn_lines(report) {
        out.push_str(&w);
        out.push('\n');
    }
    out
}

/// One line for a footer or the TUI's summary bar — `balance: runpod $32.54 ~65h · vast
/// $136.87 no billing pods · hetzner postpaid`, a ⚠ before a row that needs a top-up — and
/// whether any row does. `None` when no provider is configured.
pub fn compact(report: &Report) -> Option<(String, bool)> {
    if report.rows.is_empty() {
        return None;
    }
    let parts: Vec<String> = report
        .rows
        .iter()
        .map(|r| {
            let warn = if r.warn { "⚠ " } else { "" };
            match (r.billing, r.balance) {
                ("postpaid", _) => format!("{} postpaid", r.provider),
                (_, Some(b)) => format!("{warn}{} {} {}", r.provider, money(r, b), runway_label(r)),
                _ => format!("{} ?", r.provider),
            }
        })
        .collect();
    Some((format!("balance: {}", parts.join(" · ")), report.warnings > 0))
}

/// For `teardown --check` (informational, never part of its verdict): what's left on each
/// account and what the provider itself still reports spending — at the end of a program a
/// RunPod rate above zero means something on the account still bills.
pub fn teardown_lines(report: &Report) -> Vec<String> {
    if report.rows.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["account balances (informational — not part of the check):".to_string()];
    for r in &report.rows {
        let line = match (&r.error, r.billing, r.balance) {
            (Some(e), _, _) => format!("? {}: balance unavailable — {e}", r.provider),
            (_, "postpaid", _) => format!("– {}: postpaid — billed monthly in arrears", r.provider),
            (_, _, Some(b)) => {
                let spend = match r.provider_per_hr {
                    Some(p) if p > 0.0 => format!(
                        "; {} still reports {}/h of spend — something on the account bills",
                        r.provider,
                        money(r, p)
                    ),
                    Some(_) => format!("; {} reports no current spend", r.provider),
                    None => String::new(),
                };
                format!("{} {}: {} left{spend}", if r.warn { "⚠" } else { "·" }, r.provider, money(r, b))
            }
            _ => format!("? {}: balance unavailable", r.provider),
        };
        out.push(format!("  {line}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_server::{canned, client, serve, stall};

    /// Recorded shape (live 2026-10-08, values as observed): RunPod's GraphQL answer.
    fn runpod_body(balance: Value, spend: Value) -> Value {
        json!({ "data": { "myself": {
            "clientBalance": balance, "currentSpendPerHr": spend, "spendLimit": 80, "underBalance": false
        } } })
    }

    /// Vast's `/users/current/` shape (the vast CLI's user fields): it carries the email and
    /// the API key, which must never surface anywhere.
    fn vast_body(credit: Value, balance: Value) -> Value {
        json!({
            "id": 424242, "email": "someone@example.org", "api_key": "VASTSECRETKEY123", "sid": "s-1",
            "credit": credit, "balance": balance, "balance_threshold": 0, "billed_expected": 0,
            "ssh_key": "ssh-ed25519 AAAA operator", "username": "someone"
        })
    }

    fn probe(provider: &str, account: std::result::Result<Account, &str>) -> AccountProbe {
        AccountProbe { provider: provider.into(), account: account.map_err(String::from) }
    }

    fn prepaid(balance: f64, rate: Option<f64>) -> Account {
        Account::Prepaid { balance, provider_per_hr: rate, spend_limit_per_hr: None, under_balance: None, owed: None }
    }

    fn pod(provider: &str, status: &str, cost: Option<f64>) -> Pod {
        Pod { provider: provider.into(), status: status.into(), cost_per_hr: cost, ..Default::default() }
    }

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z

    #[test]
    fn runpod_parse_reads_the_four_fields_and_fails_closed() {
        let a = parse_runpod(&runpod_body(json!(32.54), json!(0))).unwrap();
        assert_eq!(
            a,
            Account::Prepaid {
                balance: 32.54,
                provider_per_hr: Some(0.0),
                spend_limit_per_hr: Some(80.0),
                under_balance: Some(false),
                owed: None
            }
        );
        // Numeric strings are numbers; a missing rate is unknown, not zero.
        let v = json!({ "data": { "myself": { "clientBalance": "12.5", "underBalance": true } } });
        assert_eq!(
            parse_runpod(&v).unwrap(),
            Account::Prepaid { balance: 12.5, provider_per_hr: None, spend_limit_per_hr: None, under_balance: Some(true), owed: None }
        );
        // No balance → an error; a GraphQL error is quoted, a body never.
        let cases = [
            (json!({ "data": { "myself": null } }), "no numeric data.myself.clientBalance"),
            (json!({ "data": { "myself": { "clientBalance": null } } }), "no numeric"),
            (json!({ "data": { "myself": { "clientBalance": "lots" } } }), "no numeric"),
            (json!({ "errors": [{ "message": "Cannot query field \"clientBalance\"" }], "data": null }), "Cannot query field"),
            (json!([]), "no numeric"),
        ];
        for (v, want) in cases {
            let e = parse_runpod(&v).unwrap_err().to_string();
            assert!(e.contains(want), "{v}: {e}");
        }
        // A balance with an error about another field: the balance still counts.
        let partial = json!({ "data": { "myself": { "clientBalance": 5 } }, "errors": [{ "message": "spendLimit: denied" }] });
        assert!(matches!(parse_runpod(&partial).unwrap(), Account::Prepaid { balance, .. } if balance == 5.0));
    }

    #[test]
    fn vast_parse_reads_credit_only_and_never_echoes_the_account() {
        assert_eq!(parse_vast(&vast_body(json!(136.87), json!(0))).unwrap(), prepaid(136.87, None));
        let owed = parse_vast(&vast_body(json!(10), json!(-4.1))).unwrap();
        assert!(matches!(owed, Account::Prepaid { balance, owed: Some(o), .. } if balance == 10.0 && (o - 4.1).abs() < 1e-9));
        for bad in [vast_body(json!(null), json!(5)), json!({ "user": vast_body(json!(1), json!(0)) }), json!("nope")] {
            let e = parse_vast(&bad).unwrap_err().to_string();
            assert!(e.contains("no numeric `credit`"), "{e}");
            for secret in ["VASTSECRETKEY123", "someone@example.org", "424242"] {
                assert!(!e.contains(secret), "{secret} leaked: {e}");
            }
        }
        // Nothing the report prints carries the account's identity either.
        let r = report(&[probe("vast", Ok(parse_vast(&vast_body(json!(136.87), json!(-2))).unwrap()))], &burns(&[], &[]), 48.0, NOW);
        let all = format!("{}{}{:?}", render(&r), serde_json::to_string(&r).unwrap(), compact(&r));
        for secret in ["VASTSECRETKEY123", "someone@example.org", "424242", "operator"] {
            assert!(!all.contains(secret), "{secret} leaked: {all}");
        }
    }

    /// The real request/response path over a loopback socket: bearer auth (the key never in
    /// the URL or the error), status first, a stall bounded.
    #[tokio::test]
    async fn fetches_go_status_first_and_keep_the_key_out_of_errors() {
        let srv = serve(vec![
            canned(200, "application/json", &runpod_body(json!(32.54), json!(0.25)).to_string()),
            canned(401, "text/html", "<html>401 Authorization Required</html>"),
            canned(200, "application/json", &vast_body(json!(136.87), json!(0)).to_string()),
            canned(429, "application/json", r#"{"error":"slow down"}"#),
            canned(200, "text/html", "<html>maintenance</html>"),
            stall(),
        ]);
        let c = client();
        let gql = format!("{}/graphql", srv.base);
        let a = fetch_runpod_at(&c, &gql, "rpa_SECRET").await.unwrap();
        assert!(matches!(a, Account::Prepaid { balance, provider_per_hr: Some(r), .. } if balance == 32.54 && r == 0.25));
        let e = fetch_runpod_at(&c, &gql, "rpa_SECRET").await.unwrap_err();
        assert_eq!(e.kind(), Some(crate::ProviderErrorKind::Auth), "{e}");
        assert!(e.to_string().contains("runpod balance HTTP 401") && !e.to_string().contains("rpa_SECRET"), "{e}");
        assert_eq!(fetch_vast_at(&c, &srv.base, "VK").await.unwrap(), prepaid(136.87, None));
        let e = fetch_vast_at(&c, &format!("{}/", srv.base), "VK").await.unwrap_err();
        assert_eq!(e.kind(), Some(crate::ProviderErrorKind::RateLimited), "{e}");
        let e = fetch_vast_at(&c, &srv.base, "VK").await.unwrap_err().to_string();
        assert!(e.contains("isn't JSON") && e.contains("maintenance"), "{e}");
        let e = bounded("vast", Duration::from_millis(200), fetch_vast_at(&c, &srv.base, "VK")).await.unwrap_err();
        assert!(e.to_string().contains("vast balance timed out after 0.2s"), "{e}");
        let reqs = srv.requests.lock().unwrap().clone();
        assert_eq!(reqs[0], "POST /graphql HTTP/1.1");
        assert_eq!(reqs[2], "GET /users/current/ HTTP/1.1");
        assert_eq!(reqs[3], "GET /users/current/ HTTP/1.1", "a trailing slash on the base isn't doubled");
        assert!(reqs.iter().all(|r| !r.contains("SECRET") && !r.contains("VK")), "{reqs:?}");
        let bodies = srv.bodies.lock().unwrap().clone();
        assert_eq!(serde_json::from_str::<Value>(&bodies[0]).unwrap(), json!({ "query": RUNPOD_QUERY }));
    }

    /// No key, no read: an empty config reads nothing (and makes no request).
    #[tokio::test]
    async fn fetch_all_reads_only_configured_providers() {
        assert!(fetch_all(&Config::parse(""), Duration::from_millis(10)).await.is_empty());
        let h = fetch_all(&Config::parse("HETZNER_API_KEY=h\nRUNPOD_API_KEY=\"\"\n"), Duration::from_millis(10)).await;
        assert_eq!(h, vec![probe("hetzner", Ok(Account::Postpaid))]);
    }

    #[test]
    fn warn_hours_parses_or_refuses() {
        let w = |t: &str| warn_hours(&Config::parse(t));
        assert_eq!(w(""), Ok(48.0));
        assert_eq!(w("BALANCE_WARN_HOURS=\"\""), Ok(48.0));
        assert_eq!(w("BALANCE_WARN_HOURS=12"), Ok(12.0));
        assert_eq!(w("BALANCE_WARN_HOURS=0"), Ok(0.0));
        assert_eq!(w("BALANCE_WARN_HOURS=1.5"), Ok(1.5));
        for bad in ["-1", "two days", "inf", "NaN"] {
            let e = w(&format!("BALANCE_WARN_HOURS=\"{bad}\"")).unwrap_err();
            assert!(e.starts_with("BALANCE_WARN_HOURS must be"), "{bad}: {e}");
        }
        assert!(w("BALANCE_WARN_HOURS=rpa_AAAAAAAAAAAAAAAA").unwrap_err().contains("a long value"));
        // The lenient form the best-effort surfaces use: the default, and a line saying so.
        assert_eq!(warn_hours_or_default(&Config::parse("BALANCE_WARN_HOURS=12")), (12.0, None));
        assert_eq!(
            warn_hours_or_default(&Config::parse("BALANCE_WARN_HOURS=soon")),
            (48.0, Some("warning: BALANCE_WARN_HOURS must be a number of hours ≥ 0 (got `soon`) — using 48".into()))
        );
    }

    #[test]
    fn burns_count_billing_pods_per_provider_and_failed_listings_are_unknown() {
        let pods = [
            pod("runpod", "RUNNING", Some(0.25)),
            pod("runpod", "EXITED", Some(0.30)), // not billing
            pod("runpod", "PROVISIONING", None), // billing, unpriced
            pod("vast", "running", Some(0.40)),
            pod("hetzner", "off", Some(0.006)), // Hetzner bills while it exists
        ];
        let b = burns(&pods, &["vast".to_string()]);
        assert_eq!(b.of("runpod"), Some(Burn { per_hr: 0.25, billing: 2, unpriced: 1 }));
        assert_eq!(b.of("vast"), None, "its listing failed: unknown, not zero");
        assert_eq!(b.of("hetzner"), Some(Burn { per_hr: 0.006, billing: 1, unpriced: 0 }));
        assert_eq!(b.of("lambda"), Some(Burn::default()), "listed fine with no pods");
        assert_eq!(Burns::unknown().of("runpod"), None);
    }

    /// The runway table: (case, account, fleet burn, runway, warn).
    #[test]
    fn runway_is_balance_over_the_higher_burn() {
        let b = |per_hr: f64, billing: usize, unpriced: usize| Some(Burn { per_hr, billing, unpriced });
        let hours = |h: f64| Runway::Hours { hours: h };
        let cases: Vec<(&str, std::result::Result<Account, &str>, Option<Burn>, Runway, bool)> = vec![
            ("fleet burn wins", Ok(prepaid(30.0, Some(0.0))), b(0.5, 2, 0), hours(60.0), false),
            ("provider rate wins", Ok(prepaid(30.0, Some(1.0))), b(0.5, 2, 0), hours(30.0), true),
            ("unknown fleet: provider rate", Ok(prepaid(96.0, Some(1.0))), None, hours(96.0), false),
            ("nothing anywhere", Ok(prepaid(30.0, Some(0.0))), b(0.0, 0, 0), Runway::NotBurning, false),
            ("provider says 0, fleet unknown", Ok(prepaid(30.0, Some(0.0))), None, Runway::NotBurning, false),
            ("vast: no rate, no pods", Ok(prepaid(136.87, None)), b(0.0, 0, 0), Runway::NothingBilling, false),
            ("vast: no rate, listing failed", Ok(prepaid(136.87, None)), None, Runway::Unknown, false),
            ("vast: only unpriced pods", Ok(prepaid(136.87, None)), b(0.0, 1, 1), Runway::Unknown, false),
            ("zero balance warns even idle", Ok(prepaid(0.0, Some(0.0))), b(0.0, 0, 0), Runway::NotBurning, true),
            ("negative balance", Ok(prepaid(-3.0, Some(0.5))), None, hours(0.0), true),
            ("just over the line", Ok(prepaid(48.5, Some(1.0))), None, hours(48.5), false),
            ("postpaid", Ok(Account::Postpaid), b(0.006, 1, 0), Runway::Postpaid, false),
            ("unreadable", Err("runpod balance HTTP 401"), b(0.5, 1, 0), Runway::Unknown, false),
        ];
        for (case, account, fleet, want, warn) in cases {
            let r = row(&probe("runpod", account), fleet, 48.0, NOW);
            assert_eq!(r.runway, want, "{case}");
            assert_eq!(r.warn, warn, "{case}");
        }
        let flagged = Account::Prepaid { balance: 500.0, provider_per_hr: Some(0.1), spend_limit_per_hr: None, under_balance: Some(true), owed: None };
        assert!(row(&probe("runpod", Ok(flagged)), None, 48.0, NOW).warn, "underBalance warns whatever the runway");
        // The date: 60h after NOW; none past a year.
        let r = row(&probe("runpod", Ok(prepaid(30.0, Some(0.0)))), b(0.5, 2, 0), 48.0, NOW);
        assert_eq!(r.runs_out_at.as_deref(), Some("2026-10-11T00:00:00Z"));
        assert_eq!(row(&probe("runpod", Ok(prepaid(1e6, Some(0.01)))), None, 48.0, NOW).runs_out_at, None);
        // Unpriced pods with no provider rate: the runway is an upper bound.
        let r = row(&probe("vast", Ok(prepaid(10.0, None))), b(1.0, 2, 1), 48.0, NOW);
        assert!(r.runway_upper_bound && runway_label(&r) == "≤~10h", "{r:?}");
    }

    #[test]
    fn runway_reads_in_minutes_hours_or_days() {
        for (h, want) in [(0.5, "~30m"), (1.0, "~1h"), (65.9, "~65h"), (72.0, "~3.0d"), (130.0, "~5.4d"), (600.0, "~25d"), (9000.0, "over a year")] {
            assert_eq!(fmt_runway_hours(h), want, "{h}");
        }
    }

    #[test]
    fn report_renders_a_table_notes_and_warnings() {
        let probes = vec![
            probe("runpod", Ok(Account::Prepaid { balance: 5.0, provider_per_hr: Some(0.0), spend_limit_per_hr: Some(80.0), under_balance: None, owed: None })),
            probe("vast", Ok(Account::Prepaid { balance: 136.87, provider_per_hr: None, spend_limit_per_hr: None, under_balance: None, owed: Some(2.5) })),
            probe("hetzner", Ok(Account::Postpaid)),
        ];
        let pods = [pod("runpod", "RUNNING", Some(0.5)), pod("hetzner", "running", Some(0.006))];
        let r = report(&probes, &burns(&pods, &[]), 48.0, NOW);
        assert_eq!((r.warnings, r.unknown), (1, 0));
        let text = render(&r);
        for needle in [
            "PROVIDER   BALANCE  PROVIDER RATE     FLEET  RUNWAY           RUNS OUT (UTC)",
            "runpod       $5.00        $0.00/h   $0.50/h  ⚠ ~10h           2026-10-08 22:00",
            "vast       $136.87              -   $0.00/h  no billing pods  -",
            "hetzner   postpaid              -  €0.006/h  postpaid         -",
            "runpod: account spend limit $80.00/h",
            "vast: also reports balance −$2.50",
            "hetzner: postpaid — billed monthly in arrears",
            "⚠ runpod: $5.00 left at $0.50/h — runs out in ~10h (≈ 2026-10-08 22:00 UTC), under BALANCE_WARN_HOURS=48 — top up",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
        let (line, warn) = compact(&r).unwrap();
        assert_eq!(line, "balance: ⚠ runpod $5.00 ~10h · vast $136.87 no billing pods · hetzner postpaid");
        assert!(warn);
        // JSON: the same rows, machine-readable.
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["rows"][0]["runway"], json!({ "state": "hours", "hours": 10.0 }));
        assert_eq!(v["rows"][0]["runs_out_at"], "2026-10-08T22:00:00Z");
        assert_eq!(v["rows"][2]["billing"], "postpaid");
        // Nothing configured.
        let empty = report(&[], &burns(&[], &[]), 48.0, NOW);
        assert_eq!(compact(&empty), None);
        assert!(render(&empty).contains("no provider keys configured"));
    }

    #[test]
    fn an_unreadable_balance_is_a_question_mark_never_zero() {
        let probes = vec![probe("runpod", Err("runpod balance: Cannot query field \"clientBalance\""))];
        let r = report(&probes, &burns(&[], &[]), 48.0, NOW);
        assert_eq!((r.warnings, r.unknown), (0, 1));
        assert_eq!(compact(&r).unwrap(), ("balance: runpod ?".to_string(), false));
        assert!(render(&r).contains("runpod: balance unavailable — runpod balance: Cannot query field"), "{}", render(&r));
        assert_eq!(teardown_lines(&r)[1], "  ? runpod: balance unavailable — runpod balance: Cannot query field \"clientBalance\"");
    }

    #[test]
    fn teardown_lines_say_what_still_spends() {
        let probes = vec![
            probe("runpod", Ok(prepaid(32.54, Some(0.12)))),
            probe("vast", Ok(prepaid(136.87, None))),
            probe("hetzner", Ok(Account::Postpaid)),
        ];
        let lines = teardown_lines(&report(&probes, &Burns::unknown(), 48.0, NOW));
        assert_eq!(
            lines,
            [
                "account balances (informational — not part of the check):",
                "  · runpod: $32.54 left; runpod still reports $0.12/h of spend — something on the account bills",
                "  · vast: $136.87 left",
                "  – hetzner: postpaid — billed monthly in arrears",
            ]
        );
        let idle = teardown_lines(&report(&[probe("runpod", Ok(prepaid(32.54, Some(0.0))))], &Burns::unknown(), 48.0, NOW));
        assert_eq!(idle[1], "  · runpod: $32.54 left; runpod reports no current spend");
        assert!(teardown_lines(&report(&[], &Burns::unknown(), 48.0, NOW)).is_empty());
    }
}
