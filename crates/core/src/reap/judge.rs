//! The reaper's decisions — all pure: classification of a probe ([`classify`],
//! [`quiet_now`]), the idle streak and the due rules, the decision table (spec §7, one
//! [`Row`] per outcome), the act phase's verdicts, and the typed proofs that alone make a
//! [`ReapTicket`] — the only way reap code reaches `terminate`.
//!
//! Every row but [`Row::Reaped`] is "do nothing (and log it)". The order of the checks is the
//! table's: the first that matches decides.

use serde::{Deserialize, Serialize};

use super::probe::{Files, GpuFacts, ProbeError, ProbeFacts, RepoSite, Since, Workspace};
use super::render::NotifyKind;
use super::*;
use crate::provider::Provider;
use crate::selector::Naming;

/// Every outcome of the decision table, as logged (`snake_case`). [`Row::code`] gives its
/// number in the spec (§7: `G1`–`G7` run gates, `R1`–`R40` per pod).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Row {
    // --- run gates ---
    ConfigInvalid,
    AlreadyRunning,
    StateUnreadable,
    ClockUnverified,
    Held,
    Disabled,
    DestUnhealthy,
    // --- per pod, before any SSH ---
    NoPolicy,
    ListingFailed,
    ProviderUnsupported,
    NotOnList,
    DuplicateOrTwin,
    NameMismatch,
    Locked,
    LockUnknown,
    Pinned,
    Terminating,
    NotRunning,
    NoEndpoint,
    Scheduled,
    BackupBusy,
    // --- probe and classification ---
    ProbeFailed,
    Identity,
    Clock,
    Garbled,
    Keep,
    NotCovered,
    Active,
    Unknown,
    // --- due? ---
    StreakShort,
    ExpiryConfirming,
    ExpiryWaitQuiet,
    BackoffAfterFailure,
    WouldReap,
    Capped,
    Candidate,
    // --- acting ---
    ReprobeNotIdle,
    GitFailed,
    RsyncFailed,
    VerifyPending,
    PostBackupNotQuiet,
    Rehearsed,
    RelistFailed,
    GoneBeforeReap,
    Reaped,
    TerminateRefusedLocked,
    TerminateFailed,
    // --- reconciling (§8) ---
    Pruned,
    Interrupted,
    RecoveredReaped,
    StillBilling,
}

impl Row {
    /// The spec's number for the row (§7 / §8).
    pub fn code(self) -> &'static str {
        use Row::*;
        match self {
            ConfigInvalid => "G1",
            AlreadyRunning => "G2",
            StateUnreadable => "G3",
            ClockUnverified => "G4",
            Held => "G5",
            Disabled => "G6",
            DestUnhealthy => "G7",
            NoPolicy => "R1",
            ListingFailed => "R2",
            ProviderUnsupported => "R3",
            NotOnList => "R4",
            DuplicateOrTwin => "R5",
            NameMismatch => "R6",
            Locked => "R7",
            LockUnknown => "R8",
            Pinned => "R9",
            Terminating => "R10",
            NotRunning => "R11",
            NoEndpoint => "R12",
            Scheduled => "R13",
            BackupBusy => "R14",
            ProbeFailed => "R15",
            Identity => "R16",
            Clock => "R17",
            Garbled => "R18",
            Keep => "R19",
            NotCovered => "R20",
            Active => "R21",
            Unknown => "R22",
            StreakShort => "R24",
            ExpiryConfirming => "R25",
            ExpiryWaitQuiet => "R26",
            BackoffAfterFailure => "R27",
            WouldReap => "R28",
            Capped => "R29",
            Candidate => "R23",
            ReprobeNotIdle => "R30",
            GitFailed => "R32",
            RsyncFailed => "R33",
            VerifyPending => "R34",
            PostBackupNotQuiet => "R35",
            Rehearsed => "R36",
            RelistFailed => "R37",
            GoneBeforeReap => "R37",
            Reaped => "R38",
            TerminateRefusedLocked => "R39",
            TerminateFailed => "R40",
            Pruned => "F5",
            Interrupted => "F1",
            RecoveredReaped => "F2",
            StillBilling => "F3",
        }
    }

    /// The word (the serde name).
    pub fn word(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------
// Before any SSH (R1–R13)
// ---------------------------------------------------------------------------------------

/// What the listing says about a policy's pod.
#[derive(Debug, Clone, Copy)]
pub enum Listed<'a> {
    /// Its provider's listing failed or timed out.
    Failed,
    /// Its provider wasn't queried (not configured).
    NotQueried,
    /// Its provider listed fine, without it: gone.
    Gone,
    Pod(&'a Pod),
}

/// The run's context for [`pre_ssh_gate`].
pub struct GateCtx<'a> {
    pub naming: &'a Naming<'a>,
    /// `RUNPOD_API=v2`: RunPod reports its lock (on v1 it's unknown — no RunPod pod is reapable).
    pub runpod_v2: bool,
    pub pins: &'a [Pin],
    /// Every pod of every provider that listed fine (for the duplicate / twin check).
    pub listed: &'a [Pod],
    pub now: u64,
}

/// Whether reaping supports `pod`'s provider (R3): RunPod on REST v2, Hetzner. Never Vast (no
/// lock state, no verified identity) or RunPod on v1.
pub fn provider_supported(pod: &Pod, runpod_v2: bool) -> bool {
    match pod.provider.as_str() {
        "runpod" => runpod_v2,
        "hetzner" => true,
        _ => false,
    }
}

/// Rows R1–R13: whether a policy's pod may be probed at all. `Ok(pod)` = probe it; `Err(row)`
/// = nothing (not even an SSH connection). [`Listed::Gone`] is the caller's (it prunes).
pub fn pre_ssh_gate<'a>(policy: &PodPolicy, listed: Listed<'a>, ctx: &GateCtx) -> Result<&'a Pod, Row> {
    if policy.is_empty() {
        return Err(Row::NoPolicy);
    }
    let pod = match listed {
        Listed::Failed | Listed::NotQueried => return Err(Row::ListingFailed),
        Listed::Gone => return Err(Row::Pruned),
        Listed::Pod(p) => p,
    };
    if !provider_supported(pod, ctx.runpod_v2) {
        return Err(Row::ProviderUnsupported);
    }
    if !ctx.naming.reapable(&pod.name) {
        return Err(Row::NotOnList);
    }
    let twins = [format!("{}-old", pod.name), format!("{}-new", pod.name)];
    let same_name = ctx.listed.iter().filter(|p| p.name == pod.name).count();
    if same_name != 1 || ctx.listed.iter().any(|p| twins.contains(&p.name)) {
        return Err(Row::DuplicateOrTwin);
    }
    if policy.name != pod.name {
        return Err(Row::NameMismatch);
    }
    match lock_state(pod) {
        Some(true) => return Err(Row::Locked),
        None => return Err(Row::LockUnknown),
        Some(false) => {}
    }
    if ctx.pins.iter().any(|p| p.holds(&pod.name, &pod.id, ctx.now)) {
        return Err(Row::Pinned);
    }
    if policy.terminating.is_some() {
        return Err(Row::Terminating);
    }
    if !pod.status.eq_ignore_ascii_case("RUNNING") {
        return Err(Row::NotRunning);
    }
    if pod.ssh_ip.is_none() || pod.ssh_port.is_none() || pod.proxy_only {
        return Err(Row::NoEndpoint);
    }
    if let (true, Some(exp)) = (policy.expiry_only(), policy.expires_at) {
        if ctx.now.saturating_add(EXPIRY_PROBE_LEAD) < exp {
            return Err(Row::Scheduled);
        }
    }
    Ok(pod)
}

/// Where this run's probe of `policy`'s pod scans files from (spec §6.1): the previous
/// observation's end (pod clock) less [`SCAN_MARGIN`] — continuous coverage — when there is a
/// recent one; the whole idle window when this observation could make the pod due (the
/// deciding look re-checks all of it); else [`FIRST_SCAN_BACK`].
pub fn plan_since(policy: &PodPolicy, now: u64) -> Since {
    if let Some(w) = policy.idle_after_secs {
        if let Some(s) = idle_streak(&policy.observations) {
            if now.saturating_sub(s.first_t) >= w && s.count + 1 >= min_obs(w) {
                return Since::Back(w + SCAN_MARGIN);
            }
        }
    }
    match policy.observations.last() {
        Some(prev) if now.saturating_sub(prev.t) <= MAX_GAP && prev.t <= now => match prev.pod_t {
            Some(pt) => Since::At(pt.saturating_sub(SCAN_MARGIN)),
            None => Since::Back(FIRST_SCAN_BACK),
        },
        _ => Since::Back(FIRST_SCAN_BACK),
    }
}

// ---------------------------------------------------------------------------------------
// Classification (R15–R23)
// ---------------------------------------------------------------------------------------

/// The jobs listing's outcome (the second exec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobsOutcome {
    /// Read: this many running or starting.
    Read { busy: u32 },
    /// The exec failed, or the reply didn't parse.
    Failed,
}

/// What [`classify`] needs besides the facts.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyCtx<'a> {
    /// The pod's provider (a GPU is required on RunPod).
    pub provider: &'a str,
    /// The listed id the pod must report.
    pub listed_id: &'a str,
    /// Control clock when the probe answered.
    pub now: u64,
    pub clock_ok: bool,
}

/// An observation with no facts (probe failed or garbled, identity or clock wrong).
fn unknown_obs(ctx: &ClassifyCtx, reasons: Vec<Reason>, container: Option<String>, pod_t: Option<u64>) -> Observation {
    Observation {
        t: ctx.now,
        clock_ok: ctx.clock_ok,
        container,
        pod_t,
        verdict: Verdict::Unknown,
        reasons,
        cpu_usec: None,
        net_bytes: None,
        counters_t: None,
        scan_from: None,
    }
}

/// The checks that make an observation's facts usable at all: the probe parsed, the pod is
/// the listed one (identity), the container is known, and its clock agrees with ours.
/// `Err(reason)` otherwise.
fn usable(facts: &Result<ProbeFacts, ProbeError>, ctx: &ClassifyCtx) -> Result<(), Reason> {
    let f = match facts {
        Err(ProbeError::Failed) => return Err(Reason::ProbeFailed),
        Err(ProbeError::Garbled) => return Err(Reason::Garbled),
        Ok(f) => f,
    };
    if f.id.is_empty() || f.id != ctx.listed_id {
        return Err(Reason::Identity);
    }
    if f.container().is_none() {
        return Err(Reason::ContainerUnknown);
    }
    if f.now.abs_diff(ctx.now) > POD_SKEW_MAX {
        return Err(Reason::Clock);
    }
    Ok(())
}

/// The point-in-time signals: everything but the cumulative counters (spec §6.2 `quiet_now`
/// uses exactly these). `(activity, unknown)`.
fn point_signals(f: &ProbeFacts, jobs: JobsOutcome, provider: &str) -> (Vec<Reason>, Vec<Reason>) {
    let (mut act, mut unk) = (Vec::new(), Vec::new());
    if f.keep {
        act.push(Reason::Keep);
    }
    match f.workspace {
        Workspace::HasData => act.push(Reason::Workspace),
        Workspace::Err => unk.push(Reason::WorkspaceUnknown),
        Workspace::Absent | Workspace::Clean => {}
    }
    match f.repo {
        RepoSite::OutsideHome => act.push(Reason::RepoOutside),
        RepoSite::Err => unk.push(Reason::RepoUnknown),
        RepoSite::UnderHome | RepoSite::Absent => {}
    }
    match f.home_mounts {
        Some(0) => {}
        _ => unk.push(Reason::HomeMounts),
    }
    match f.sessions {
        Some(0) => {}
        Some(_) => act.push(Reason::Session),
        None => unk.push(Reason::SessionsUnknown),
    }
    match f.who {
        Some(0) => {}
        Some(_) => act.push(Reason::Who),
        None => unk.push(Reason::WhoUnknown),
    }
    match f.sockets {
        Some(s) => {
            if s.inbound > 0 {
                act.push(Reason::Inbound);
            }
            if s.outbound > 0 {
                act.push(Reason::Outbound);
            }
            if s.overflow {
                act.push(Reason::ManySockets);
            }
        }
        None => unk.push(Reason::TcpUnknown),
    }
    // The window: ~10 s on the pod's clock.
    let win = f.window();
    if !(8..=30).contains(&win) {
        unk.push(Reason::WindowBad);
    } else {
        match (f.cpu.start, f.cpu.end) {
            (Some(a), Some(b)) if b >= a => {
                if (b - a) / win > CPU_WIN_MILLICORES * 1000 {
                    act.push(Reason::Cpu);
                }
            }
            (Some(_), Some(_)) => unk.push(Reason::CounterReset),
            _ => unk.push(Reason::CpuUnknown),
        }
        match (f.net.start, f.net.end) {
            (Some(a), Some(b)) if b >= a => {
                if (b - a) / win > NET_BYTES_PER_SEC {
                    act.push(Reason::Net);
                }
            }
            (Some(_), Some(_)) => unk.push(Reason::CounterReset),
            _ => unk.push(Reason::NetUnknown),
        }
    }
    match f.gpu {
        GpuFacts::Read { max_util, max_mem_mib, .. } => {
            if max_util > GPU_UTIL_PCT {
                act.push(Reason::GpuUtil);
            }
            if max_mem_mib > GPU_MEM_MIB {
                act.push(Reason::GpuMem);
            }
            match f.gpu_procs {
                Some(0) => {}
                Some(_) => act.push(Reason::GpuProcs),
                None => unk.push(Reason::GpuUnknown),
            }
        }
        // A RunPod pod always has a GPU: no reading is "can't tell". A Hetzner VM has none.
        GpuFacts::Absent if provider == "hetzner" => {}
        GpuFacts::Absent | GpuFacts::Unreadable => unk.push(Reason::GpuUnknown),
    }
    match f.tmux_busy {
        Some(0) => {}
        Some(_) => act.push(Reason::Tmux),
        None => unk.push(Reason::TmuxUnknown),
    }
    match f.screen {
        Some(0) => {}
        Some(_) => act.push(Reason::Screen),
        None => unk.push(Reason::ScreenUnknown),
    }
    match f.files {
        Files::Hit => act.push(Reason::Files),
        Files::NoneChanged => {}
        Files::Timeout => unk.push(Reason::FilesTimeout),
        Files::Err => unk.push(Reason::FilesError),
    }
    match jobs {
        JobsOutcome::Read { busy: 0 } => {}
        JobsOutcome::Read { .. } => act.push(Reason::Job),
        JobsOutcome::Failed => unk.push(Reason::JobsUnknown),
    }
    (act, unk)
}

/// Classify one probe (spec §6.2) against the previous observation `prev` of the pod:
/// `Unknown` if the probe failed or is garbled, is about another pod, its container or clock
/// is off, or any applicable signal is unknown (including no baseline for the cumulative
/// counters); `Active` if any signal fires, or a veto (keep, not covered); `Idle` only when
/// every applicable signal is present and under its threshold. Vetoes are `Active` so the
/// streak resets. Counters are only kept when they can be trusted as the next baseline.
pub fn classify(prev: Option<&Observation>, facts: &Result<ProbeFacts, ProbeError>, jobs: JobsOutcome, ctx: &ClassifyCtx) -> Observation {
    if let Err(r) = usable(facts, ctx) {
        let (container, pod_t) = match facts {
            Ok(f) if r != Reason::Identity => (f.container(), Some(f.now)),
            _ => (None, None),
        };
        return unknown_obs(ctx, vec![r], container, pod_t);
    }
    let Ok(f) = facts else { unreachable!("usable() passed") };
    let container = f.container();
    // Drift: Δ pod clock vs Δ control clock since the previous look at this container.
    let same = prev.filter(|p| p.container == container && p.clock_ok && p.t <= ctx.now && ctx.now - p.t <= MAX_GAP);
    if let Some(p) = same {
        if let Some(pt) = p.pod_t {
            let pod_d = f.now as i128 - pt as i128;
            let ctl_d = ctx.now as i128 - p.t as i128;
            if (pod_d - ctl_d).unsigned_abs() > u128::from(DRIFT_MAX) {
                return unknown_obs(ctx, vec![Reason::Clock], container, Some(f.now));
            }
        }
    }
    let (mut act, mut unk) = point_signals(f, jobs, ctx.provider);
    // Cumulative counters since the previous observation of the same container.
    match (prev, same) {
        (Some(p), None) if p.container.is_some() && p.container != container => unk.push(Reason::ContainerChanged),
        (_, Some(p)) if p.cpu_usec.is_some() && p.counters_t.is_some() => {
            let (pt, cpu_then, net_then) = (p.counters_t.unwrap_or(0), p.cpu_usec.unwrap_or(0), p.net_bytes);
            let dt = f.t1.saturating_sub(pt);
            if f.t1 <= pt || dt == 0 {
                unk.push(Reason::Clock);
            } else {
                match f.cpu.end {
                    Some(c) if c >= cpu_then => {
                        if (c - cpu_then) / dt > CPU_CUM_MILLICORES * 1000 {
                            act.push(Reason::Cpu);
                        }
                    }
                    Some(_) => unk.push(Reason::CounterReset),
                    None => unk.push(Reason::CpuUnknown),
                }
                match (f.net.end, net_then) {
                    (Some(n), Some(then)) if n >= then => {
                        if (n - then) / dt > NET_BYTES_PER_SEC {
                            act.push(Reason::Net);
                        }
                    }
                    (Some(_), Some(_)) => unk.push(Reason::CounterReset),
                    _ => unk.push(Reason::NoBaseline),
                }
            }
        }
        _ => unk.push(Reason::NoBaseline),
    }
    if !ctx.clock_ok {
        unk.push(Reason::Clock);
    }
    act.sort();
    act.dedup();
    unk.sort();
    unk.dedup();
    let verdict = if !act.is_empty() {
        Verdict::Active
    } else if !unk.is_empty() {
        Verdict::Unknown
    } else {
        Verdict::Idle
    };
    let reasons = if act.is_empty() { unk } else { act };
    Observation {
        t: ctx.now,
        clock_ok: ctx.clock_ok,
        container,
        pod_t: Some(f.now),
        verdict,
        reasons,
        cpu_usec: f.cpu.end,
        net_bytes: f.net.end,
        counters_t: Some(f.t1),
        scan_from: Some(f.scan_from),
    }
}

// ---------------------------------------------------------------------------------------
// Streak and due (spec §6.3)
// ---------------------------------------------------------------------------------------

/// The run of `Idle` observations at the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Streak {
    pub first_t: u64,
    pub last_t: u64,
    pub count: usize,
}

impl Streak {
    pub fn span(&self) -> u64 {
        self.last_t - self.first_t
    }
}

/// The idle streak at the tail of `obs`: consecutive `Idle` observations, each with a verified
/// clock and the same container, neighbours at most [`MAX_GAP`] apart (control clock), each
/// one's file scan starting no later than the previous one ended (pod clock) — so nothing
/// between them went unwatched. `None` when the last observation isn't such an `Idle`.
pub fn idle_streak(obs: &[Observation]) -> Option<Streak> {
    let ok = |o: &Observation| o.verdict == Verdict::Idle && o.clock_ok && o.container.is_some();
    let last = obs.last().filter(|o| ok(o))?;
    let mut first = last;
    let mut count = 1;
    for pair in obs.windows(2).rev() {
        let (prev, cur) = (&pair[0], &pair[1]);
        if !std::ptr::eq(cur, first) {
            break;
        }
        let covered = match (cur.scan_from, prev.pod_t) {
            (Some(from), Some(end)) => from <= end,
            _ => false,
        };
        if !ok(prev) || prev.container != last.container || prev.t >= cur.t || cur.t - prev.t > MAX_GAP || !covered {
            break;
        }
        first = prev;
        count += 1;
    }
    Some(Streak { first_t: first.t, last_t: last.t, count })
}

/// Idle is due: an idle window `W` is set, the streak spans at least `W` with at least
/// [`min_obs`]`(W)` observations, and the deciding (last) observation scanned files over the
/// whole window.
pub fn idle_due(policy: &PodPolicy) -> bool {
    let Some(w) = policy.idle_after_secs else { return false };
    let Some(s) = idle_streak(&policy.observations) else { return false };
    let full_scan = policy.observations.last().is_some_and(|o| match (o.pod_t, o.scan_from) {
        (Some(end), Some(from)) => end.saturating_sub(from) >= w,
        _ => false,
    });
    s.span() >= w && s.count >= min_obs(w) && full_scan
}

/// The last [`EXPIRY_QUIET_OBS`] observations are consecutive `Idle` ones.
fn quiet_tail(obs: &[Observation]) -> bool {
    idle_streak(obs).is_some_and(|s| s.count >= EXPIRY_QUIET_OBS)
}

/// Expiry is due: passed, first seen passed at least [`EXPIRY_CONFIRM`] ago by a verified
/// run, and the last [`EXPIRY_QUIET_OBS`] observations are consecutive `Idle` ones.
pub fn expiry_due(policy: &PodPolicy, now: u64) -> bool {
    match (policy.expires_at, policy.first_due_seen) {
        (Some(exp), Some(seen)) => exp <= now && seen <= now && now - seen >= EXPIRY_CONFIRM && quiet_tail(&policy.observations),
        _ => false,
    }
}

/// Overdue: an expiry that has passed.
pub fn overdue(policy: &PodPolicy, now: u64) -> bool {
    policy.expires_at.is_some_and(|e| e <= now)
}

// ---------------------------------------------------------------------------------------
// The decision (R15–R29)
// ---------------------------------------------------------------------------------------

/// What the run may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Look and log (`pods reap run`, or `--apply` while held / disabled / clock unverified).
    Observe,
    /// Everything up to and including the real backup and its checks; never a terminate.
    Rehearse,
    /// Act.
    Apply,
}

/// One pod's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub row: Row,
    pub reasons: Vec<Reason>,
    /// Send this notification (rate-limited by the caller).
    pub notify: Option<NotifyKind>,
    /// For [`Row::Candidate`] / [`Row::WouldReap`]: why it's due.
    pub trigger: Option<Trigger>,
}

impl Decision {
    pub fn of(row: Row) -> Decision {
        Decision { row, reasons: Vec::new(), notify: None, trigger: None }
    }
}

/// The row for an observation that isn't `Idle` (R15–R22), and the notification an overdue
/// pod gets for it (decision Q1: deferred indefinitely, notified at most every 6 h).
pub fn observation_row(obs: &Observation, overdue: bool) -> Decision {
    let has = |r: Reason| obs.reasons.contains(&r);
    let row = match obs.verdict {
        Verdict::Idle => return Decision::of(Row::Candidate),
        Verdict::Active if has(Reason::Keep) => Row::Keep,
        Verdict::Active if has(Reason::Workspace) || has(Reason::RepoOutside) => Row::NotCovered,
        Verdict::Active => Row::Active,
        Verdict::Unknown if has(Reason::ProbeFailed) => Row::ProbeFailed,
        Verdict::Unknown if has(Reason::Identity) => Row::Identity,
        Verdict::Unknown if has(Reason::Garbled) => Row::Garbled,
        Verdict::Unknown if has(Reason::Clock) => Row::Clock,
        Verdict::Unknown => Row::Unknown,
    };
    let notify = overdue.then(|| if obs.verdict == Verdict::Active { NotifyKind::OverdueInUse } else { NotifyKind::OverdueUnreachable });
    Decision { row, reasons: obs.reasons.clone(), notify, trigger: None }
}

/// The due rows (R24–R28) for a pod whose latest observation is `Idle` (already appended to
/// `policy.observations`, with `first_due_seen` updated). Caps (R29) are the run's. Expiry
/// wins over idle when both are due.
pub fn due_row(policy: &PodPolicy, now: u64, mode: Mode) -> Decision {
    let trigger = if expiry_due(policy, now) {
        Some(Trigger::Expiry)
    } else if idle_due(policy) {
        Some(Trigger::Idle)
    } else {
        None
    };
    let Some(trigger) = trigger else {
        let row = match (policy.expires_at, policy.first_due_seen) {
            (Some(e), None) if e <= now => Row::ExpiryConfirming,
            (Some(e), Some(seen)) if e <= now && now.saturating_sub(seen) < EXPIRY_CONFIRM => Row::ExpiryConfirming,
            (Some(e), Some(_)) if e <= now => Row::ExpiryWaitQuiet,
            _ if policy.idle_after_secs.is_some() => Row::StreakShort,
            _ => Row::Scheduled,
        };
        return Decision::of(row);
    };
    if policy.retry_after.is_some_and(|r| r > now) {
        return Decision { row: Row::BackoffAfterFailure, reasons: Vec::new(), notify: None, trigger: Some(trigger) };
    }
    let row = if mode == Mode::Observe { Row::WouldReap } else { Row::Candidate };
    Decision { row, reasons: Vec::new(), notify: None, trigger: Some(trigger) }
}

/// The candidates' order: overdue expiries first (the oldest expiry first), then idle ones
/// (the longest streak first). `(key, trigger, expires_at, streak span)`.
pub fn order_candidates(mut c: Vec<(String, Trigger, Option<u64>, u64)>) -> Vec<String> {
    c.sort_by(|a, b| match (a.1, b.1) {
        (Trigger::Expiry, Trigger::Idle) => std::cmp::Ordering::Less,
        (Trigger::Idle, Trigger::Expiry) => std::cmp::Ordering::Greater,
        (Trigger::Expiry, Trigger::Expiry) => a.2.cmp(&b.2).then(a.0.cmp(&b.0)),
        (Trigger::Idle, Trigger::Idle) => b.3.cmp(&a.3).then(a.0.cmp(&b.0)),
    });
    c.into_iter().map(|(k, ..)| k).collect()
}

/// How many terminate attempts this run may still make: the run cap (`--max` can only lower
/// it) less what it used, and what the rolling day has left.
pub fn caps_left(settings: &ReapSettings, max_flag: Option<u32>, used_this_run: u32, attempts_today: usize) -> u32 {
    let per_run = max_flag.map_or(settings.max_per_run, |m| m.min(settings.max_per_run));
    let run_left = per_run.saturating_sub(used_this_run);
    let day_left = (settings.max_per_day as usize).saturating_sub(attempts_today) as u32;
    run_left.min(day_left)
}

// ---------------------------------------------------------------------------------------
// The act phase: verdicts and proofs
// ---------------------------------------------------------------------------------------

/// How a backup step's process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepEnd {
    /// It exited with this code (and printed this).
    Exit { code: i32, stdout: String },
    /// Killed by a signal.
    Killed,
    /// Ran out of its budget (stopped).
    Timeout,
    /// Couldn't be started, or the call itself failed.
    Error,
}

impl StepEnd {
    fn ok(&self) -> bool {
        matches!(self, StepEnd::Exit { code: 0, .. })
    }
}

/// The git step's verdict (R32): exit 0 with a sentinel — `PUSHED`/`NO_CHANGES` (in sync with
/// upstream after P1) or `SKIP` (main/master: the rsync tiers carry `.git`). `Ok(branch)`.
/// `None` = the step was skipped because the pod has no repo (the probe said so): the tiers
/// carry the whole home.
pub fn git_verdict(git: Option<&StepEnd>) -> Result<Option<String>, Row> {
    let Some(end) = git else { return Ok(None) };
    match end {
        StepEnd::Exit { code: 0, stdout } => match crate::backup::parse_backup_output(stdout) {
            Some((_, branch)) => Ok(Some(branch)),
            None => Err(Row::GitFailed),
        },
        _ => Err(Row::GitFailed),
    }
}

/// The verify's verdict (R34): the big tier's dry run exited 0, printed its stats, and would
/// send nothing. `Err(None)` = it failed or said nothing; `Err(Some(n))` = `n` files pending.
pub fn verify_verdict(end: &StepEnd) -> Result<(), Option<u64>> {
    let StepEnd::Exit { code: 0, stdout } = end else { return Err(None) };
    match crate::pull::parse_rsync_stats(stdout) {
        Some(s) if s.files_sent == 0 => Ok(()),
        Some(s) => Err(Some(s.files_sent)),
        None => Err(None),
    }
}

/// Everything a reap backup did, as the CLI recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEvidence {
    pub key: String,
    /// The container the deciding observation saw.
    pub container: String,
    /// Control clock: when the backup started and finished.
    pub started: u64,
    pub finished: u64,
    /// The per-pod backup lock was taken (non-blocking).
    pub lock_held: bool,
    /// Both tier roots were absolute, marked right, and had the free-space floor, just before.
    pub dest_ok: bool,
    /// The git step (`None` = skipped: no repo on the pod).
    pub git: Option<StepEnd>,
    /// The snapshot tier's passes (files, then `.git`).
    pub snapshot: Vec<StepEnd>,
    /// The big tier.
    pub big: StepEnd,
    /// The big tier's dry-run verify.
    pub verify: StepEnd,
    pub snapshot_dir: String,
    pub big_dir: String,
}

/// A verified backup (spec I11): constructed only by [`BackupProof::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupProof {
    key: String,
    container: String,
    started: u64,
    finished: u64,
    pub snapshot_dir: String,
    pub big_dir: String,
    pub branch: Option<String>,
}

/// A backup that didn't verify: the row, and for a verify the files still pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFailure {
    pub row: Row,
    pub pending: Option<u64>,
}

impl BackupProof {
    /// Rows R31–R34 over the evidence, in order: the lock, the destination, git (exit 0 and a
    /// sentinel), every rsync pass exit 0 (23, 24, 255, a timeout: no), the verify (exit 0,
    /// stats, nothing pending).
    pub fn check(ev: &BackupEvidence) -> Result<BackupProof, BackupFailure> {
        let fail = |row| BackupFailure { row, pending: None };
        if !ev.lock_held {
            return Err(fail(Row::BackupBusy));
        }
        if !ev.dest_ok {
            return Err(fail(Row::DestUnhealthy));
        }
        let branch = git_verdict(ev.git.as_ref()).map_err(fail)?;
        if ev.snapshot.is_empty() || !ev.snapshot.iter().all(StepEnd::ok) || !ev.big.ok() {
            return Err(fail(Row::RsyncFailed));
        }
        verify_verdict(&ev.verify).map_err(|pending| BackupFailure { row: Row::VerifyPending, pending })?;
        if ev.finished < ev.started || ev.container.is_empty() {
            return Err(fail(Row::RsyncFailed));
        }
        Ok(BackupProof {
            key: ev.key.clone(),
            container: ev.container.clone(),
            started: ev.started,
            finished: ev.finished,
            snapshot_dir: ev.snapshot_dir.clone(),
            big_dir: ev.big_dir.clone(),
            branch,
        })
    }

    pub fn started(&self) -> u64 {
        self.started
    }
}

/// Why a reap is due — constructed only by [`TriggerProof::from_decision`], from a
/// [`Row::Candidate`] decision on a fresh `Idle` observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerProof {
    key: String,
    provider: String,
    id: String,
    name: String,
    trigger: Trigger,
    /// Control clock of the deciding observation.
    decided_at: u64,
    container: String,
}

impl TriggerProof {
    /// From a candidate decision on `policy`, whose last observation (the deciding one) is
    /// `Idle` and at most [`OBS_FRESH`] old at `now` (R30: an older one is re-probed first).
    pub fn from_decision(policy: &PodPolicy, d: &Decision, now: u64) -> Result<TriggerProof, Row> {
        let (Row::Candidate, Some(trigger)) = (d.row, d.trigger) else { return Err(Row::StreakShort) };
        let obs = policy.observations.last().ok_or(Row::ReprobeNotIdle)?;
        if obs.verdict != Verdict::Idle || !obs.clock_ok || obs.t > now || now - obs.t > OBS_FRESH {
            return Err(Row::ReprobeNotIdle);
        }
        let due = match trigger {
            Trigger::Expiry => expiry_due(policy, now),
            Trigger::Idle => idle_due(policy),
        };
        if !due {
            return Err(Row::ReprobeNotIdle);
        }
        Ok(TriggerProof {
            key: policy.key(),
            provider: policy.provider.clone(),
            id: policy.id.clone(),
            name: policy.name.clone(),
            trigger,
            decided_at: obs.t,
            container: obs.container.clone().ok_or(Row::ReprobeNotIdle)?,
        })
    }

    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// The container the deciding observation saw (what the backup and the post-backup
    /// probe must find again).
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Control clock of the deciding observation.
    pub fn decided_at(&self) -> u64 {
        self.decided_at
    }
}

/// What the post-backup probe must find (spec §6.2 `quiet_now`).
#[derive(Debug, Clone, Copy)]
pub struct QuietCtx<'a> {
    pub provider: &'a str,
    pub id: &'a str,
    pub container: &'a str,
    /// Control clock when the probe answered.
    pub now: u64,
}

/// The pod quiet right after the backup — constructed only by [`quiet_now`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietProof {
    container: String,
    at: u64,
}

/// The post-backup check (R35): the same pod (id) and container as before the backup, its
/// clock in step, and only point-in-time signals — sessions, logins, sockets, the CPU and
/// network over the probe's own window, GPU, tmux, screen, jobs, the keep marker, the
/// backup's scope — plus no file changed since the backup started (the scan's `since`). The
/// cumulative counters are NOT used: they include the backup's own traffic. `Err` lists why.
pub fn quiet_now(facts: &Result<ProbeFacts, ProbeError>, jobs: JobsOutcome, ctx: &QuietCtx) -> Result<QuietProof, Vec<Reason>> {
    let cctx = ClassifyCtx { provider: ctx.provider, listed_id: ctx.id, now: ctx.now, clock_ok: true };
    usable(facts, &cctx).map_err(|r| vec![r])?;
    let Ok(f) = facts else { return Err(vec![Reason::ProbeFailed]) };
    if f.container().as_deref() != Some(ctx.container) {
        return Err(vec![Reason::ContainerChanged]);
    }
    let (mut act, unk) = point_signals(f, jobs, ctx.provider);
    act.extend(unk);
    if !act.is_empty() {
        act.sort();
        act.dedup();
        return Err(act);
    }
    Ok(QuietProof { container: ctx.container.to_string(), at: ctx.now })
}

/// The re-listing just before the terminate — constructed only by [`relist_verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelistProof {
    key: String,
    at: u64,
}

/// R37 over the re-listing of the pod's provider (`listed_at`, `now`: control clock): fresh
/// (≤ [`RELIST_MAX_AGE`]), the pod there with the same id and name, `RUNNING`, its lock state
/// reported and off. Gone ⇒ [`Row::GoneBeforeReap`]; anything else ⇒ [`Row::RelistFailed`].
pub fn relist_verdict(listed: Listed, expect_key: &str, expect_name: &str, listed_at: u64, now: u64) -> Result<RelistProof, Row> {
    if listed_at > now || now - listed_at > RELIST_MAX_AGE {
        return Err(Row::RelistFailed);
    }
    let pod = match listed {
        Listed::Pod(p) => p,
        Listed::Gone => return Err(Row::GoneBeforeReap),
        Listed::Failed | Listed::NotQueried => return Err(Row::RelistFailed),
    };
    if key(&pod.provider, &pod.id) != expect_key || pod.name != expect_name || !pod.status.eq_ignore_ascii_case("RUNNING") || lock_state(pod) != Some(false)
    {
        return Err(Row::RelistFailed);
    }
    Ok(RelistProof { key: expect_key.to_string(), at: listed_at })
}

/// The state re-read just before the terminate still allows it — constructed only by
/// [`recheck_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckProof {
    key: String,
}

/// R37's state half: re-read (strictly, under its lock) just before the terminate — no hold,
/// no pin on the name or id, the policy still there under the same name, and still due by
/// `trigger` (an extension, a `clear` or a longer idle window since this run decided make it
/// not due). `obs` is the run's view of the pod's observations (this run's included).
pub fn recheck_state(state: &ReapState, k: &str, trigger: Trigger, obs: &[Observation], now: u64) -> Result<RecheckProof, Row> {
    if state.hold.is_some() {
        return Err(Row::Held);
    }
    let p = state.pods.get(k).ok_or(Row::NoPolicy)?;
    if state.pinned(&p.name, &p.id, now) {
        return Err(Row::Pinned);
    }
    let mut view = p.clone();
    view.observations = obs.to_vec();
    let due = match trigger {
        Trigger::Expiry => expiry_due(&view, now),
        Trigger::Idle => idle_due(&view),
    };
    if !due {
        return Err(Row::StreakShort);
    }
    Ok(RecheckProof { key: k.to_string() })
}

/// The run-level gates that must hold to act (G4–G7, I1, I8): apply mode (not rehearse),
/// `REAP_ENABLED=1` from the config file, no hold, a verified clock, a healthy backup
/// destination, and an attempt left under both caps. Constructed only by [`RunGate::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGate(());

/// The run's state for [`RunGate::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunInputs {
    pub mode: Mode,
    pub enabled: bool,
    pub held: bool,
    pub clock_ok: bool,
    pub dest_ok: bool,
    pub caps_left: u32,
}

impl RunGate {
    pub fn check(r: &RunInputs) -> Result<RunGate, Row> {
        if r.mode != Mode::Apply {
            return Err(Row::WouldReap);
        }
        if !r.enabled {
            return Err(Row::Disabled);
        }
        if r.held {
            return Err(Row::Held);
        }
        if !r.clock_ok {
            return Err(Row::ClockUnverified);
        }
        if !r.dest_ok {
            return Err(Row::DestUnhealthy);
        }
        if r.caps_left == 0 {
            return Err(Row::Capped);
        }
        Ok(RunGate(()))
    }
}

/// The durable write-ahead record of this attempt (`terminating` + the attempt counted, saved
/// and fsynced) — constructed only by the store once that save succeeded
/// ([`super::store::write_ahead`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAheadProof {
    key: String,
    at: u64,
}

impl WriteAheadProof {
    pub(crate) fn saved(key: &str, at: u64) -> WriteAheadProof {
        WriteAheadProof { key: key.to_string(), at }
    }
}

/// The ONE way reap code terminates a pod (spec I4): a ticket exists only when every gate
/// passed — the run's ([`RunGate`]), the trigger's, a durable write-ahead, a verified backup
/// after the deciding observation, the pod quiet after it in the same container, a fresh
/// re-listing and the state re-read — all about the same pod. Its constructor is private;
/// [`ReapTicket::terminate`] consumes it.
#[derive(Debug, PartialEq, Eq)]
pub struct ReapTicket {
    provider: String,
    id: String,
    name: String,
    trigger: Trigger,
}

impl ReapTicket {
    /// Issue a ticket from the proofs, checking they agree: one pod (key) throughout, one
    /// container, and in order — decided, written ahead, backed up, found quiet, re-listed.
    pub fn issue(
        _run: RunGate,
        trigger: TriggerProof,
        wal: WriteAheadProof,
        backup: BackupProof,
        quiet: QuietProof,
        relist: RelistProof,
        recheck: RecheckProof,
    ) -> Result<ReapTicket, Row> {
        let k = &trigger.key;
        if [&wal.key, &backup.key, &relist.key, &recheck.key].iter().any(|x| *x != k) {
            return Err(Row::RelistFailed);
        }
        if backup.container != trigger.container || quiet.container != trigger.container {
            return Err(Row::PostBackupNotQuiet);
        }
        // decided ≤ written ahead ≤ backup start ≤ backup end ≤ quiet ≤ re-listed.
        let order = [trigger.decided_at, wal.at, backup.started, backup.finished, quiet.at, relist.at];
        if order.windows(2).any(|w| w[0] > w[1]) {
            return Err(Row::RelistFailed);
        }
        Ok(ReapTicket::new(trigger))
    }

    fn new(t: TriggerProof) -> ReapTicket {
        ReapTicket { provider: t.provider, id: t.id, name: t.name, trigger: t.trigger }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    pub fn key(&self) -> String {
        key(&self.provider, &self.id)
    }

    /// Terminate the ticket's pod — routed by its provider's NAME
    /// ([`Provider::terminate_pod_on`]). Consumes the ticket: one ticket, one call.
    pub async fn terminate(self, provider: &dyn Provider) -> crate::Result<()> {
        provider.terminate_pod_on(&self.provider, &self.id).await
    }
}

// ---------------------------------------------------------------------------------------
// Reconciling with the listing (spec §8 F1–F3, pruning)
// ---------------------------------------------------------------------------------------

/// What reconciling the state with a listing did (each logged; some notified).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub row: Row,
    pub key: String,
    pub name: String,
}

/// Reconcile `state` with the listing (`listing(provider)` = that provider's pods, or `None`
/// when it failed or wasn't queried — then nothing of it changes):
/// - a pending `terminating` whose pod is listed: cleared, `Interrupted` (F1 — a fresh
///   candidate, which needs a new backup); whose pod is gone: a tombstone, `RecoveredReaped`
///   (F2);
/// - a policy whose pod is gone from an OK listing: pruned (records for attempts `up --check`
///   terminated go the same way);
/// - a tombstone whose pod is still listed and billing: counted; the second run that sees it
///   sets an automatic hold (F3 — the provider and we disagree).
pub fn reconcile<'a>(state: &mut ReapState, listing: &dyn Fn(&str) -> Option<&'a [Pod]>, now: u64) -> Vec<Event> {
    let mut events = Vec::new();
    let keys: Vec<String> = state.pods.keys().cloned().collect();
    for k in keys {
        let p = &state.pods[&k];
        let Some(pods) = listing(&p.provider) else { continue };
        let present = pods.iter().any(|x| x.id == p.id);
        let name = p.name.clone();
        match (present, p.terminating.clone()) {
            (true, Some(_)) => {
                if let Some(p) = state.pods.get_mut(&k) {
                    p.terminating = None;
                }
                events.push(Event { row: Row::Interrupted, key: k, name });
            }
            (false, Some(t)) => {
                state.pods.remove(&k);
                state.reaped.insert(
                    k.clone(),
                    Tombstone { at: now, name: name.clone(), trigger: Some(t.trigger), snapshot_dir: None, big_dir: None, branch: None, recovered: true, seen_after: 0 },
                );
                events.push(Event { row: Row::RecoveredReaped, key: k, name });
            }
            (false, None) => {
                state.pods.remove(&k);
                events.push(Event { row: Row::Pruned, key: k, name });
            }
            (true, None) => {}
        }
    }
    let mut hold = None;
    for (k, t) in state.reaped.iter_mut() {
        let provider = k.split_once(':').map_or("", |(p, _)| p);
        let id = k.split_once(':').map_or("", |(_, i)| i);
        let Some(pods) = listing(provider) else { continue };
        if let Some(pod) = pods.iter().find(|x| x.id == id) {
            if crate::status::bills_hourly(&pod.provider, &pod.status) {
                t.seen_after += 1;
                events.push(Event { row: Row::StillBilling, key: k.clone(), name: t.name.clone() });
                if t.seen_after >= 2 {
                    hold = Some(format!("{} ({k}) is still listed and billing two runs after it was terminated", t.name));
                }
            }
        }
    }
    if let (Some(reason), None) = (hold, &state.hold) {
        state.hold = Some(Hold { reason, set_at: now, auto: true });
    }
    events
}

#[cfg(test)]
mod tests;
