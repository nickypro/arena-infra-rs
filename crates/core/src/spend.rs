//! What each machine cost and who it was for — `arena spend`, for the organisers.
//!
//! **Per machine instance, never per name.** A row is one instance: provider + provider id.
//! A terminated machine keeps its row; a name created again (`pods up apple` for someone
//! else) is a new row by itself. The **owner** is a separate, optional field of that row:
//! `pods up/create --owner NAME` sets it for the instances it makes, `arena spend assign`
//! sets or fixes it later. Nothing else in arena knows about owners.
//!
//! **Where the numbers come from.** The ledger is a fold over observations: every complete
//! provider listing arena already makes (`pods list`, the dashboard, `teardown --check`,
//! `arena balance`, the snapshot cron line) and the lifecycle commands (a create records its
//! instance with its owner; terminate/stop/start record the change at the moment it is
//! made, so a terminate's end time is exact). Between two observations the cost is a step
//! integral, as balance's metering: each instance carries `$/h` segments (a price change, a
//! stop — a stopped RunPod pod bills its volume disk only, priced as [`crate::balance`]
//! prices it — a start), and a stretch between two segments is charged at the higher of the
//! two ends. Provider facts are used where exact and cheap: Hetzner's and RunPod's creation
//! time for an instance arena didn't make; the create's own answer for one it did. RunPod's
//! REST billing *can* be split per pod (`GET /v2/billing/pods`, hourly, `podId` per record —
//! checked live 2026-10-30), so `arena spend` corrects RunPod rows from it: a settled hour
//! (two hours old) is what RunPod billed for that pod, a younger one the higher of billing
//! and metering (billing lags about an hour).
//!
//! **Estimates are flagged, with the way they err.** A stretch longer than [`GAP`] with no
//! observation (charged at its rate throughout: errs high if it was stopped meanwhile); a
//! machine that vanished between observations (charged to the first listing without it:
//! errs high by at most that gap); one seen first long after its provider says it was made
//! (charged at its first seen rate back to then); no price known (0: errs low); a stopped
//! Vast disk at a storage ceiling (errs high). Hetzner's monthly cap (a server never costs
//! more than its monthly price in a month, about 26 days of hourly price) isn't modelled: a
//! week never reaches it.
//!
//! **An incomplete listing never ends a machine.** Only a provider that listed successfully
//! can say one of its machines is gone; one that failed (or wasn't asked) leaves its rows as
//! they were. New rows are only this fleet's names (`{prefix}-…` and the `@` list entries),
//! so another fleet's machines on a shared account stay out — `arena spend`'s RunPod footer
//! says how much of the account's billing the rows account for.
//!
//! **AI keys** (OpenRouter, by hash): owner = the owner of the machine the key was minted for
//! when it was minted (or the first one that machine gets, for a key minted before it), or
//! assigned like a machine. Spend is the key's all-time `usage`, sampled; before arena
//! deletes a key (rotate, revoke, terminate --revoke-key, a handover) its usage is recorded
//! from the listing just taken, so the old owner keeps it. A key that vanished outside arena
//! keeps its last seen usage, flagged.
//!
//! **Currencies are never summed**: RunPod, Vast and Shadeform bill USD; Hetzner bills the
//! currency its account is in (prod EUR, the sandbox USD — `GET /pricing` says; until it was
//! read, EUR as everywhere else in arena). Totals are per currency.
//!
//! Layout as balance: the state transition ([`observe`], [`apply_event`], [`observe_keys`]),
//! the views and renderers are pure and table-tested; the I/O (the state file `spend.json`,
//! 0600, per fleet prefix next to `balance.json`, under the state directory's lock) is last.
//! Quiet everywhere but `arena spend`: a failing source or write changes nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::balance::{self, floor_hour, Billed};
use crate::fleet::fmt_money;
use crate::openrouter::KeyInfo;
use crate::pod::Pod;
use crate::proxy::{Listing, ListingStatus};
use crate::selector::Naming;
use crate::status::bills_hourly;
use crate::table::{self, Align};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The state file's name in the fleet's state directory.
pub const SPEND_FILE: &str = "spend.json";

/// The state file's format.
pub const STATE_VERSION: u32 = 1;

/// A stretch longer than this with no observation of a machine is an estimate (seconds):
/// cron lists every 2–15 minutes where it's installed, so a longer silence means nothing was
/// watching.
pub const GAP: u64 = 30 * 60;

/// An unchanged machine's last observation is moved at most this often (seconds): the
/// dashboard lists every few seconds, and a write per refresh says nothing new.
pub const REFRESH: u64 = 60;

/// A machine arena just created that a listing doesn't show yet isn't taken as gone for this
/// long (seconds): a provider's listing can lag its create.
pub const CREATE_GRACE: u64 = 5 * 60;

/// A machine arena terminated that a listing still shows (a provider winding it down: Hetzner's
/// `deleting`, a pod not yet gone from the list) stays ended for this long (seconds); listed
/// after that, the terminate didn't take, and it's open again.
pub const TERMINATE_GRACE: u64 = 10 * 60;

/// A key's usage is sampled at most once per this many seconds (one sample per UTC hour):
/// `--since DATE` needs the usage at a day's start, not every refresh's.
pub const KEY_SAMPLE_EVERY: u64 = 3_600;

/// Key samples older than this are thinned to one per UTC day (seconds).
pub const KEY_THIN_AFTER: u64 = 31 * 86_400;

/// A RunPod billing hour is settled — what RunPod billed for it, whole — this long after it
/// began (seconds): live, a bucket shows up about an hour after its usage and fills in
/// ~5-minute steps.
pub const SETTLED: u64 = 2 * 3_600;

/// The longest owner name.
pub const OWNER_MAX: usize = 64;

/// How a row says "no owner".
pub const NO_OWNER: &str = "(no owner)";

/// Close enough to be the same rate.
const SAME_RATE: f64 = 1e-9;

// ---------------------------------------------------------------------------------------
// State (spend.json)
// ---------------------------------------------------------------------------------------

/// When a machine's billing began, as far as arena knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartHow {
    /// arena created it: the create's own time (exact).
    Created,
    /// Its provider's creation time (Hetzner `created`, RunPod `createdAt`): seen later.
    Provider,
    /// Its first observation: it may have started earlier (before tracking began, or on a
    /// provider that doesn't say).
    Seen,
}

/// How a machine's end is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndHow {
    /// arena terminated it (exact).
    Terminated,
    /// Its provider listed it as terminated: it ended between that and the observation before.
    Gone,
    /// A complete listing of its provider no longer had it: it ended between that listing and
    /// the one before (charged to the later one: errs high by at most the gap).
    Vanished,
}

impl EndHow {
    fn label(self) -> &'static str {
        match self {
            EndHow::Terminated => "terminated",
            EndHow::Gone => "gone",
            EndHow::Vanished => "vanished",
        }
    }
}

/// A machine's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub at: u64,
    pub how: EndHow,
}

/// Where a segment's rate comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateHow {
    /// The listing's (or the create's) price — plus a running RunPod pod's disk.
    Listed,
    /// The listing had no price: this instance's last known one (an instance's price is fixed).
    Remembered,
    /// Stopped: its disk at the provider's published storage rate (RunPod: the volume disk).
    Storage,
    /// Stopped on Vast: its disk at a storage ceiling (hosts set their own; errs high).
    Ceiling,
    /// No price known: counted as 0 (errs low).
    Unpriced,
}

impl RateHow {
    /// Listed and remembered are the same rate: neither starts a segment.
    fn class(self) -> u8 {
        match self {
            RateHow::Listed | RateHow::Remembered => 0,
            RateHow::Storage => 1,
            RateHow::Ceiling => 2,
            RateHow::Unpriced => 3,
        }
    }
}

/// One stretch at one rate: observed at `from` and last at `to`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Seg {
    pub from: u64,
    pub to: u64,
    /// In the provider's currency, per hour.
    pub per_hr: f64,
    /// Billing its full rate (running — or a Hetzner server, which bills while off).
    pub running: bool,
    pub how: RateHow,
}

/// One machine instance (provider + provider id).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub provider: String,
    pub id: String,
    /// Its name as last seen.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// GPU (`1×RTX A4000`) or server type (`cx23`), as last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    /// When its billing began (see `start_how`).
    pub start: u64,
    pub start_how: StartHow,
    pub first_seen: u64,
    pub last_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    pub segs: Vec<Seg>,
    /// Its disks as last listed: what a stop leaves billing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_gb: Option<f64>,
    /// What it cost from `start` to its end (or last observation), metered — kept current by
    /// every change, for whoever reads the file; `arena spend` recomputes it per window.
    #[serde(default)]
    pub accrued: f64,
    /// Whether any part of `accrued` is an estimate.
    #[serde(default)]
    pub estimate: bool,
}

impl Machine {
    pub fn is_open(&self) -> bool {
        self.ended.is_none()
    }

    /// The end of what's known: its end, else its last observation.
    pub fn until(&self) -> u64 {
        self.ended.map_or(self.last_seen, |e| e.at.max(self.last_seen))
    }

    fn last_running_rate(&self) -> Option<f64> {
        self.segs.iter().rev().find(|s| s.running && s.how != RateHow::Unpriced).map(|s| s.per_hr)
    }
}

/// How a key's owner is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyOwnerHow {
    /// The owner of the machine it was minted for, when arena minted it.
    Minted,
    /// Minted before its machine had an owner: the first owner that machine got.
    Adopted,
    /// `arena spend assign`.
    Assigned,
}

/// How a key's end is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyEndHow {
    /// arena deleted it, its usage read just before.
    Deleted,
    /// Gone from a complete listing, not by arena: its usage is the last one seen (errs low).
    Vanished,
}

/// A key's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyGone {
    pub at: u64,
    pub how: KeyEndHow,
}

/// One OpenRouter key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiKey {
    pub hash: String,
    /// Its name (`{prefix}-apple`: the machine it was minted for).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_how: Option<KeyOwnerHow>,
    /// OpenRouter's `created_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    pub first_seen: u64,
    /// `(when, all-time usage USD)`, at most one per UTC hour (per day once old).
    pub usage: Vec<(u64, f64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone: Option<KeyGone>,
}

impl AiKey {
    pub fn last_usage(&self) -> f64 {
        self.usage.last().map_or(0.0, |u| u.1)
    }
}

/// `spend.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpendState {
    pub version: u32,
    /// When tracking began (the first observation or event recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(default)]
    pub machines: Vec<Machine>,
    #[serde(default)]
    pub keys: Vec<AiKey>,
    /// An account's billing currency where read (`hetzner` → `USD`/`EUR`, from `GET /pricing`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub currencies: BTreeMap<String, String>,
}

impl Default for SpendState {
    fn default() -> Self {
        SpendState { version: STATE_VERSION, since: None, machines: Vec::new(), keys: Vec::new(), currencies: BTreeMap::new() }
    }
}

impl SpendState {
    fn begin(&mut self, now: u64) {
        self.version = STATE_VERSION;
        if self.since.is_none_or(|s| s > now) {
            self.since = Some(self.since.map_or(now, |s| s.min(now)));
        }
    }

    /// The open machine `id` on `provider` — any provider when `None`, then only if exactly one
    /// open machine has that id (ids are only unique per provider).
    fn open_index(&self, provider: Option<&str>, id: &str) -> Option<usize> {
        let mut hits = self
            .machines
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_open() && m.id == id && provider.is_none_or(|p| m.provider == p))
            .map(|(i, _)| i);
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    }

    /// The account currency of `provider`'s machines: as read, else what arena assumes
    /// elsewhere (Hetzner EUR, the rest USD).
    pub fn currency(&self, provider: &str) -> String {
        self.currencies.get(provider).cloned().unwrap_or_else(|| default_currency(provider).to_string())
    }
}

/// arena's assumption without a reading ([`crate::fleet::currency_symbol`]).
pub fn default_currency(provider: &str) -> &'static str {
    if crate::fleet::currency_symbol(provider) == "€" {
        "EUR"
    } else {
        "USD"
    }
}

/// `$`, `€`, or the code and a space.
pub fn symbol(currency: &str) -> String {
    match currency {
        "USD" => "$".into(),
        "EUR" => "€".into(),
        other => format!("{other} "),
    }
}

// ---------------------------------------------------------------------------------------
// The fold: listings
// ---------------------------------------------------------------------------------------

/// A listed pod's rate: `None` when the listing says it's gone for good.
fn rate_of(p: &Pod, last_running: Option<f64>) -> Option<(f64, bool, RateHow)> {
    if balance::gone_for_good(&p.status) {
        return None;
    }
    if bills_hourly(&p.provider, &p.status) {
        // A running RunPod pod's disk bills on top of its listed price (balance's metering too).
        let disk = if p.provider == "runpod" {
            balance::disk_per_hr(p.disk_gb, balance::RUNPOD_RUNNING_DISK_PER_GB_MONTH).unwrap_or(0.0)
        } else {
            0.0
        };
        return Some(match p.cost_per_hr.filter(|c| c.is_finite() && *c >= 0.0) {
            Some(c) => (c + disk, true, RateHow::Listed),
            None => match last_running {
                Some(r) => (r, true, RateHow::Remembered),
                None => (disk, true, RateHow::Unpriced),
            },
        });
    }
    // Stopped: the disk it keeps. Hetzner never gets here (it bills while off); Shadeform
    // can't stop.
    let how = if p.provider == "vast" { RateHow::Ceiling } else { RateHow::Storage };
    Some(match balance::stopped_disk_per_hr(p) {
        Some(r) if r > 0.0 => (r, false, how),
        Some(_) => (0.0, false, RateHow::Storage),
        None if p.provider == "runpod" || p.provider == "vast" => (0.0, false, RateHow::Unpriced),
        None => (0.0, false, RateHow::Storage),
    })
}

/// How a rate reached the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// A listing: a silence longer than [`GAP`] before it is an estimate.
    Listed,
    /// A lifecycle command arena ran: nothing arena knows of changed the rate since the last
    /// record (only arena stops and starts a fleet's machines), so the segment runs up to it.
    Event,
}

/// Extend `m` with a rate observed at `now`: the last segment grows when the rate holds (and,
/// for a listing, the silence before was short); otherwise a new segment starts (the stretch
/// between is charged at the higher end). `min_step`: an unchanged segment isn't moved by
/// less (the dashboard's refreshes). Returns whether anything changed.
fn extend(m: &mut Machine, now: u64, (per_hr, running, how): (f64, bool, RateHow), min_step: u64, seen: Seen) -> bool {
    if now < m.last_seen {
        return false; // the clock went back: a later observation is already recorded
    }
    match m.segs.last_mut() {
        Some(s)
            if s.running == running
                && s.how.class() == how.class()
                && (s.per_hr - per_hr).abs() < SAME_RATE
                && (seen == Seen::Event || now - s.to <= GAP) =>
        {
            if now - s.to < min_step {
                return false;
            }
            s.to = now;
        }
        _ => m.segs.push(Seg { from: now, to: now, per_hr, running, how }),
    }
    m.last_seen = now;
    true
}

/// Whether a listed name is one of this fleet's: `{prefix}-…` or an `@` list entry.
pub fn claims(naming: &Naming, name: &str) -> bool {
    naming.is_cohort(name) || naming.is_staff(name)
}

/// The fold over one listing at `now`. Per provider that listed successfully: each of this
/// fleet's pods extends its open row (or reopens one a glitch ended, or starts a new one);
/// an open row its provider no longer lists, or lists as terminated, ends. A provider that
/// failed or wasn't asked leaves its rows untouched. `pods` are the listing's pods with
/// their details filled (`pods list`'s enrich). Never ends a machine arena created under
/// [`CREATE_GRACE`] ago.
pub fn observe(mut state: SpendState, listing: &Listing, pods: &[Pod], naming: &Naming, now: u64) -> SpendState {
    let ok: BTreeSet<&str> = listing
        .providers
        .iter()
        .map(|pl| pl.provider.as_str())
        .filter(|p| listing.status(p) == ListingStatus::Ok)
        .collect();
    if ok.is_empty() {
        return state;
    }
    state.begin(now);
    // What the providers listed — from the listing itself, so a caller's narrower `pods` can
    // never make a machine look gone.
    let listed: BTreeSet<(&str, &str)> = listing
        .providers
        .iter()
        .filter(|pl| ok.contains(pl.provider.as_str()))
        .filter_map(|pl| pl.pods.as_ref().ok())
        .flatten()
        .map(|p| (p.provider.as_str(), p.id.as_str()))
        .collect();
    for p in pods.iter().filter(|p| ok.contains(p.provider.as_str()) && !p.id.is_empty()) {
        let found = state.open_index(Some(&p.provider), &p.id).or_else(|| {
            // Ended by a listing that didn't show it (a glitch) — or by a terminate that
            // didn't take: it's here, so it's open again.
            state.machines.iter().rposition(|m| m.provider == p.provider && m.id == p.id)
        });
        match found {
            Some(i) => {
                let m = &mut state.machines[i];
                let rate = rate_of(p, m.last_running_rate());
                let winding_down = m.ended.is_some_and(|e| e.how == EndHow::Terminated && now.saturating_sub(e.at) < TERMINATE_GRACE);
                if m.ended.is_some() && rate.is_some() && !winding_down {
                    m.ended = None;
                }
                if !m.is_open() {
                    continue;
                }
                let moved = m.name != p.name;
                note_listing(m, p);
                match rate {
                    Some(r) => {
                        extend(m, now, r, if moved { 0 } else { REFRESH }, Seen::Listed);
                    }
                    None => end(m, now, EndHow::Gone),
                }
            }
            None if claims(naming, &p.name) => {
                if let Some(r) = rate_of(p, None) {
                    let since = state.since.unwrap_or(now);
                    let (start, start_how) = match p.created_at.filter(|c| *c <= now && *c >= since) {
                        Some(c) => (c, StartHow::Provider),
                        None => (now, StartHow::Seen),
                    };
                    let mut m = new_machine(p, start, start_how, now);
                    extend(&mut m, now, r, 0, Seen::Listed);
                    state.machines.push(m);
                }
            }
            None => {}
        }
    }
    for m in state.machines.iter_mut().filter(|m| m.is_open() && ok.contains(m.provider.as_str())) {
        if listed.contains(&(m.provider.as_str(), m.id.as_str())) {
            continue;
        }
        if m.start_how == StartHow::Created && now.saturating_sub(m.first_seen) < CREATE_GRACE {
            continue;
        }
        end(m, now, EndHow::Vanished);
    }
    refresh_accrued(&mut state);
    state
}

fn new_machine(p: &Pod, start: u64, start_how: StartHow, now: u64) -> Machine {
    let mut m = Machine {
        provider: p.provider.clone(),
        id: p.id.clone(),
        name: p.name.clone(),
        owner: None,
        gpu: None,
        start,
        start_how,
        first_seen: now,
        last_seen: now,
        ended: None,
        segs: Vec::new(),
        disk_gb: None,
        volume_gb: None,
        accrued: 0.0,
        estimate: false,
    };
    note_listing(&mut m, p);
    m
}

/// What a listing says about a machine besides its rate.
fn note_listing(m: &mut Machine, p: &Pod) {
    m.name.clone_from(&p.name);
    // Its GPU (or server type) doesn't change: the first label kept (`pods list`'s probe and
    // the plain listing spell it differently).
    let gpu = crate::fleet::gpu_label(p);
    if m.gpu.is_none() && gpu != "-" {
        m.gpu = Some(gpu);
    }
    if p.disk_gb.is_some() {
        m.disk_gb = p.disk_gb;
    }
    if p.volume_gb.is_some() {
        m.volume_gb = p.volume_gb;
    }
}

fn end(m: &mut Machine, now: u64, how: EndHow) {
    if m.ended.is_none() {
        m.ended = Some(Ended { at: now.max(m.last_seen), how });
    }
}

// ---------------------------------------------------------------------------------------
// The fold: lifecycle events
// ---------------------------------------------------------------------------------------

/// What an owner a create records: one given (`--owner`), the one of the open machine of the
/// same name it replaces (`replace`/`migrate`: the same person's machine, moved), or none.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum OwnerRule {
    Given(String),
    Inherit,
    #[default]
    None,
}

/// A lifecycle change arena made, recorded at the moment it was made.
#[derive(Clone, Debug)]
pub enum Event<'a> {
    /// A create returned this pod.
    Created { pod: &'a Pod, owner: &'a OwnerRule },
    /// A terminate succeeded (`provider` when the caller knows it).
    Terminated { provider: Option<&'a str>, id: &'a str },
    /// A stop succeeded: from now it bills its disk only.
    Stopped { provider: Option<&'a str>, id: &'a str },
    /// A start succeeded: from now it bills its running price again.
    Started { provider: Option<&'a str>, id: &'a str },
}

/// Record one lifecycle event at `now`. An event for an instance the ledger doesn't have
/// (or can't tell apart: the same id on two providers) changes nothing — the next listing
/// sees it.
pub fn apply_event(mut state: SpendState, ev: &Event, now: u64) -> SpendState {
    state.begin(now);
    match ev {
        Event::Created { pod, owner } => {
            if pod.id.is_empty() {
                return state;
            }
            let inherited = match owner {
                OwnerRule::Given(o) => Some(o.clone()),
                OwnerRule::Inherit => state
                    .machines
                    .iter()
                    .filter(|m| m.is_open() && m.name == pod.name && !(m.provider == pod.provider && m.id == pod.id))
                    .find_map(|m| m.owner.clone()),
                OwnerRule::None => None,
            };
            let i = match state.open_index(Some(&pod.provider), &pod.id) {
                Some(i) => i,
                None => {
                    let mut m = new_machine(pod, now, StartHow::Created, now);
                    if let Some(r) = rate_of(pod, None) {
                        extend(&mut m, now, r, 0, Seen::Event);
                    }
                    state.machines.push(m);
                    state.machines.len() - 1
                }
            };
            if let Some(o) = inherited {
                state.machines[i].owner = Some(o.clone());
                adopt_keys(&mut state, &pod.name, &o);
            }
        }
        Event::Terminated { provider, id } => {
            if let Some(i) = state.open_index(*provider, id) {
                let m = &mut state.machines[i];
                if let Some(s) = m.segs.last().copied() {
                    extend(m, now, (s.per_hr, s.running, s.how), 0, Seen::Event);
                }
                m.ended = Some(Ended { at: now.max(m.last_seen), how: EndHow::Terminated });
            }
        }
        Event::Stopped { provider, id } => {
            if let Some(i) = state.open_index(*provider, id) {
                let m = &mut state.machines[i];
                if let Some(s) = m.segs.last().copied() {
                    extend(m, now, (s.per_hr, s.running, s.how), 0, Seen::Event);
                }
                // What it bills stopped, from its disks as last listed.
                let probe = Pod {
                    provider: m.provider.clone(),
                    status: "EXITED".into(),
                    disk_gb: m.disk_gb,
                    volume_gb: m.volume_gb,
                    ..Pod::default()
                };
                if !bills_hourly(&m.provider, "EXITED") {
                    if let Some(r) = rate_of(&probe, None) {
                        extend(m, now, r, 0, Seen::Event);
                    }
                }
            }
        }
        Event::Started { provider, id } => {
            if let Some(i) = state.open_index(*provider, id) {
                let m = &mut state.machines[i];
                let rate = match m.last_running_rate() {
                    Some(r) => (r, true, RateHow::Remembered),
                    None => (0.0, true, RateHow::Unpriced),
                };
                if let Some(s) = m.segs.last().copied() {
                    extend(m, now, (s.per_hr, s.running, s.how), 0, Seen::Event);
                }
                extend(m, now, rate, 0, Seen::Event);
            }
        }
    }
    refresh_accrued(&mut state);
    state
}

/// What a target given to `arena spend assign` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assigned {
    /// The machine (its current instance, or the instance by id).
    Machine { name: String, provider: String, id: String, was: Option<String> },
    /// An AI key, by hash.
    Key { name: String, hash: String, was: Option<String> },
}

/// `arena spend assign <target> <owner>` (`owner` `None`: clear it): `target` is a machine
/// name (its open instance — bare or full: `full_names` are the forms it may take), else an
/// instance id, else an AI key's hash (or a unique prefix of 8+). Keys of that machine that
/// have no owner adopt the new one. Errors say what matched instead.
pub fn assign(mut state: SpendState, full_names: &[String], target: &str, owner: Option<&str>, now: u64) -> Result<(SpendState, Assigned), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("no machine, instance id or key hash given".into());
    }
    let by_name: Vec<usize> = state
        .machines
        .iter()
        .enumerate()
        .filter(|(_, m)| m.is_open() && full_names.contains(&m.name))
        .map(|(i, _)| i)
        .collect();
    let by_id: Vec<usize> = state.machines.iter().enumerate().filter(|(_, m)| m.id == target).map(|(i, _)| i).collect();
    let pick = match (by_name.as_slice(), by_id.as_slice()) {
        ([i], _) => Some(*i),
        ([], [i]) => Some(*i),
        ([_, _, ..], _) => {
            let ids: Vec<String> = by_name.iter().map(|i| format!("{} {}", state.machines[*i].provider, state.machines[*i].id)).collect();
            return Err(format!("{target} names {} running instances ({}) — give the instance id", by_name.len(), ids.join(", ")));
        }
        ([], [_, _, ..]) => return Err(format!("instance id {target} is on several providers — rename one apart first")),
        ([], []) => None,
    };
    state.begin(now);
    if let Some(i) = pick {
        let m = &mut state.machines[i];
        let was = std::mem::replace(&mut m.owner, owner.map(String::from));
        let (name, provider, id) = (m.name.clone(), m.provider.clone(), m.id.clone());
        if let Some(o) = owner {
            adopt_keys(&mut state, &name, o);
        }
        return Ok((state, Assigned::Machine { name, provider, id, was }));
    }
    let keys: Vec<usize> = state
        .keys
        .iter()
        .enumerate()
        .filter(|(_, k)| k.hash == target || (target.len() >= 8 && k.hash.starts_with(target)))
        .map(|(i, _)| i)
        .collect();
    match keys.as_slice() {
        [i] => {
            let k = &mut state.keys[*i];
            let was = std::mem::replace(&mut k.owner, owner.map(String::from));
            k.owner_how = owner.map(|_| KeyOwnerHow::Assigned);
            let (name, hash) = (k.name.clone(), k.hash.clone());
            Ok((state, Assigned::Key { name, hash, was }))
        }
        [] => Err(format!(
            "no machine, instance id or AI key `{target}` in the spend ledger (`arena spend --by machine` lists them; a \
             machine not seen yet appears once `pods list` or `arena spend` lists it)"
        )),
        _ => Err(format!("`{target}` is the start of {} key hashes — give more of it", keys.len())),
    }
}

/// An owner name as typed: trimmed, at most [`OWNER_MAX`] characters, printable, no comma
/// (the CSV's lists) — and not the "(no owner)" row's label.
pub fn parse_owner(s: &str) -> Result<String, String> {
    let o = s.trim();
    if o.is_empty() {
        return Err("an owner can't be empty".into());
    }
    if o.chars().count() > OWNER_MAX {
        return Err(format!("an owner is at most {OWNER_MAX} characters"));
    }
    if o.chars().any(|c| c.is_control() || c == ',') {
        return Err("an owner can't hold control characters or commas".into());
    }
    if o == NO_OWNER {
        return Err(format!("`{NO_OWNER}` is the label for no owner"));
    }
    Ok(o.to_string())
}

// ---------------------------------------------------------------------------------------
// The fold: AI keys
// ---------------------------------------------------------------------------------------

/// Whether key `name` is machine `machine`'s (as `keys gen` names it: the machine's name).
fn key_for(naming: &Naming, key: &str, machine: &str) -> bool {
    key == machine || crate::openrouter::key_name(naming.prefix, naming.list, machine) == key
}

/// Keys named for `machine` with no owner (and not gone) adopt `owner`: a key minted before
/// its machine had one.
fn adopt_keys(state: &mut SpendState, machine: &str, owner: &str) {
    for k in state.keys.iter_mut().filter(|k| k.owner.is_none() && k.gone.is_none() && k.name == machine) {
        k.owner = Some(owner.to_string());
        k.owner_how = Some(KeyOwnerHow::Adopted);
    }
}

/// Record a usage reading, at most one per UTC hour (a later one in the same hour replaces
/// it), thinning old samples to one per day. A reading never goes down (a refund or a reset
/// isn't spend we can place): the higher is kept.
fn sample(usage: &mut Vec<(u64, f64)>, now: u64, u: f64) {
    if !u.is_finite() || u < 0.0 {
        return;
    }
    let u = usage.last().map_or(u, |l| l.1.max(u));
    match usage.last_mut() {
        Some(l) if (l.1 - u).abs() < SAME_RATE => {}
        Some(l) if now / KEY_SAMPLE_EVERY == l.0 / KEY_SAMPLE_EVERY && now >= l.0 => *l = (now, u),
        Some(l) if now < l.0 => {}
        _ => usage.push((now, u)),
    }
    // Old samples: the last of each UTC day.
    let cut = now.saturating_sub(KEY_THIN_AFTER);
    let mut kept: Vec<(u64, f64)> = Vec::with_capacity(usage.len());
    for s in usage.iter().copied() {
        if let Some(prev) = kept.last_mut() {
            if s.0 < cut && prev.0 / 86_400 == s.0 / 86_400 {
                *prev = s;
                continue;
            }
        }
        kept.push(s);
    }
    *usage = kept;
}

/// The fold over a complete key listing (this fleet's workspace, all pages) at `now`: this
/// fleet's keys (named for its machines) are sampled — a new one gets the owner of the open
/// machine it's named for, if that has one — and a tracked key the listing no longer has is
/// gone (outside arena: its last seen usage stays, flagged).
pub fn observe_keys(mut state: SpendState, keys: &[KeyInfo], naming: &Naming, now: u64) -> SpendState {
    state.begin(now);
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for k in keys {
        let Some(name) = k.name.as_deref().filter(|n| claims(naming, n)) else { continue };
        if k.hash.is_empty() {
            continue;
        }
        seen.insert(k.hash.as_str());
        let i = match state.keys.iter().position(|x| x.hash == k.hash) {
            Some(i) => i,
            None => {
                let owner = state.machines.iter().rev().find(|m| m.is_open() && key_for(naming, name, &m.name)).and_then(|m| m.owner.clone());
                state.keys.push(AiKey {
                    hash: k.hash.clone(),
                    name: name.to_string(),
                    owner_how: owner.as_ref().map(|_| KeyOwnerHow::Adopted),
                    owner,
                    created: k.created_at.as_deref().and_then(crate::snapshot::parse_rfc3339),
                    first_seen: now,
                    usage: Vec::new(),
                    gone: None,
                });
                state.keys.len() - 1
            }
        };
        let key = &mut state.keys[i];
        key.name = name.to_string();
        if key.gone.is_some_and(|g| g.how == KeyEndHow::Vanished) {
            key.gone = None; // listed again
        }
        if let Some(u) = k.usage {
            sample(&mut key.usage, now, u);
        }
    }
    for key in state.keys.iter_mut().filter(|k| k.gone.is_none() && !seen.contains(k.hash.as_str())) {
        key.gone = Some(KeyGone { at: now, how: KeyEndHow::Vanished });
    }
    state
}

/// Keys arena just deleted (`hashes`): their usage is the one recorded from the listing taken
/// before the delete ([`observe_keys`]); they're gone, by arena.
pub fn keys_deleted(mut state: SpendState, hashes: &[String], now: u64) -> SpendState {
    for k in state.keys.iter_mut().filter(|k| hashes.contains(&k.hash)) {
        k.gone = Some(KeyGone { at: now, how: KeyEndHow::Deleted });
    }
    state
}

/// A key arena just minted for `machine`: its owner is that machine's open instance's owner
/// (`owner` when the caller already decided — a handover), if any.
pub fn key_minted(mut state: SpendState, hash: &str, machine: &str, owner: Option<&str>, created: Option<u64>, now: u64) -> SpendState {
    state.begin(now);
    let owner = owner
        .map(String::from)
        .or_else(|| state.machines.iter().rev().find(|m| m.is_open() && m.name == machine).and_then(|m| m.owner.clone()));
    let key = AiKey {
        hash: hash.to_string(),
        name: machine.to_string(),
        owner_how: owner.as_ref().map(|_| KeyOwnerHow::Minted),
        owner,
        created: created.or(Some(now)),
        first_seen: now,
        usage: vec![(now, 0.0)],
        gone: None,
    };
    match state.keys.iter_mut().find(|k| k.hash == hash) {
        Some(k) => {
            k.owner.clone_from(&key.owner);
            k.owner_how = key.owner_how;
        }
        None => state.keys.push(key),
    }
    state
}

/// The owner a name's current key was minted for (or adopted, or assigned): what a create
/// for someone else compares against ([`handover_needed`]). `None`: no tracked key, or one
/// with no owner.
pub fn key_owner<'s>(state: &'s SpendState, machine: &str) -> Option<&'s str> {
    state.keys.iter().rev().find(|k| k.gone.is_none() && k.name == machine).and_then(|k| k.owner.as_deref())
}

/// Whether creating `machine` for `new_owner` hands its key over: the name's current key
/// belongs to a different known owner. Returns that owner.
pub fn handover_needed<'s>(state: &'s SpendState, machine: &str, new_owner: &str) -> Option<&'s str> {
    key_owner(state, machine).filter(|o| *o != new_owner)
}

// ---------------------------------------------------------------------------------------
// Accrual
// ---------------------------------------------------------------------------------------

/// Which way an estimate errs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Errs {
    High,
    Low,
    Either,
}

/// What part of a machine's cost is an estimate, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Est {
    /// Not observed for longer than [`GAP`]: charged at its rate throughout.
    Gap,
    /// Vanished (or listed terminated) between two observations: charged to the later one.
    Vanished,
    /// Made (its provider says) well before arena first saw it: charged back to then at its
    /// first seen rate.
    BeforeSeen,
    /// Running before tracking began: counted from its first observation only.
    PredatesTracking,
    /// No price known: counted as 0.
    Unpriced,
    /// A stopped Vast disk at a storage ceiling.
    Ceiling,
}

impl Est {
    pub fn errs(self) -> Errs {
        match self {
            Est::Gap | Est::Vanished | Est::Ceiling => Errs::High,
            Est::Unpriced | Est::PredatesTracking => Errs::Low,
            Est::BeforeSeen => Errs::Either,
        }
    }

    fn words(self) -> &'static str {
        match self {
            Est::Gap => "not observed for a while (charged at its rate throughout: errs high if it was stopped meanwhile)",
            Est::Vanished => "vanished between two observations (charged to the later one: errs high)",
            Est::BeforeSeen => "first seen well after its provider says it was made (charged back to then at its first rate)",
            Est::PredatesTracking => "running before tracking began (counted from then: errs low)",
            Est::Unpriced => "no price known for a while (counted as 0: errs low)",
            Est::Ceiling => "stopped on Vast (its disk at a storage ceiling: errs high)",
        }
    }
}

/// One stretch to charge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Piece {
    from: u64,
    to: u64,
    per_hr: f64,
    running: bool,
    est: Option<Est>,
}

fn rate_est(how: RateHow) -> Option<Est> {
    match how {
        RateHow::Unpriced => Some(Est::Unpriced),
        RateHow::Ceiling => Some(Est::Ceiling),
        _ => None,
    }
}

/// A machine's stretches: before its first observation (back to its start), its segments,
/// the silences between them (at the higher end), and after the last one to its end.
fn pieces(m: &Machine) -> Vec<Piece> {
    let mut out = Vec::new();
    let Some(first) = m.segs.first() else { return out };
    if m.start < first.from {
        let est = if first.from - m.start > GAP { Some(Est::BeforeSeen) } else { rate_est(first.how) };
        out.push(Piece { from: m.start, to: first.from, per_hr: first.per_hr, running: first.running, est });
    }
    for (i, s) in m.segs.iter().enumerate() {
        if i > 0 {
            let p = &m.segs[i - 1];
            if s.from > p.to {
                let hi = if p.per_hr >= s.per_hr { p } else { s };
                let est = if s.from - p.to > GAP { Some(Est::Gap) } else { rate_est(hi.how) };
                out.push(Piece { from: p.to, to: s.from, per_hr: hi.per_hr, running: hi.running, est });
            }
        }
        out.push(Piece { from: s.from, to: s.to, per_hr: s.per_hr, running: s.running, est: rate_est(s.how) });
    }
    if let (Some(e), Some(last)) = (m.ended, m.segs.last()) {
        if e.at > last.to {
            let est = match e.how {
                EndHow::Terminated => (e.at - last.to > GAP).then_some(Est::Gap).or(rate_est(last.how)),
                EndHow::Gone | EndHow::Vanished => Some(Est::Vanished),
            };
            out.push(Piece { from: last.to, to: e.at, per_hr: last.per_hr, running: last.running, est });
        }
    }
    out
}

/// One UTC hour of a machine's cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hour {
    pub amount: f64,
    /// Of `amount`, the part that's an estimate.
    pub est: f64,
    /// The kinds of estimate in this hour (a stretch charged at 0 is one too).
    pub kinds: BTreeSet<Est>,
    pub running_secs: u64,
}

/// A machine's cost per UTC hour within `[from, to)`, metered.
pub fn metered_by_hour(m: &Machine, from: u64, to: u64) -> BTreeMap<u64, Hour> {
    let mut out: BTreeMap<u64, Hour> = BTreeMap::new();
    for p in pieces(m) {
        let (mut t, end) = (p.from.max(from), p.to.min(to));
        while t < end {
            let hour = floor_hour(t);
            let stop = (hour + 3_600).min(end);
            let h = out.entry(hour).or_default();
            let amount = (stop - t) as f64 / 3_600.0 * p.per_hr.max(0.0);
            h.amount += amount;
            if let Some(e) = p.est {
                h.est += amount;
                h.kinds.insert(e);
            }
            if p.running {
                h.running_secs += stop - t;
            }
            t = stop;
        }
    }
    out
}

/// Recompute every machine's `accrued` and `estimate` (from its start to its end or last
/// observation).
fn refresh_accrued(state: &mut SpendState) {
    for m in &mut state.machines {
        let hours = metered_by_hour(m, 0, u64::MAX);
        m.accrued = hours.values().map(|h| h.amount).sum();
        m.estimate = hours.values().any(|h| !h.kinds.is_empty());
    }
}

/// RunPod's billing split per pod: `GET /v2/billing/pods`, hourly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PodBilled {
    /// The window RunPod resolved (start snapped down, end up, to the hour).
    pub from: u64,
    pub to: u64,
    /// Pod id → UTC hour → USD.
    pub pods: BTreeMap<String, BTreeMap<u64, f64>>,
    /// When it was read.
    pub at: u64,
}

impl PodBilled {
    /// Whether hour `h` is settled: inside the window read, and billed whole by when it was.
    fn settled(&self, h: u64) -> bool {
        h >= self.from && h + 3_600 <= self.to && h + SETTLED <= self.at
    }
}

/// `GET /v2/billing/pods` (`records[] {podId, startTime, totalAmount, …}`, `metadata.query
/// {bucketSize, startTime, endTime}`, `metadata.recordCount`). Anything not plainly hourly,
/// dated, non-negative and complete (fewer records than `recordCount`: paged) is refused —
/// a misread bill would overwrite good metering.
pub fn parse_pod_billing(v: &Value, at: u64) -> Result<PodBilled, String> {
    let time = |x: Option<&Value>| x.and_then(Value::as_str).and_then(crate::snapshot::parse_rfc3339);
    let q = v.pointer("/metadata/query").filter(|q| q.is_object()).ok_or("no metadata.query")?;
    if q.get("bucketSize").and_then(Value::as_str) != Some("hour") {
        return Err("the buckets aren't hourly".into());
    }
    let from = time(q.get("startTime")).ok_or("no readable metadata.query.startTime")?;
    let to = time(q.get("endTime")).ok_or("no readable metadata.query.endTime")?;
    if from > to {
        return Err("the window ends before it starts".into());
    }
    let records = v.get("records").and_then(Value::as_array).ok_or("no records array")?;
    if let Some(n) = v.pointer("/metadata/recordCount").and_then(Value::as_u64) {
        if n as usize > records.len() {
            return Err(format!("{} of {n} records (paged?)", records.len()));
        }
    }
    let mut pods: BTreeMap<String, BTreeMap<u64, f64>> = BTreeMap::new();
    for r in records {
        let id = r.get("podId").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("a record without a podId")?;
        let start = time(r.get("startTime")).ok_or("a record without a readable startTime")?;
        let usd = r
            .get("totalAmount")
            .and_then(crate::provider::runpod::loose_f64)
            .filter(|a| a.is_finite() && (0.0..=1e7).contains(a))
            .ok_or("a record's totalAmount is missing, negative or not a number")?;
        *pods.entry(id.to_string()).or_default().entry(floor_hour(start)).or_default() += usd;
    }
    Ok(PodBilled { from: floor_hour(from), to, pods, at })
}

// ---------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------

/// Where a machine's figure comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// arena's metering.
    Metered,
    /// RunPod's billing for its settled hours, metering for the rest.
    Billed,
}

/// One machine instance in a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MachineRow {
    pub name: String,
    pub provider: String,
    pub id: String,
    pub gpu: Option<String>,
    pub owner: Option<String>,
    pub start: u64,
    pub start_how: StartHow,
    pub end: Option<u64>,
    pub end_how: Option<EndHow>,
    /// Running (full-rate) hours in the window.
    pub hours: f64,
    pub currency: String,
    pub amount: f64,
    /// Of `amount`, the part that's an estimate.
    pub estimated: f64,
    pub estimates: Vec<Est>,
    pub source: Source,
    /// RunPod: what its billing says for the window (every bucket, settled or not).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billed: Option<f64>,
    /// When it was last observed (open machines).
    pub last_seen: u64,
}

/// One key in a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyRow {
    pub name: String,
    pub hash: String,
    pub owner: Option<String>,
    /// USD in the window.
    pub usd: f64,
    /// Its usage before the window isn't known: the first reading after its start was used
    /// (errs low).
    pub base_unknown: bool,
    pub gone: Option<KeyEndHow>,
}

/// One owner's sums.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OwnerRow {
    /// `None`: no owner.
    pub owner: Option<String>,
    /// Machine names (short, each once, in start order).
    pub machines: Vec<String>,
    pub instances: usize,
    pub hours: f64,
    /// Compute per currency.
    pub compute: BTreeMap<String, f64>,
    /// AI keys, USD.
    pub ai: f64,
    /// Compute + AI per currency.
    pub total: BTreeMap<String, f64>,
    /// Whether any part of the compute is an estimate.
    pub estimate: bool,
    /// Whether any part of the AI spend is (a key deleted outside arena, a key first seen
    /// after the window began).
    pub ai_estimate: bool,
}

/// RunPod's footer: what the rows account for of the account's billing in the window.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Accounted {
    pub rows: f64,
    pub account: f64,
}

/// `arena spend`'s whole answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    /// The window: from (`None`: since tracking began) to now.
    pub since: Option<u64>,
    pub tracking_since: Option<u64>,
    pub now: u64,
    pub owners: Vec<OwnerRow>,
    pub total: OwnerRow,
    pub machines: Vec<MachineRow>,
    pub keys: Vec<KeyRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runpod: Option<Accounted>,
}

/// A machine's cost in `[from, to)`, RunPod hours corrected by `billed` where it has them.
fn machine_row(state: &SpendState, m: &Machine, from: u64, to: u64, billed: Option<&PodBilled>) -> MachineRow {
    let metered = metered_by_hour(m, from, to);
    let mut amount = 0.0;
    let mut estimated = 0.0;
    let mut kinds: BTreeSet<Est> = BTreeSet::new();
    let mut source = Source::Metered;
    let mut billed_total = None;
    let mut metering = |h: &Hour, amount: &mut f64| {
        *amount += h.amount;
        estimated += h.est;
        kinds.extend(h.kinds.iter().copied());
    };
    match billed.filter(|_| m.provider == "runpod") {
        Some(b) => {
            let empty = BTreeMap::new();
            let bill = b.pods.get(&m.id).unwrap_or(&empty);
            // The machine's own hours in the window: the metered ones, and the billed ones from
            // its start to its end (a bucket metering missed still counts).
            let first = floor_hour(m.start.max(from));
            let last = floor_hour(m.until());
            let mut hours: BTreeSet<u64> = metered.keys().copied().collect();
            hours.extend(bill.keys().copied().filter(|h| *h >= first && *h < to && *h <= last));
            billed_total = Some(bill.iter().filter(|(h, _)| **h >= first && **h < to && **h <= last).map(|(_, v)| v).sum());
            let none = Hour::default();
            for h in hours {
                let me = metered.get(&h).unwrap_or(&none);
                match bill.get(&h) {
                    // Settled: what RunPod billed for this pod in that hour, whatever metering said.
                    Some(v) if b.settled(h) => {
                        amount += v;
                        source = Source::Billed;
                    }
                    // Still filling in: the higher of the two.
                    Some(v) if *v >= me.amount => amount += v,
                    // No bucket (or a lower one still filling): metering stands.
                    _ => metering(me, &mut amount),
                }
            }
        }
        None => {
            for h in metered.values() {
                metering(h, &mut amount);
            }
        }
    }
    let hours = metered.values().map(|h| h.running_secs).sum::<u64>() as f64 / 3_600.0;
    let mut ests: Vec<Est> = kinds.into_iter().collect();
    if m.start_how == StartHow::Seen && state.since.is_some_and(|s| m.first_seen <= s + REFRESH) && m.first_seen >= from && m.first_seen < to {
        ests.push(Est::PredatesTracking);
    }
    MachineRow {
        name: m.name.clone(),
        provider: m.provider.clone(),
        id: m.id.clone(),
        gpu: m.gpu.clone(),
        owner: m.owner.clone(),
        start: m.start,
        start_how: m.start_how,
        end: m.ended.map(|e| e.at),
        end_how: m.ended.map(|e| e.how),
        hours,
        currency: state.currency(&m.provider),
        amount,
        estimated,
        estimates: ests,
        source,
        billed: billed_total,
        last_seen: m.last_seen,
    }
}

/// A key's spend in `[from, ∞)`: its last usage less its usage at `from` — the last reading
/// at or before then; 0 when it was made after; else its first reading (errs low, flagged).
fn key_row(k: &AiKey, from: Option<u64>) -> KeyRow {
    let last = k.last_usage();
    let (base, unknown) = match from {
        None => (0.0, false),
        Some(f) => match k.usage.iter().rev().find(|(t, _)| *t <= f) {
            Some((_, u)) => (*u, false),
            None if k.created.is_some_and(|c| c >= f) || (k.first_seen >= f && k.usage.first().is_some_and(|u| u.1 == 0.0)) => (0.0, false),
            None => (k.usage.first().map_or(0.0, |u| u.1), true),
        },
    };
    KeyRow {
        name: k.name.clone(),
        hash: k.hash.clone(),
        owner: k.owner.clone(),
        usd: (last - base).max(0.0),
        base_unknown: unknown,
        gone: k.gone.map(|g| g.how),
    }
}

/// The name as an organiser types it: without the fleet prefix.
fn short<'n>(naming: &Naming, name: &'n str) -> &'n str {
    name.strip_prefix(naming.prefix).and_then(|r| r.strip_prefix('-')).unwrap_or(name)
}

fn add(map: &mut BTreeMap<String, f64>, currency: &str, v: f64) {
    *map.entry(currency.to_string()).or_default() += v;
}

/// Build the report for `[since, now)`. `billed`: RunPod's per-pod billing for the window
/// (read by `arena spend`); `account`: RunPod's account-wide billing over the same window
/// (what the footer compares with).
pub fn report(state: &SpendState, naming: &Naming, since: Option<u64>, now: u64, billed: Option<&PodBilled>, account: Option<&Billed>) -> Report {
    let from = since.unwrap_or(0);
    let mut machines: Vec<MachineRow> = state
        .machines
        .iter()
        .filter(|m| m.start <= now && m.until() >= from)
        .map(|m| machine_row(state, m, from, now, billed))
        .collect();
    machines.sort_by(|a, b| a.start.cmp(&b.start).then(a.name.cmp(&b.name)));
    let keys: Vec<KeyRow> = state
        .keys
        .iter()
        .filter(|k| k.gone.is_none_or(|g| g.at >= from))
        .map(|k| key_row(k, since))
        .filter(|r| r.usd > 0.0)
        .collect();
    let mut by: BTreeMap<Option<String>, OwnerRow> = BTreeMap::new();
    for r in &machines {
        let o = by.entry(r.owner.clone()).or_insert_with(|| OwnerRow { owner: r.owner.clone(), ..Default::default() });
        let n = short(naming, &r.name).to_string();
        if !o.machines.contains(&n) {
            o.machines.push(n);
        }
        o.instances += 1;
        o.hours += r.hours;
        add(&mut o.compute, &r.currency, r.amount);
        add(&mut o.total, &r.currency, r.amount);
        o.estimate |= !r.estimates.is_empty();
    }
    for k in &keys {
        let o = by.entry(k.owner.clone()).or_insert_with(|| OwnerRow { owner: k.owner.clone(), ..Default::default() });
        o.ai += k.usd;
        add(&mut o.total, "USD", k.usd);
        o.ai_estimate |= k.base_unknown || k.gone == Some(KeyEndHow::Vanished);
    }
    // Owners by name; the "(no owner)" row last, so a forgotten --owner stands out.
    let mut owners: Vec<OwnerRow> = by.into_values().collect();
    owners.sort_by(|a, b| match (&a.owner, &b.owner) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.to_lowercase().cmp(&y.to_lowercase()).then(x.cmp(y)),
    });
    let mut total = OwnerRow::default();
    for o in &owners {
        total.instances += o.instances;
        total.hours += o.hours;
        total.ai += o.ai;
        total.estimate |= o.estimate;
        total.ai_estimate |= o.ai_estimate;
        for (c, v) in &o.compute {
            add(&mut total.compute, c, *v);
        }
        for (c, v) in &o.total {
            add(&mut total.total, c, *v);
        }
    }
    let runpod = match (billed, account) {
        (Some(b), Some(a)) => {
            let rows: f64 = machines.iter().filter(|r| r.provider == "runpod").filter_map(|r| r.billed).sum();
            let start = floor_hour(from.max(b.from).max(a.from));
            let account: f64 = a.buckets.iter().filter(|(h, _)| *h >= start && *h < now).map(|(_, v)| v).sum();
            Some(Accounted { rows, account })
        }
        _ => None,
    };
    Report { since, tracking_since: state.since, now, owners, total, machines, keys, runpod }
}

// ---------------------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------------------

/// `$4.20 + €0.15`; `-` for nothing.
pub fn amounts(map: &BTreeMap<String, f64>) -> String {
    let parts: Vec<String> = map.iter().filter(|(_, v)| v.abs() >= 0.0005).map(|(c, v)| fmt_money(&symbol(c), *v)).collect();
    if parts.is_empty() {
        "$0.00".into()
    } else {
        parts.join(" + ")
    }
}

fn hours(h: f64) -> String {
    format!("{h:.1}")
}

fn ymd_hm(t: u64) -> String {
    let s = crate::snapshot::rfc3339(t);
    format!("{} {}", &s[..10], &s[11..16])
}

fn machines_cell(o: &OwnerRow) -> String {
    let names = o.machines.join(", ");
    if o.instances > o.machines.len() {
        format!("{names} ({} instances)", o.instances)
    } else {
        names
    }
}

/// The per-owner table, its totals row and the notes.
pub fn render_owners(r: &Report) -> String {
    if r.owners.is_empty() {
        return format!("(nothing recorded{} yet — `pods list` or `pods up` records machines as they're seen)\n", window_words(r));
    }
    let headers = ["OWNER", "MACHINES", "HOURS", "COMPUTE", "AI", "TOTAL"];
    let align = [Align::Left, Align::Left, Align::Right, Align::Right, Align::Right, Align::Right];
    let tilde = |e: bool| if e { "~" } else { "" };
    let mut rows: Vec<Vec<String>> = r
        .owners
        .iter()
        .map(|o| {
            vec![
                o.owner.clone().unwrap_or_else(|| NO_OWNER.into()),
                machines_cell(o),
                hours(o.hours),
                format!("{}{}", tilde(o.estimate), amounts(&o.compute)),
                format!("{}{}", tilde(o.ai_estimate), fmt_money("$", o.ai)),
                format!("{}{}", tilde(o.estimate || o.ai_estimate), amounts(&o.total)),
            ]
        })
        .collect();
    let t = &r.total;
    rows.push(vec![
        "total".into(),
        format!("{} instance(s)", t.instances),
        hours(t.hours),
        format!("{}{}", tilde(t.estimate), amounts(&t.compute)),
        format!("{}{}", tilde(t.ai_estimate), fmt_money("$", t.ai)),
        format!("{}{}", tilde(t.estimate || t.ai_estimate), amounts(&t.total)),
    ]);
    let mut out = table::render(&headers, &align, &rows);
    for n in notes(r) {
        out.push_str(&format!("  {n}\n"));
    }
    out
}

/// The per-instance table (`--by machine`).
pub fn render_machines(r: &Report, naming: &Naming) -> String {
    if r.machines.is_empty() {
        return format!("(no machine recorded{})\n", window_words(r));
    }
    let headers = ["NAME", "ID", "PROVIDER", "GPU", "OWNER", "START (UTC)", "END (UTC)", "HOURS", "COST", "SOURCE"];
    let align = [Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Right, Align::Left];
    let rows: Vec<Vec<String>> = r
        .machines
        .iter()
        .map(|m| {
            let end = match (m.end, m.end_how) {
                (Some(t), Some(h)) => format!("{} {}", ymd_hm(t), h.label()),
                _ => format!("running (seen {})", ago(r.now, m.last_seen)),
            };
            let start = match m.start_how {
                StartHow::Seen => format!("{} (first seen)", ymd_hm(m.start)),
                _ => ymd_hm(m.start),
            };
            let tilde = if m.estimates.is_empty() { "" } else { "~" };
            vec![
                short(naming, &m.name).to_string(),
                m.id.clone(),
                m.provider.clone(),
                m.gpu.clone().unwrap_or_else(|| "-".into()),
                m.owner.clone().unwrap_or_else(|| "-".into()),
                start,
                end,
                hours(m.hours),
                format!("{tilde}{}", fmt_money(&symbol(&m.currency), m.amount)),
                match m.source {
                    Source::Billed => "billed".into(),
                    Source::Metered => "metered".into(),
                },
            ]
        })
        .collect();
    let mut out = table::render(&headers, &align, &rows);
    for n in notes(r) {
        out.push_str(&format!("  {n}\n"));
    }
    out
}

fn ago(now: u64, t: u64) -> String {
    format!("{} ago", crate::snapshot::fmt_age(now.saturating_sub(t)))
}

fn window_words(r: &Report) -> String {
    match r.since {
        Some(s) => format!(" since {}", &crate::snapshot::rfc3339(s)[..10]),
        None => String::new(),
    }
}

/// The lines under a table: the window, the estimates (with the way they err), keys that
/// went without arena, RunPod's footer.
pub fn notes(r: &Report) -> Vec<String> {
    let mut out = Vec::new();
    let since = r.since.or(r.tracking_since);
    if let Some(s) = since {
        let what = if r.since.is_some() { "since" } else { "tracked since" };
        out.push(format!("{what} {} UTC; AI = each key's OpenRouter usage{}", ymd_hm(s), if r.since.is_some() { " in the window" } else { " (all time)" }));
    }
    let mut by_kind: BTreeMap<Est, (Vec<String>, f64)> = BTreeMap::new();
    for m in r.machines.iter().filter(|m| !m.estimates.is_empty()) {
        for e in &m.estimates {
            let entry = by_kind.entry(*e).or_default();
            entry.0.push(m.name.clone());
            entry.1 += m.estimated;
        }
    }
    for (e, (names, _)) in &by_kind {
        let shown: Vec<&str> = names.iter().take(4).map(String::as_str).collect();
        let more = if names.len() > 4 { format!(" +{} more", names.len() - 4) } else { String::new() };
        out.push(format!("~ {}{more}: {}", shown.join(", "), e.words()));
    }
    let vanished: Vec<&str> = r.keys.iter().filter(|k| k.gone == Some(KeyEndHow::Vanished)).map(|k| k.name.as_str()).collect();
    if !vanished.is_empty() {
        out.push(format!("~ AI key(s) deleted outside arena: {} — their last seen usage (errs low)", vanished.join(", ")));
    }
    let unknown: Vec<&str> = r.keys.iter().filter(|k| k.base_unknown).map(|k| k.name.as_str()).collect();
    if !unknown.is_empty() {
        out.push(format!("~ AI key(s) first seen after the window began: {} — counted from then (errs low)", unknown.join(", ")));
    }
    if let Some(a) = r.runpod {
        out.push(format!(
            "runpod: accounted for {} of {} billed on the account in this window (billing lags ~1 h; the rest is other pods, \
             volumes or serverless)",
            fmt_money("$", a.rows),
            fmt_money("$", a.account)
        ));
    }
    out
}

/// One CSV field: quoted when it holds a comma, quote or line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_line(fields: &[String]) -> String {
    let mut s = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    s.push('\n');
    s
}

fn num(v: f64) -> String {
    format!("{v:.4}")
}

/// The currencies a report holds, USD first (AI is USD).
fn currencies(r: &Report) -> Vec<String> {
    let mut cs: BTreeSet<String> = r.total.total.keys().cloned().collect();
    cs.insert("USD".into());
    let mut v: Vec<String> = cs.into_iter().collect();
    v.sort_by_key(|c| (c != "USD", c.clone()));
    v
}

/// `--csv`: one row per owner (`compute_<cur>`, `ai_usd`, `total_<cur>` per currency held).
pub fn csv_owners(r: &Report) -> String {
    let cs = currencies(r);
    let mut head: Vec<String> = vec!["owner".into(), "machines".into(), "instances".into(), "hours".into()];
    head.extend(cs.iter().map(|c| format!("compute_{}", c.to_lowercase())));
    head.push("ai_usd".into());
    head.extend(cs.iter().map(|c| format!("total_{}", c.to_lowercase())));
    head.push("estimate".into());
    let mut out = csv_line(&head);
    for o in r.owners.iter().chain(std::iter::once(&r.total)) {
        let label = if std::ptr::eq(o, &r.total) { "total".to_string() } else { o.owner.clone().unwrap_or_else(|| NO_OWNER.into()) };
        let mut f = vec![label, o.machines.join(" "), o.instances.to_string(), num(o.hours)];
        f.extend(cs.iter().map(|c| num(o.compute.get(c).copied().unwrap_or(0.0))));
        f.push(num(o.ai));
        f.extend(cs.iter().map(|c| num(o.total.get(c).copied().unwrap_or(0.0))));
        f.push((o.estimate || o.ai_estimate).to_string());
        out.push_str(&csv_line(&f));
    }
    out
}

/// `--by machine --csv`: one row per instance.
pub fn csv_machines(r: &Report) -> String {
    let head: Vec<String> = ["name", "id", "provider", "gpu", "owner", "start", "end", "end_how", "hours", "currency", "cost", "source", "estimate", "estimated"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut out = csv_line(&head);
    for m in &r.machines {
        let f = vec![
            m.name.clone(),
            m.id.clone(),
            m.provider.clone(),
            m.gpu.clone().unwrap_or_default(),
            m.owner.clone().unwrap_or_default(),
            crate::snapshot::rfc3339(m.start),
            m.end.map(crate::snapshot::rfc3339).unwrap_or_default(),
            m.end_how.map(|h| h.label().to_string()).unwrap_or_default(),
            num(m.hours),
            m.currency.clone(),
            num(m.amount),
            match m.source {
                Source::Billed => "billed".into(),
                Source::Metered => "metered".into(),
            },
            m.estimates.iter().map(|e| format!("{e:?}").to_lowercase()).collect::<Vec<_>>().join(" "),
            num(m.estimated),
        ];
        out.push_str(&csv_line(&f));
    }
    out
}

/// `--since DATE`: `YYYY-MM-DD` (UTC midnight) or RFC 3339.
pub fn parse_since(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some((y, m, d)) = crate::schedule::parse_ymd(s).filter(|_| s.len() == 10) {
        let days = crate::schedule::days_from_civil(y, m, d);
        return u64::try_from(days).map(|d| d * 86_400).map_err(|_| format!("`{s}` is before 1970"));
    }
    crate::snapshot::parse_rfc3339(s).ok_or_else(|| format!("`{s}` is not a date (YYYY-MM-DD, UTC) or an RFC 3339 time"))
}

// ---------------------------------------------------------------------------------------
// The state file
// ---------------------------------------------------------------------------------------

/// Parse `spend.json`: `Err` (with why) for anything that isn't a version-1 file.
pub fn parse_state(text: &str) -> Result<SpendState, String> {
    let s: SpendState = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if s.version != STATE_VERSION {
        return Err(format!("unsupported version {} (expected {STATE_VERSION})", s.version));
    }
    Ok(s)
}

/// How a load went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    Ok,
    Missing,
    /// Unreadable, corrupt or another version: ignored (why).
    Ignored(String),
}

/// Load tolerantly: missing → empty; unreadable, corrupt or another version → empty and why.
pub fn load(path: &Path) -> (SpendState, Loaded) {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_state(&text) {
            Ok(s) => (s, Loaded::Ok),
            Err(e) => (SpendState::default(), Loaded::Ignored(e)),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (SpendState::default(), Loaded::Missing),
        Err(e) => (SpendState::default(), Loaded::Ignored(e.to_string())),
    }
}

/// The file's text (pretty, one trailing newline).
pub fn to_json(state: &SpendState) -> String {
    let mut s = serde_json::to_string_pretty(state).unwrap_or_default();
    s.push('\n');
    s
}

/// Record into the state file at `path`: under an exclusive `flock` on the state directory
/// (the lock balance and the health cache take), reload, apply `f`, and save atomically
/// (0600) when anything changed. A file that had to be ignored is first moved aside
/// (`spend.json.ignored-<unix time>`), never silently overwritten: a week's ledger is worth
/// a look. `Ok` carries the state and, when the old file was ignored, a warning.
pub fn record(path: &Path, now: u64, f: impl FnOnce(SpendState) -> SpendState) -> std::io::Result<(SpendState, Option<String>)> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir)?;
    let lock = std::fs::File::open(dir).ok().filter(|f| f.lock().is_ok());
    let (state, loaded) = load(path);
    let mut warning = None;
    if let Loaded::Ignored(why) = &loaded {
        let aside = path.with_file_name(format!("{SPEND_FILE}.ignored-{now}"));
        std::fs::rename(path, &aside)?;
        warning = Some(format!(
            "warning: the spend ledger {} couldn't be used ({why}) — moved to {} and started afresh",
            path.display(),
            aside.display()
        ));
    }
    let before = state.clone();
    let next = f(state);
    if next != before || loaded != Loaded::Ok {
        crate::snapshot::write_atomic(path, to_json(&next).as_bytes(), 0o600)?;
    }
    drop(lock);
    Ok((next, warning))
}

/// [`record`] for an inline surface: no path, a failed write, a moved-aside file — all
/// silent (only `arena spend` speaks).
pub fn record_quietly(path: Option<&Path>, now: u64, f: impl FnOnce(SpendState) -> SpendState) {
    if let Some(p) = path {
        let _ = record(p, now, f);
    }
}

/// Record a listing ([`observe`]) quietly: what `pods list`, the dashboard, teardown, `arena
/// balance` and the snapshot cron line do after they listed.
pub fn record_listing(path: Option<&Path>, listing: &Listing, pods: &[Pod], naming: &Naming, now: u64) {
    record_quietly(path, now, |s| observe(s, listing, pods, naming, now));
}

/// Record a complete key listing ([`observe_keys`]) quietly.
pub fn record_keys(path: Option<&Path>, keys: &[KeyInfo], naming: &Naming, now: u64) {
    record_quietly(path, now, |s| observe_keys(s, keys, naming, now));
}

// ---------------------------------------------------------------------------------------
// The reads `arena spend` makes besides the listings
// ---------------------------------------------------------------------------------------

/// What `arena spend` reads from the providers' billing, behind a seam: [`HttpSpendApi`] for
/// real, a fake in tests. Each is `None` when its key isn't configured (not asked).
#[async_trait::async_trait]
pub trait SpendApi: Send + Sync {
    /// RunPod's billing per pod and hour from `from` to `to`.
    async fn pod_billing(&self, from: u64, to: u64, now: u64) -> Option<Result<PodBilled, String>>;
    /// RunPod's billing for the whole account, hourly (what balance reads).
    async fn account_billing(&self, from: u64, to: u64) -> Option<Result<Billed, String>>;
    /// The Hetzner account's currency (`GET /pricing`).
    async fn hetzner_currency(&self) -> Option<Result<String, String>>;
}

/// The real reads: two RunPod REST v2 GETs and Hetzner's `GET /pricing`, keys in the header
/// only, to fixed hosts.
pub struct HttpSpendApi {
    client: reqwest::Client,
    runpod_key: Option<String>,
    v2_base: String,
    hetzner: Option<crate::provider::hetzner::HetznerProvider>,
}

impl HttpSpendApi {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let key = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
        HttpSpendApi {
            client: reqwest::Client::new(),
            runpod_key: key("RUNPOD_API_KEY"),
            v2_base: crate::provider::runpod_v2::BASE.to_string(),
            hetzner: key("HETZNER_API_KEY").map(|k| {
                crate::provider::hetzner::HetznerProvider::new(k, crate::provider::hetzner::HetznerOpts::from_config(cfg))
            }),
        }
    }

    /// The same reads against a loopback server, RunPod's only.
    #[cfg(test)]
    pub(crate) fn with_runpod(base: &str, key: &str) -> Self {
        HttpSpendApi {
            client: crate::http::test_server::client(),
            runpod_key: Some(key.into()),
            v2_base: base.into(),
            hetzner: None,
        }
    }

    async fn get(&self, rb: reqwest::RequestBuilder) -> Result<Value, String> {
        match crate::http::send_text_with(rb, &crate::http::Limits::RUNPOD).await {
            Ok((status, text)) => balance::classify(status.as_u16(), &text).map_err(|f| {
                let mut s = f.kind.label().to_string();
                if let Some(h) = f.http {
                    s.push_str(&format!(", HTTP {h}"));
                }
                if let Some(d) = f.detail {
                    s.push_str(&format!(": {d}"));
                }
                s
            }),
            Err(_) => Err("no answer (network error, or its quota is held)".into()),
        }
    }
}

#[async_trait::async_trait]
impl SpendApi for HttpSpendApi {
    async fn pod_billing(&self, from: u64, to: u64, now: u64) -> Option<Result<PodBilled, String>> {
        let key = self.runpod_key.as_deref()?;
        let rb = crate::provider::runpod_v2::billing_pods_request(&self.client, &self.v2_base, key, from, to);
        Some(self.get(rb).await.and_then(|v| parse_pod_billing(&v, now)))
    }

    async fn account_billing(&self, from: u64, to: u64) -> Option<Result<Billed, String>> {
        let key = self.runpod_key.as_deref()?;
        let rb = crate::provider::runpod_v2::billing_request(&self.client, &self.v2_base, key, from, to);
        Some(self.get(rb).await.and_then(|v| balance::parse_billing(&v).map_err(|f| f.detail.unwrap_or_else(|| f.kind.label().into()))))
    }

    async fn hetzner_currency(&self) -> Option<Result<String, String>> {
        use crate::image::cloud::ImageCloud;
        let h = self.hetzner.as_ref()?;
        Some(h.image_price().await.map(|p| p.currency).map_err(|e| e.to_string()))
    }
}

/// A fake for the CLI's tests: canned answers, no network.
#[cfg(any(test, feature = "test-util"))]
#[derive(Default)]
pub struct FakeSpendApi {
    pub pods: Option<Result<PodBilled, String>>,
    pub account: Option<Result<Billed, String>>,
    pub currency: Option<Result<String, String>>,
}

#[cfg(any(test, feature = "test-util"))]
#[async_trait::async_trait]
impl SpendApi for FakeSpendApi {
    async fn pod_billing(&self, _from: u64, _to: u64, _now: u64) -> Option<Result<PodBilled, String>> {
        self.pods.clone()
    }
    async fn account_billing(&self, _from: u64, _to: u64) -> Option<Result<Billed, String>> {
        self.account.clone()
    }
    async fn hetzner_currency(&self) -> Option<Result<String, String>> {
        self.currency.clone()
    }
}

#[cfg(test)]
#[path = "spend_tests.rs"]
mod tests;
