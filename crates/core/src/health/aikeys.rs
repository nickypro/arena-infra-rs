//! The AI keys' half of the deep check (and all of `pods test --keys`): does the machine's
//! OpenRouter key work, and is it the one the keys CSV has for that machine? The same for the
//! Hugging Face token every machine gets.
//!
//! **Why.** Nothing checked a machine's AI key. A key deleted, paused or out of money on
//! OpenRouter, a machine that never got its key, a machine still holding an old key after a
//! rotate or a handover: each passed every check, and the participant found out from their first
//! failing call. Now the check script asks each service about the key the participant's login
//! shell exports (once, and free: OpenRouter's `GET /api/v1/key`, Hugging Face's `whoami-v2`) and
//! reports what it answered with the key's fingerprint, never the key ([`KeyFacts`]). Here those
//! facts are judged against what the control box knows ([`KeyBook`]: the keys CSV by
//! fingerprint, the config's `HF_TOKEN`, who owns the machine), into an `ai_key` and an
//! `hf_token` check ([`key_checks`]), each saying what to run.
//!
//! **A key is not the machine.** A key FAIL is [`super::FailureKind::Key`]: `pods test --deep`
//! still exits non-zero on it (a script notices), but `up --check` never replaces a machine for
//! it, never retries it and never remembers its host as bad; and a check whose only FAILs are
//! keys clears a bad-host record like a pass ([`super::PodHealth::machine_status`]).
//!
//! **Unknown is never a FAIL.** Facts from a script older than these checks, or cut off before
//! them, read as not reported (Skip). A staff `@` box (its key is the organisers' choice) and a
//! fleet with no keys configured get a Skip line saying what was found, never a verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use super::{Check, DeepFacts, PodHealth, Status};
use crate::config::{Config, KEYS_DIR};
use crate::fleet::fmt_money;
use crate::naming::{is_absolute, qualify};
use crate::pod::Pod;

/// The OpenRouter key's check (`--json`, `-v`, the health cache).
pub const AI_KEY: &str = "ai_key";
/// The Hugging Face token's check.
pub const HF_TOKEN_CHECK: &str = "hf_token";

/// Whether the check named `name` is about an AI key or token, not the machine.
pub fn is_key_check(name: &str) -> bool {
    name == AI_KEY || name == HF_TOKEN_CHECK
}

/// One key or token as the machine's login shell exports it, and what its service answered —
/// the script's `openrouter.*` / `hf.*` facts. Never the secret: its [`fingerprint`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct KeyFacts {
    /// Set (non-empty) in the login shell: `Some(false)` = missing; `None` = not said.
    pub set: Option<bool>,
    /// The first 12 hex of its SHA-256 ([`fingerprint`]); `None` with no hasher on the machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// curl's exit status (`missing`: no curl on the machine).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curl_exit: Option<String>,
    /// The HTTP status the service answered with (`None`/`0`: no answer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    /// curl's error, or the answer's own `message`/`error` (a non-2xx).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// OpenRouter: the key's cap in USD (`None` with `unlimited`: it has none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unlimited: bool,
    /// What's left of the cap in its current reset period, as OpenRouter counts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_remaining: Option<f64>,
    /// `daily`, `weekly`, `monthly`: when the cap starts over (`None`: never).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_reset: Option<String>,
    /// Spent over its whole life, USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<f64>,
    /// Whether OpenRouter says it's disabled (paused), where it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

impl KeyFacts {
    /// The service answered 2xx.
    fn accepted(&self) -> bool {
        self.http.is_some_and(|h| (200..300).contains(&h))
    }

    /// The service turned the key away (401/403).
    fn rejected(&self) -> bool {
        matches!(self.http, Some(401 | 403))
    }

    /// What was found, in a few words, for a line that judges nothing (a staff box, a fleet
    /// with no keys): `none here`, `one here, accepted`, `one here, rejected (HTTP 401)`.
    fn seen(&self) -> String {
        match self.set {
            Some(false) => "none here".into(),
            _ if self.accepted() => "one here, accepted".into(),
            _ if self.rejected() => format!("one here, rejected (HTTP {})", self.http.unwrap_or_default()),
            _ => "one here, not checked".into(),
        }
    }

    /// Why the service couldn't be asked: curl's error, else its exit status.
    fn unreached(&self) -> String {
        self.error
            .clone()
            .or_else(|| self.curl_exit.as_ref().filter(|c| *c != "0").map(|c| format!("curl exit {c}")))
            .unwrap_or_else(|| "no answer".into())
    }
}

/// The first 12 hex digits of a key's SHA-256, as the check script prints it: what the keys
/// CSV's key is compared by without the secret ever leaving the machine.
pub fn fingerprint(secret: &str) -> String {
    crate::claude::fingerprint(secret)
}

/// One `openrouter.<field>` / `hf.<field>` fact into `f` (`false`: not one of these keys).
/// `unknown` or an unparseable value leaves a field unknown.
pub(super) fn key_fact(f: &mut DeepFacts, key: &str, value: &str) -> bool {
    let (slot, field) = match key.split_once('.') {
        Some(("openrouter", field)) => (&mut f.openrouter, field),
        Some(("hf", field)) => (&mut f.hf, field),
        _ => return false,
    };
    const FIELDS: &[&str] =
        &["key", "fingerprint", "curl_exit", "http", "error", "limit", "limit_remaining", "limit_reset", "usage", "disabled"];
    if !FIELDS.contains(&field) {
        return false;
    }
    let k = slot.get_or_insert_with(KeyFacts::default);
    let text = || (!value.is_empty() && value != "unknown").then(|| value.to_string());
    let num = || value.parse::<f64>().ok().filter(|v| v.is_finite());
    match field {
        "key" => {
            k.set = match value {
                "set" => Some(true),
                "missing" => Some(false),
                _ => None,
            }
        }
        "fingerprint" => {
            k.fingerprint = Some(value.to_ascii_lowercase()).filter(|v| v.len() == 12 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        "curl_exit" => k.curl_exit = text(),
        "http" => k.http = value.parse().ok(),
        "error" => k.error = text(),
        "limit" => {
            k.unlimited = value == "none";
            k.limit = num();
        }
        "limit_remaining" => k.limit_remaining = num(),
        "limit_reset" => k.limit_reset = text().filter(|v| v != "none"),
        "usage" => k.usage = num(),
        _ => {
            k.disabled = match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
        }
    }
    true
}

/// What a machine's keys are held to: the fleet's keys CSV and config, and who should have one.
/// Fingerprints only — the secrets aren't kept. `Default`: a fleet with no keys configured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyBook {
    /// `MACHINE_NAME_PREFIX`: the commands a verdict names take the machine without it (`apple`).
    pub prefix: String,
    /// The fleet hands out OpenRouter keys: its keys CSV has rows, or a provisioning key is set.
    pub openrouter: bool,
    /// Machine name → the fingerprint of the OpenRouter key its keys-CSV row holds.
    pub csv: BTreeMap<String, String>,
    /// Machine instance (`provider:id`, [`crate::snapshot::cache_key`]) → its owner in the spend
    /// ledger (`pods up --owner`, `spend assign`): someone who should have a key.
    pub owners: BTreeMap<String, String>,
    /// The fingerprint of the Hugging Face token every machine gets (`HF_TOKEN`), when set.
    pub hf: Option<String>,
    /// A machine's own row in `huggingface_api_keys.csv`, which `copy-keys` writes after the
    /// broadcast token (so it wins in the shell): machine name → fingerprint.
    pub hf_csv: BTreeMap<String, String>,
    /// Staff `@` boxes, by full name: their keys are the organisers' choice.
    pub staff: BTreeSet<String>,
}

impl KeyBook {
    /// From the secrets themselves, kept only as fingerprints: `openrouter` and `hf_rows` are the
    /// keys CSVs' `(machine, secret)` rows, `hf` the token every machine gets (`HF_TOKEN`). No
    /// owners: [`Self::with_owners`].
    pub fn new(cfg: &Config, openrouter: &[(String, String)], hf: Option<&str>, hf_rows: &[(String, String)]) -> Self {
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
        let provisioning = cfg.get("OPENROUTER_PROVISIONING_KEY").is_some_and(|k| !k.trim().is_empty());
        Self {
            openrouter: !openrouter.is_empty() || provisioning,
            csv: openrouter.iter().map(|(h, k)| (h.clone(), fingerprint(k))).collect(),
            owners: BTreeMap::new(),
            hf: hf.map(str::trim).filter(|t| !t.is_empty()).map(fingerprint),
            hf_csv: hf_rows.iter().map(|(h, k)| (h.clone(), fingerprint(k))).collect(),
            staff: cfg.machine_names.iter().filter(|e| is_absolute(e)).map(|e| qualify(&prefix, e)).collect(),
            prefix,
        }
    }

    /// The fleet's: `openrouter_api_keys.csv` and `huggingface_api_keys.csv` in its keys folder
    /// (beside the config: [`Config::fleet_file`], as `copy-keys` reads them), the config's
    /// `HF_TOKEN`, and the owners in the spend ledger at `ledger` (none without one). A file that
    /// can't be read holds no rows.
    pub fn load(cfg: &Config, ledger: Option<&Path>) -> Self {
        let dir = cfg.fleet_file(KEYS_DIR).path;
        let rows = |base: &str| {
            std::fs::read_to_string(dir.join(format!("{base}_api_keys.csv")))
                .map(|t| crate::apikeys::parse_csv(&t))
                .unwrap_or_default()
        };
        Self::new(cfg, &rows("openrouter"), cfg.get("HF_TOKEN"), &rows("huggingface")).with_owners(ledger)
    }

    /// With the owners in the spend ledger at `ledger` ([`ledger_owners`]).
    pub fn with_owners(mut self, ledger: Option<&Path>) -> Self {
        self.owners = ledger.map(ledger_owners).unwrap_or_default();
        self
    }

    /// Whether there's anything to hold a machine's keys to: OpenRouter keys or an HF token.
    pub fn any(&self) -> bool {
        self.openrouter || self.hf.is_some() || !self.hf_csv.is_empty()
    }

    /// `apple` for `devtest-apple`: the machine as commands take it.
    fn short<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix(self.prefix.as_str()).and_then(|s| s.strip_prefix('-')).filter(|s| !s.is_empty()).unwrap_or(name)
    }
}

/// Every machine instance's owner in the spend ledger at `path` (`provider:id` → owner, the open
/// row's as [`crate::spend::owner_of`] has it). An unreadable ledger has none.
pub fn ledger_owners(path: &Path) -> BTreeMap<String, String> {
    let state = crate::spend::load(path).0;
    state
        .machines
        .iter()
        .filter_map(|m| {
            crate::spend::owner_of(&state, &m.provider, &m.id).map(|o| (crate::snapshot::cache_key(&m.provider, &m.id), o.to_string()))
        })
        .collect()
}

/// The machine's AI-key checks: its OpenRouter key (`ai_key`) and its Hugging Face token
/// (`hf_token`), from the facts the script reported, held to `book`. Pure.
pub fn key_checks(f: &DeepFacts, pod: &Pod, book: &KeyBook) -> Vec<Check> {
    vec![ai_key_check(f.openrouter.as_ref(), pod, book), hf_token_check(f.hf.as_ref(), pod, book)]
}

/// Facts from a script that didn't say (older than these checks, or cut off before them).
const NOT_REPORTED: &str = "not reported (a check from before key checks, or cut off)";

/// What "little left" means: under a tenth of the key's cap, and never more than $1 — a tenth
/// leaves the organisers a day or so to raise it before calls start failing (a participant's
/// day of AI exercises spends cents to a few dollars), and the $1 ceiling keeps a large cap
/// from warning while it still has plenty.
pub fn low_line(limit: f64) -> f64 {
    (limit * 0.1).min(1.0)
}

/// `$0.42 of $1.00 left`, `$0.000 of $0.05 left (resets daily)`, `no cap, $0.12 spent` — or
/// nothing when OpenRouter said neither.
fn money(k: &KeyFacts) -> String {
    let reset = k.limit_reset.as_deref().map(|r| format!(" (resets {r})")).unwrap_or_default();
    match (k.limit, k.limit_remaining) {
        (Some(l), Some(r)) => format!("{} of {} left{reset}", fmt_money("$", r), fmt_money("$", l)),
        (Some(l), None) => format!("cap {}{reset}", fmt_money("$", l)),
        _ if k.unlimited => match k.usage {
            Some(u) => format!("no cap, {} spent", fmt_money("$", u)),
            None => "no cap".into(),
        },
        _ => String::new(),
    }
}

/// curl's error or the service's message, cut for a NOTES cell.
fn short_why(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 48 {
        format!("{}…", s.chars().take(47).collect::<String>())
    } else {
        s
    }
}

// The details are kept short — the table's NOTES cell is clipped — with the command to run
// last; `-v` and `--json` say the same, the facts in full.
fn ai_key_check(k: Option<&KeyFacts>, pod: &Pod, book: &KeyBook) -> Check {
    use Status::*;
    let name = AI_KEY;
    let Some(k) = k.filter(|k| k.set.is_some()) else {
        return Check::new(name, Skip, NOT_REPORTED);
    };
    let s = book.short(&pod.name);
    let row = book.csv.get(&pod.name);
    let owner = book.owners.get(&crate::snapshot::cache_key(&pod.provider, &pod.id));
    if book.staff.contains(&pod.name) {
        return Check::new(name, Skip, format!("staff box: its key is the organisers' choice ({})", k.seen()));
    }
    if !book.openrouter && row.is_none() && owner.is_none() {
        return Check::new(name, Skip, format!("no OpenRouter keys configured for this fleet ({})", k.seen()));
    }
    if k.set == Some(false) {
        return match (row, owner) {
            (Some(_), _) => {
                Check::new(name, Fail, format!("missing, though the keys CSV has one for it — `arena pods copy-keys {s}`"))
            }
            (None, Some(o)) => {
                Check::new(name, Fail, format!("missing, and its owner ({o}) should have one — `arena keys gen {s} --copy`"))
            }
            (None, None) => Check::new(
                name,
                Warn,
                format!("missing, nor in the keys CSV — `arena keys gen {s} --copy` if it should have one"),
            ),
        };
    }
    // Not the key the keys CSV has for it: an old one, from before a rotate or a handover. The
    // CSV's is then the one to put on — whatever else is wrong with the old one.
    let stale = matches!((row, k.fingerprint.as_deref()), (Some(want), Some(got)) if want != got);
    let copy = format!("`arena pods copy-keys {s}`");
    const OLD: &str = "an old key, not the keys CSV's (rotated or handed over since?)";
    let fix = |own: String| if stale { format!("{OLD} — {copy}") } else { own };
    let unchecked = |what: String| {
        let then = if stale { format!("; {OLD} — {copy}") } else { format!(" — `arena pods test --keys {s}` later") };
        Check::new(name, Warn, format!("not checked: {what}{then}"))
    };
    if k.curl_exit.as_deref() == Some("missing") {
        return unchecked("no curl on the machine".into());
    }
    if k.rejected() {
        let h = k.http.unwrap_or_default();
        let paused = k.error.as_deref().is_some_and(|e| e.to_ascii_lowercase().contains("disabled"));
        return if paused {
            Check::new(name, Fail, format!("paused on OpenRouter (HTTP {h}) — {}", fix(format!("`arena keys resume {s}`"))))
        } else {
            Check::new(name, Fail, format!("rejected by OpenRouter (HTTP {h}) — {}", fix(format!("`arena keys rotate {s} --copy`"))))
        };
    }
    if !k.accepted() {
        return unchecked(match k.http.filter(|&h| h != 0) {
            Some(h) => format!("OpenRouter answered HTTP {h}"),
            None => format!("couldn't reach OpenRouter ({})", short_why(&k.unreached())),
        });
    }
    let money = money(k);
    if k.disabled == Some(true) {
        return Check::new(name, Fail, format!("paused on OpenRouter — {}", fix(format!("`arena keys resume {s}`"))));
    }
    let raise = format!("`arena keys limit {s} +5`");
    if let (Some(_), Some(left)) = (k.limit, k.limit_remaining) {
        if left <= 0.0 {
            return Check::new(name, Fail, format!("out of money ({money}) — {}", fix(raise)));
        }
    }
    let low = matches!((k.limit, k.limit_remaining), (Some(l), Some(r)) if l > 0.0 && r < low_line(l));
    let ok = if money.is_empty() { "ok".to_string() } else { format!("ok, {money}") };
    match (stale, low) {
        (false, false) if row.is_none() => Check::new(name, Pass, format!("{ok} (no keys-CSV row to compare)")),
        (false, false) => Check::new(name, Pass, ok),
        (false, true) => Check::new(name, Warn, format!("only {money} (<10% of its cap) — {raise}")),
        (true, false) => Check::new(name, Warn, format!("works, but {OLD} — {copy}")),
        (true, true) => Check::new(name, Warn, format!("only {money}, and {OLD} — {copy}")),
    }
}

fn hf_token_check(k: Option<&KeyFacts>, pod: &Pod, book: &KeyBook) -> Check {
    use Status::*;
    let name = HF_TOKEN_CHECK;
    let Some(k) = k.filter(|k| k.set.is_some()) else {
        return Check::new(name, Skip, NOT_REPORTED);
    };
    let s = book.short(&pod.name);
    if book.staff.contains(&pod.name) {
        return Check::new(name, Skip, format!("staff box: its token is the organisers' choice ({})", k.seen()));
    }
    // The token it should have: its own row in huggingface_api_keys.csv (copied last, so it wins),
    // else the fleet's HF_TOKEN — and where to put a new one.
    let (want, own_row) = match (book.hf_csv.get(&pod.name), book.hf.as_ref()) {
        (Some(row), _) => (row, true),
        (None, Some(fleet)) => (fleet, false),
        (None, None) => return Check::new(name, Skip, format!("no HF_TOKEN configured for this fleet ({})", k.seen())),
    };
    let copy = format!("`arena pods copy-keys {s}`");
    if k.set == Some(false) {
        return Check::new(name, Fail, format!("missing — {copy}"));
    }
    let stale = k.fingerprint.as_deref().is_some_and(|got| got != want);
    let old = if own_row { "not its huggingface_api_keys.csv row's" } else { "not the config's HF_TOKEN" };
    let new_one = if own_row {
        format!("a new token in its huggingface_api_keys.csv row, then {copy}")
    } else {
        "a new HF_TOKEN in the config, then `arena pods copy-keys`".to_string()
    };
    let unchecked = |what: String| {
        let then = if stale { format!("; {old} — {copy}") } else { format!(" — `arena pods test --keys {s}` later") };
        Check::new(name, Warn, format!("not checked: {what}{then}"))
    };
    if k.curl_exit.as_deref() == Some("missing") {
        return unchecked("no curl on the machine".into());
    }
    if k.rejected() {
        let fix = if stale { format!("{old} — {copy}") } else { new_one };
        return Check::new(name, Fail, format!("rejected by Hugging Face (HTTP {}) — {fix}", k.http.unwrap_or_default()));
    }
    if !k.accepted() {
        return unchecked(match k.http.filter(|&h| h != 0) {
            Some(429) => "Hugging Face is rate-limiting (HTTP 429)".to_string(),
            Some(h) => format!("Hugging Face answered HTTP {h}"),
            None => format!("couldn't reach Hugging Face ({})", short_why(&k.unreached())),
        });
    }
    if stale {
        Check::new(name, Warn, format!("works, but {old} — {copy}"))
    } else {
        Check::new(name, Pass, "accepted by Hugging Face")
    }
}

/// The AI KEY column's cell for `h`: `$0.42 left`, `no cap`, `ok`, `missing`, `rejected`,
/// `paused`, `?` (not checked) — `None` when there's no verdict on its key (not reported, a
/// staff box, a fleet with no keys), so a table of such machines needs no column.
pub fn ai_key_cell(h: &PodHealth) -> Option<String> {
    let c = h.checks.iter().find(|c| c.name == AI_KEY && c.status != Status::Skip)?;
    let k = h.facts.as_ref().and_then(|f| f.openrouter.as_ref());
    let Some(k) = k else { return Some(c.status.label().to_string()) };
    Some(match () {
        _ if k.set == Some(false) => "missing".into(),
        _ if k.rejected() => "rejected".into(),
        _ if !k.accepted() => "?".into(),
        _ if k.disabled == Some(true) => "paused".into(),
        _ => match (k.limit, k.limit_remaining) {
            (Some(_), Some(r)) => format!("{} left", fmt_money("$", r)),
            _ if k.unlimited => "no cap".into(),
            _ => "ok".into(),
        },
    })
}

/// The HF column's cell for `h` (`pods test --keys`): `ok`, `missing`, `rejected`, `?`, `stale`
/// — `None` as [`ai_key_cell`].
pub fn hf_token_cell(h: &PodHealth) -> Option<String> {
    let c = h.checks.iter().find(|c| c.name == HF_TOKEN_CHECK && c.status != Status::Skip)?;
    let Some(k) = h.facts.as_ref().and_then(|f| f.hf.as_ref()) else { return Some(c.status.label().to_string()) };
    Some(match () {
        _ if k.set == Some(false) => "missing".into(),
        _ if k.rejected() => "rejected".into(),
        _ if !k.accepted() => "?".into(),
        _ if c.status == Status::Warn => "stale".into(),
        _ => "ok".into(),
    })
}

#[cfg(test)]
#[path = "aikeys_tests.rs"]
mod tests;
