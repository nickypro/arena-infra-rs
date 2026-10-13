//! Opt-in auto-reap: terminate a pod that is past an end time the operator set (**expiry**),
//! or that has demonstrably not been used for an operator-chosen window (**idle**) — after a
//! verified backup, and only when every check is certain. Off by default; nothing here acts
//! on a pod nobody gave a policy.
//!
//! **The one rule everything follows: only the control box's per-pod state can *authorise* a
//! reap; anything can *veto* one.** A policy is keyed `provider:id` and carries the pod's
//! name, both checked against the live listing; a missing, lost or unreadable state file
//! carries no authority (nothing is reaped), while a lock, a pin, a keep marker on the pod, a
//! hold, an unreachable pod, an unverified clock, a failed listing, a failed backup or a probe
//! that can't be read each stop it. "Unsure" is always "do nothing and log it".
//!
//! Layout (pure code here and in the submodules; the I/O lives in the CLI's `reap.rs`):
//! - this file: constants, settings (`REAP_*`, file-only — [`crate::config::Config::is_file_only`]),
//!   durations and times, the state file's types (`reap.json`, strict serde);
//! - [`probe`]: the read-only probe script ([`probe::probe_command`]) and its strict parser;
//! - [`judge`]: classification, the idle streak, due rules, the decision table (spec §7,
//!   rows `R1`–`R40` and the run gates `G1`–`G7`), the backup/relist verdicts, the typed
//!   proofs and [`judge::ReapTicket`] — the only way reap code reaches `terminate`;
//! - [`render`]: the `ENDS` column, the plan table, the JSONL log line, the participant
//!   notice and banner, the cron line, the notification text;
//! - [`store`]: `reap.json` load (strict) / save (atomic, fsynced) under its flock, the run
//!   lock, the log file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::pod::Pod;

pub mod judge;
pub mod probe;
pub mod render;
pub mod store;

// ---------------------------------------------------------------------------------------
// Constants (documented, not knobs)
// ---------------------------------------------------------------------------------------

/// The most two neighbouring observations in an idle streak may be apart (control clock). A
/// longer gap — the reaper didn't run, the pod was unreachable for a while, a long reap
/// backup held the run lock — restarts the streak: nothing was watching in between.
pub const MAX_GAP: u64 = 40 * 60;

/// How long after `expires_at` was first seen passed (by a clock-verified run) an expiry may
/// act: one run alone never decides.
pub const EXPIRY_CONFIRM: u64 = 10 * 60;

/// Consecutive `Idle` observations an expiry needs (the pod quiet now, twice).
pub const EXPIRY_QUIET_OBS: usize = 2;

/// An expiry-only pod is first probed this long before its expiry (no SSH before that).
pub const EXPIRY_PROBE_LEAD: u64 = 60 * 60;

/// The most a pod's clock may differ from the control box's before its observation is
/// `Unknown(Clock)`.
pub const POD_SKEW_MAX: u64 = 300;

/// The most (Δ pod clock − Δ control clock) between two observations may be: a pod whose
/// clock jumped can't vouch for "no file changed since".
pub const DRIFT_MAX: u64 = 120;

/// The control clock going back by more than this since the last run makes it unverified.
pub const CLOCK_BACKSTEP: u64 = 120;

/// CPU use (millicores, averaged since the previous observation of the same container) above
/// which a pod is in use: 0.10 cores.
pub const CPU_CUM_MILLICORES: u64 = 100;

/// CPU use over the probe's own 10 s window above which a pod is in use: 0.25 cores.
pub const CPU_WIN_MILLICORES: u64 = 250;

/// Network bytes per second (rx + tx, every interface but `lo`) above which a pod is in use,
/// cumulatively and over the window: 100 KB/s.
pub const NET_BYTES_PER_SEC: u64 = 100_000;

/// A GPU busier than this (%) in any of the probe's samples is in use.
pub const GPU_UTIL_PCT: u32 = 5;

/// GPU memory held above this (MiB, summed over the GPUs) is in use — a notebook kernel with
/// a model loaded at 0 % utilisation is somebody's work (decision Q3). To be confirmed
/// against the recorded fixture of an idle pod.
pub const GPU_MEM_MIB: u64 = 1024;

/// The probe's whole budget (connect + 10 s window + 5 GPU samples + the file scan).
pub const PROBE_BUDGET_SECS: u64 = 90;

/// The file scan's own budget inside the probe.
pub const FILE_SCAN_SECS: u64 = 60;

/// The jobs listing's budget (a second exec).
pub const JOBS_BUDGET_SECS: u64 = 30;

/// A probe reply longer than this is `Garbled` (never parsed).
pub const PROBE_OUTPUT_CAP: usize = 64 * 1024;

/// At most this many socket lines come back; past it the probe says so and that counts as in use.
pub const TCP_LINE_CAP: usize = 500;

/// At most this many sshd process lines come back; past it sessions are unknown.
pub const SSHD_LINE_CAP: usize = 100;

/// Pods probed at once.
pub const PROBE_CONCURRENCY: usize = 8;

/// A deciding observation older than this is taken again before anything acts on it: the
/// deciding look is taken immediately before the backup (RS4), so nothing in between goes
/// unwatched for more than this.
pub const OBS_FRESH: u64 = 60;

/// Processes started within this long of the container's start (PID 1) are the image's own
/// (its boot-time services): they, and later restarts with the same command line, are the
/// pod's process baseline. Anything else running that isn't an idle daemon is somebody's —
/// a dormant `nohup python train.py` included (RS7).
pub const BOOT_GRACE: u64 = 120;

/// The re-listing a terminate rides on may be at most this old.
pub const RELIST_MAX_AGE: u64 = 60;

/// After a failed reap backup, the pod isn't tried again for this long.
pub const BACKUP_FAIL_RETRY: u64 = 6 * 3600;

/// A notification about one pod and one gate is repeated at most this often (decision Q1).
pub const NOTIFY_REPEAT: u64 = 6 * 3600;

/// `reap-log.jsonl` is rotated (to `.1`) past this size.
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Observations kept per pod: those within `W + 2 h` (or the expiry lead), at most this many.
pub const OBS_KEEP_MAX: usize = 1000;

/// The extra time observations are kept beyond the idle window.
pub const OBS_KEEP_EXTRA: u64 = 2 * 3600;

/// Tombstones (what was reaped, and where its backup is) are kept this long.
pub const TOMBSTONE_KEEP: u64 = 30 * 86_400;

/// A first observation (no earlier one of the pod) scans files this far back.
pub const FIRST_SCAN_BACK: u64 = 15 * 60;

/// Each observation's scan starts this long before the previous one's end (pod clock).
pub const SCAN_MARGIN: u64 = 120;

/// The post-backup probe scans files from this long before the backup started.
pub const POST_BACKUP_MARGIN: u64 = 60;

/// `REAP_NOTIFY_CMD` is killed after this long.
pub const NOTIFY_TIMEOUT_SECS: u64 = 10;

/// `--expire-in` / `--extend` / an `--expire-at` time: at least this far ahead…
pub const EXPIRE_MIN: u64 = 10 * 60;
/// …and at most this far.
pub const EXPIRE_MAX: u64 = 60 * 86_400;
/// `--idle-after`: at most this long (the floor is `REAP_IDLE_FLOOR_MINS`).
pub const IDLE_MAX: u64 = 14 * 86_400;

/// The one participant warning (`wall`, apply mode only) goes out this long before a reap.
pub const WARN_LEAD: u64 = 60 * 60;

/// The fewest observations an idle window `w` (seconds) needs: `max(4, ⌈w / 30 min⌉)`.
pub fn min_obs(w: u64) -> usize {
    (w.div_ceil(1800) as usize).max(4)
}

// ---------------------------------------------------------------------------------------
// Settings (REAP_*, file-only)
// ---------------------------------------------------------------------------------------

/// The reaper's settings, from the config FILE (`REAP_*` keys are file-only: the environment
/// can't set them — [`Config::is_file_only`]). Out of range is a config error, never clamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapSettings {
    /// `REAP_ENABLED=1`: `--apply` may act. Anything else (unset, `0`) and `--apply` runs as
    /// observe.
    pub enabled: bool,
    /// `REAP_MAX_PER_RUN` (1..=5, default 1): terminate attempts per run.
    pub max_per_run: u32,
    /// `REAP_MAX_PER_DAY` (1..=50, default 10): terminate attempts per rolling 24 h.
    pub max_per_day: u32,
    /// `REAP_IDLE_FLOOR_MINS` (15..=1440, default 120), in seconds: the smallest `--idle-after`.
    pub idle_floor_secs: u64,
    /// `REAP_MIN_FREE_GB` (1..=100000, default 20), in bytes: free space each backup tier root
    /// must have before a reap backup.
    pub min_free_bytes: u64,
    /// `REAP_NOTIFY_CMD`: an `sh -c` command fed one line per notification on stdin.
    pub notify_cmd: Option<String>,
}

impl Default for ReapSettings {
    fn default() -> Self {
        ReapSettings {
            enabled: false,
            max_per_run: 1,
            max_per_day: 10,
            idle_floor_secs: 120 * 60,
            min_free_bytes: 20 * 1_000_000_000,
            notify_cmd: None,
        }
    }
}

impl ReapSettings {
    /// Read and validate the `REAP_*` keys. `Err` names the key and the rule.
    pub fn from_config(cfg: &Config) -> Result<ReapSettings, String> {
        let mut s = ReapSettings::default();
        let get = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty());
        s.enabled = match get("REAP_ENABLED") {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => return Err(format!("REAP_ENABLED must be 1 (on) or 0 / unset (off) (got `{}`)", clip_value(other))),
        };
        let ranged = |key: &str, lo: u64, hi: u64, default: u64| -> Result<u64, String> {
            match get(key) {
                None => Ok(default),
                Some(v) => v
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (lo..=hi).contains(n))
                    .ok_or_else(|| format!("{key} must be a whole number {lo}..={hi} (got `{}`)", clip_value(v))),
            }
        };
        s.max_per_run = ranged("REAP_MAX_PER_RUN", 1, 5, 1)? as u32;
        s.max_per_day = ranged("REAP_MAX_PER_DAY", 1, 50, 10)? as u32;
        s.idle_floor_secs = ranged("REAP_IDLE_FLOOR_MINS", 15, 1440, 120)? * 60;
        s.min_free_bytes = ranged("REAP_MIN_FREE_GB", 1, 100_000, 20)? * 1_000_000_000;
        s.notify_cmd = get("REAP_NOTIFY_CMD").map(String::from);
        Ok(s)
    }
}

/// A config value quoted back in an error: short, printable.
fn clip_value(v: &str) -> String {
    let v: String = v.chars().filter(|c| !c.is_control()).take(24).collect();
    v
}

// ---------------------------------------------------------------------------------------
// Durations and times
// ---------------------------------------------------------------------------------------

/// Parse a duration `(\d+[mhd])+` — `90m`, `8h`, `1d12h` — into seconds. No seconds unit,
/// no spaces, no sign; overflow is an error.
pub fn parse_dur(text: &str) -> Result<u64, String> {
    let bad = || format!("`{}` isn't a duration like 90m, 8h or 1d12h", clip_value(text));
    let mut total: u64 = 0;
    let mut num: Option<u64> = None;
    if text.is_empty() {
        return Err(bad());
    }
    for c in text.chars() {
        match c {
            '0'..='9' => {
                let d = u64::from(c as u8 - b'0');
                num = Some(num.unwrap_or(0).checked_mul(10).and_then(|n| n.checked_add(d)).ok_or_else(bad)?);
            }
            'm' | 'h' | 'd' => {
                let n = num.take().ok_or_else(bad)?;
                let unit = match c {
                    'm' => 60,
                    'h' => 3600,
                    _ => 86_400,
                };
                total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(bad)?;
            }
            _ => return Err(bad()),
        }
    }
    if num.is_some() {
        return Err(bad());
    }
    Ok(total)
}

/// `--expire-in` / `--extend`: 10 min ..= 60 days.
pub fn check_expire_in(secs: u64) -> Result<u64, String> {
    if (EXPIRE_MIN..=EXPIRE_MAX).contains(&secs) {
        Ok(secs)
    } else {
        Err(format!("must be between {} and {} (got {})", fmt_dur(EXPIRE_MIN), fmt_dur(EXPIRE_MAX), fmt_dur(secs)))
    }
}

/// `--idle-after`: `floor` (`REAP_IDLE_FLOOR_MINS`) ..= 14 days.
pub fn check_idle_after(secs: u64, floor: u64) -> Result<u64, String> {
    if (floor..=IDLE_MAX).contains(&secs) {
        Ok(secs)
    } else {
        Err(format!(
            "--idle-after must be between {} (REAP_IDLE_FLOOR_MINS) and {} (got {})",
            fmt_dur(floor),
            fmt_dur(IDLE_MAX),
            fmt_dur(secs)
        ))
    }
}

/// `1d2h`, `3h05m`, `45m`, `30s` — compact, exact to the minute.
pub fn fmt_dur(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (d, 0, 0) => format!("{d}d"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

/// Parse `--expire-at` / `--until`: RFC 3339 (`2026-10-09T18:00:00Z`, `…+01:00`; seconds and
/// a fraction optional) or `YYYY-MM-DD HH:MM` in the control box's local zone, which
/// `resolve_local` turns into epoch seconds (the CLI asks `date -d`). The result must be at
/// least [`EXPIRE_MIN`] and at most [`EXPIRE_MAX`] after `now`.
pub fn parse_time(text: &str, now: u64, resolve_local: &dyn Fn(&str) -> Option<i64>) -> Result<u64, String> {
    let t = text.trim();
    let at = if let Some(epoch) = parse_rfc3339(t) {
        epoch
    } else if is_local_form(t) {
        resolve_local(t).ok_or_else(|| format!("couldn't resolve `{t}` in the local time zone"))?
    } else {
        return Err(format!(
            "`{}` isn't a time: use RFC 3339 (2026-10-09T18:00:00Z, …+01:00) or `YYYY-MM-DD HH:MM` (local)",
            clip_value(t)
        ));
    };
    let at = u64::try_from(at).map_err(|_| "that time is before 1970".to_string())?;
    check_time(at, now)
}

/// An absolute end time must be ≥ 10 min and ≤ 60 days from `now`.
pub fn check_time(at: u64, now: u64) -> Result<u64, String> {
    if at <= now {
        return Err(format!("that time is in the past ({} ago)", fmt_dur(now - at)));
    }
    if at - now < EXPIRE_MIN {
        return Err(format!("that time is less than {} away", fmt_dur(EXPIRE_MIN)));
    }
    if at - now > EXPIRE_MAX {
        return Err(format!("that time is more than {} away", fmt_dur(EXPIRE_MAX)));
    }
    Ok(at)
}

/// `YYYY-MM-DD HH:MM`, exactly.
fn is_local_form(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 16
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b' ',
            13 => *c == b':',
            _ => c.is_ascii_digit(),
        })
        && crate::schedule::parse_ymd(&t[..10]).is_some()
        && t[11..13].parse::<u32>().is_ok_and(|h| h < 24)
        && t[14..16].parse::<u32>().is_ok_and(|m| m < 60)
}

/// RFC 3339 → epoch seconds (`None` for anything else). `T`/`t`, `Z`/`z` or `±HH:MM`;
/// seconds optional; a fraction ignored.
pub fn parse_rfc3339(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 17 || !matches!(b[10], b'T' | b't') {
        return None;
    }
    let (y, m, d) = crate::schedule::parse_ymd(&t[..10]).filter(|_| b[4] == b'-' && b[7] == b'-')?;
    if d > days_in_month(y, m) {
        return None;
    }
    let num = |s: &str| -> Option<i64> { s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok()).flatten() };
    let hh = num(&t[11..13])?;
    if b[13] != b':' {
        return None;
    }
    let mm = num(&t[14..16])?;
    let mut rest = &t[16..];
    let mut ss = 0;
    if let Some(r) = rest.strip_prefix(':') {
        ss = num(r.get(..2)?)?;
        rest = &r[2..];
        if let Some(r) = rest.strip_prefix('.') {
            let digits = r.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            rest = &r[digits..];
        }
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[..1], "+" | "-") && &o[3..4] == ":" => {
            let (oh, om) = (num(&o[1..3])?, num(&o[4..6])?);
            if oh > 23 || om > 59 {
                return None;
            }
            let sign = if &o[..1] == "-" { -1 } else { 1 };
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = crate::schedule::days_from_civil(y, m, d);
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

// ---------------------------------------------------------------------------------------
// State (`reap.json`, version 1, strict)
// ---------------------------------------------------------------------------------------

/// The state file's format version; any other is refused (never read as empty).
pub const STATE_VERSION: u32 = 1;

/// What triggers a reap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Past `expires_at`.
    Expiry,
    /// Idle for `idle_after_secs`.
    Idle,
}

impl Trigger {
    pub fn word(self) -> &'static str {
        match self {
            Trigger::Expiry => "expiry",
            Trigger::Idle => "idle",
        }
    }
}

/// What one observation concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every applicable signal present and under its threshold.
    Idle,
    /// Something says it's in use — or a veto (keep marker, data outside the backup).
    Active,
    /// Something couldn't be read. Never becomes `Idle`.
    Unknown,
}

/// Why an observation isn't `Idle` — a FIXED vocabulary: raw pod output never reaches the
/// state, the log or a notification, only these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    // --- activity (Active) ---
    /// An authenticated SSH session other than the probe's own (a terminal, VS Code, scp…).
    Session,
    /// `who` lists a login.
    Who,
    /// An established connection to a listening port other than sshd's (Jupyter, a web UI).
    Inbound,
    /// An established outbound connection to a non-loopback address (an API eval, a download).
    Outbound,
    /// More sockets than the probe reads: some may be sessions.
    ManySockets,
    /// CPU above its threshold (since the last observation, or over the window).
    Cpu,
    /// Network above its threshold.
    Net,
    /// A GPU busier than [`GPU_UTIL_PCT`].
    GpuUtil,
    /// GPU memory held above [`GPU_MEM_MIB`].
    GpuMem,
    /// A GPU compute process listed.
    GpuProcs,
    /// A tmux pane running something other than a shell.
    Tmux,
    /// A screen session.
    Screen,
    /// A file changed (ctime) since the scan's start.
    Files,
    /// A background job (`pods run --background`) running or starting.
    Job,
    /// `~/.arena/keep` is there: a veto (decision Q2: for expiry and idle alike).
    Keep,
    /// Data on `/workspace` — not in the backup's scope (v1 reaps no volume pod).
    Workspace,
    /// The repo (`BACKUP_REPO_PATH`) resolves outside the home — not in the backup's scope.
    RepoOutside,
    /// A sweep folder (`TEMP_FOLDER_FOR_SWEEPS/`) changed since the container started — never
    /// copied by a reap's backup.
    Uncovered,
    /// Data written since the container started on another writable mount — outside the backup.
    OtherMount,
    /// A process that isn't the image's own (started after boot, not a restart of a boot-time
    /// service, not an idle daemon): somebody's job, dormant or not.
    Process,
    // --- couldn't tell (Unknown) ---
    /// SSH or the exec failed, timed out, or the reply had no end marker.
    ProbeFailed,
    /// The reply didn't parse (unknown/duplicate key, bad value, over the cap, wrong nonce).
    Garbled,
    /// The id read on the pod is empty or isn't the listed one.
    Identity,
    /// The pod's clock is skewed or drifted, or the control clock is unverified.
    Clock,
    /// No earlier observation of this container to measure CPU/network from.
    NoBaseline,
    /// A counter went down.
    CounterReset,
    /// The container (boot id / PID 1 start) changed since the last observation.
    ContainerChanged,
    /// The container's identity couldn't be read.
    ContainerUnknown,
    /// The file scan ran out of time.
    FilesTimeout,
    /// The file scan failed.
    FilesError,
    /// The GPU couldn't be read (RunPod: nvidia-smi missing, failing, hung or 0 GPUs).
    GpuUnknown,
    /// The jobs listing failed.
    JobsUnknown,
    /// SSH sessions couldn't be counted (no sshd seen, or ours not found).
    SessionsUnknown,
    /// `who` failed.
    WhoUnknown,
    /// The socket tables couldn't be read.
    TcpUnknown,
    /// CPU counters missing or inconsistent.
    CpuUnknown,
    /// Network counters missing.
    NetUnknown,
    /// tmux is installed but failed.
    TmuxUnknown,
    /// screen's answer wasn't readable.
    ScreenUnknown,
    /// `/workspace` couldn't be checked.
    WorkspaceUnknown,
    /// The repo's location couldn't be resolved.
    RepoUnknown,
    /// Something is mounted under the home: the scan (which stays on one filesystem) can't
    /// see it.
    HomeMounts,
    /// The probe's measuring window wasn't ~10 s.
    WindowBad,
    /// The home is a symlink: can't tell that the backup reads what was scanned.
    HomeLink,
    /// The home couldn't be resolved.
    HomeUnknown,
    /// The other mounts couldn't be checked.
    MountsUnknown,
    /// The process list couldn't be read in full.
    ProcessesUnknown,
}

impl Reason {
    /// Whether this reason is activity (or a veto) rather than "couldn't tell".
    pub fn is_activity(self) -> bool {
        use Reason::*;
        matches!(
            self,
            Session
                | Who
                | Inbound
                | Outbound
                | ManySockets
                | Cpu
                | Net
                | GpuUtil
                | GpuMem
                | GpuProcs
                | Tmux
                | Screen
                | Files
                | Job
                | Keep
                | Workspace
                | RepoOutside
                | Uncovered
                | OtherMount
                | Process
        )
    }

    /// The word for tables and notifications (the serde name).
    pub fn word(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }
}

/// One look at a pod (spec §6.2). Times are the control clock's except `pod_t`/`counters_t`/
/// `scan_from` (the pod's, used only for on-pod relative times).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Control clock when the probe answered.
    pub t: u64,
    /// The control clock was verified (NTP synced, not gone back) for this run.
    pub clock_ok: bool,
    /// `<boot_id>/<pid1 start>`: the container (or VM boot) observed. `None` = unknown.
    pub container: Option<String>,
    /// The pod's clock at the end of the probe.
    pub pod_t: Option<u64>,
    pub verdict: Verdict,
    #[serde(default)]
    pub reasons: Vec<Reason>,
    /// Cumulative CPU µs at `counters_t` (same container only; `None` when not trustworthy).
    pub cpu_usec: Option<u64>,
    /// Cumulative network bytes at `counters_t`.
    pub net_bytes: Option<u64>,
    /// The pod's clock when the counters were read.
    pub counters_t: Option<u64>,
    /// The pod-clock time the file scan covered from.
    pub scan_from: Option<u64>,
}

/// A pending terminate: written (and fsynced) before the reap's backup starts — the
/// write-ahead record a crashed run is reconciled from (spec §8 F1/F2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terminating {
    pub run_id: String,
    pub trigger: Trigger,
    pub at: u64,
}

/// The decision a run made for a pod, kept for `reap status` and the `ENDS` column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastDecision {
    pub t: u64,
    pub row: judge::Row,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

/// One pod's policy: when it may be reaped, and what was seen of it. Keyed `provider:id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodPolicy {
    pub provider: String,
    pub id: String,
    /// The name it had when the policy was set (or renamed through `pods rename`): a listing
    /// that says otherwise means "not this pod any more", and nothing is done.
    pub name: String,
    /// Expiry (UTC epoch seconds).
    pub expires_at: Option<u64>,
    /// Idle window, seconds.
    pub idle_after_secs: Option<u64>,
    pub set_at: u64,
    /// What set it: `set`, `up`, `create`.
    pub set_by: String,
    pub note: Option<String>,
    /// When a clock-verified run first saw the expiry passed.
    pub first_due_seen: Option<u64>,
    /// The one T−60 min participant warning went out.
    #[serde(default)]
    pub warned_soon: bool,
    /// No reap attempt before this (a reap backup failed).
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub observations: Vec<Observation>,
    pub terminating: Option<Terminating>,
    pub last: Option<LastDecision>,
    /// When each notification kind was last sent for this pod.
    #[serde(default)]
    pub notified: BTreeMap<render::NotifyKind, u64>,
    /// The container's boot-time processes (RS7), as seen so far.
    #[serde(default)]
    pub proc_baseline: Option<ProcBaseline>,
}

/// A container's own processes — those started within [`BOOT_GRACE`] of its start — by the
/// cksum of their command line, collected across looks (so a service restarted later is still
/// recognised). Reset when the container changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcBaseline {
    pub container: String,
    pub hashes: std::collections::BTreeSet<u32>,
}

impl PodPolicy {
    /// A new policy for a listed pod.
    pub fn new(pod: &Pod, expires_at: Option<u64>, idle_after_secs: Option<u64>, now: u64, set_by: &str, note: Option<String>) -> PodPolicy {
        PodPolicy {
            provider: pod.provider.clone(),
            id: pod.id.clone(),
            name: pod.name.clone(),
            expires_at,
            idle_after_secs,
            set_at: now,
            set_by: set_by.to_string(),
            note,
            first_due_seen: None,
            warned_soon: false,
            retry_after: None,
            observations: Vec::new(),
            terminating: None,
            last: None,
            notified: BTreeMap::new(),
            proc_baseline: None,
        }
    }

    /// The state key: `provider:id`.
    pub fn key(&self) -> String {
        key(&self.provider, &self.id)
    }

    /// Neither an expiry nor an idle window: no policy at all.
    pub fn is_empty(&self) -> bool {
        self.expires_at.is_none() && self.idle_after_secs.is_none()
    }

    /// Expiry only (no idle window): not probed until [`EXPIRY_PROBE_LEAD`] before it.
    pub fn expiry_only(&self) -> bool {
        self.expires_at.is_some() && self.idle_after_secs.is_none()
    }
}

/// The state key for a pod: `provider:id` (ids are only unique per provider).
pub fn key(provider: &str, id: &str) -> String {
    crate::snapshot::cache_key(provider, id)
}

/// A veto stored by name AND id: a pod renamed, or replaced under the same name, stays
/// pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub name: String,
    pub id: Option<String>,
    /// Ends at this time (`None` = until unpinned).
    pub until: Option<u64>,
    pub note: Option<String>,
    pub set_at: u64,
}

impl Pin {
    /// Whether this pin holds `pod` at `now`: its name or its id matches, and it hasn't ended.
    pub fn holds(&self, pod_name: &str, pod_id: &str, now: u64) -> bool {
        self.until.is_none_or(|u| u > now) && (self.name == pod_name || self.id.as_deref() == Some(pod_id))
    }
}

/// Stops all acting: `--apply` runs as observe until `reap release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub reason: String,
    pub set_at: u64,
    /// Set by the reaper itself (a surprise: a terminate error, a pod still billing after one).
    pub auto: bool,
}

/// What was reaped, and where its backup is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub at: u64,
    pub name: String,
    pub trigger: Option<Trigger>,
    pub snapshot_dir: Option<String>,
    pub big_dir: Option<String>,
    pub branch: Option<String>,
    /// Found gone by a later run (a crash between the terminate and its record — F2).
    #[serde(default)]
    pub recovered: bool,
    /// Runs that still saw it listed and billing after the Ok terminate (F3: 2 ⇒ hold).
    #[serde(default)]
    pub seen_after: u32,
}

/// The whole state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReapState {
    pub version: u32,
    pub last_run_at: Option<u64>,
    #[serde(default)]
    pub pods: BTreeMap<String, PodPolicy>,
    #[serde(default)]
    pub pins: Vec<Pin>,
    pub hold: Option<Hold>,
    /// Terminate attempts (write-ahead times), for the rolling daily cap.
    #[serde(default)]
    pub attempts: Vec<u64>,
    #[serde(default)]
    pub reaped: BTreeMap<String, Tombstone>,
    /// Run-level notifications (dest unhealthy, clock unverified…), rate-limited.
    #[serde(default)]
    pub run_notified: BTreeMap<render::NotifyKind, u64>,
}

impl Default for ReapState {
    fn default() -> Self {
        ReapState {
            version: STATE_VERSION,
            last_run_at: None,
            pods: BTreeMap::new(),
            pins: Vec::new(),
            hold: None,
            attempts: Vec::new(),
            reaped: BTreeMap::new(),
            run_notified: BTreeMap::new(),
        }
    }
}

impl ReapState {
    /// Parse a state file STRICTLY: unknown fields, a bad value or another version are errors
    /// — never "empty" (a corrupt file must not read as "no pins").
    pub fn parse(text: &str) -> Result<ReapState, String> {
        let probe: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        match probe.get("version").and_then(serde_json::Value::as_u64) {
            Some(v) if v == u64::from(STATE_VERSION) => {}
            Some(v) => return Err(format!("unsupported version {v} (this arena reads {STATE_VERSION})")),
            None => return Err("no version".into()),
        }
        serde_json::from_value(probe).map_err(|e| e.to_string())
    }

    /// The file's text: pretty JSON, newline-terminated.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    /// The pins holding `pod` at `now`.
    pub fn pinned(&self, name: &str, id: &str, now: u64) -> bool {
        self.pins.iter().any(|p| p.holds(name, id, now))
    }

    /// Terminate attempts in the 24 h before `now`.
    pub fn attempts_today(&self, now: u64) -> usize {
        // One in the future (the clock went back since) counts too: the safe side.
        self.attempts.iter().filter(|t| now.saturating_sub(**t) < 86_400).count()
    }

    /// Housekeeping that needs no listing: pins past their `until`, attempts older than a
    /// day, tombstones older than [`TOMBSTONE_KEEP`], and each policy's observations beyond
    /// what its window needs. Returns the pins dropped (for the log).
    pub fn tidy(&mut self, now: u64) -> Vec<Pin> {
        let (gone, kept): (Vec<Pin>, Vec<Pin>) = self.pins.drain(..).partition(|p| p.until.is_some_and(|u| u <= now));
        self.pins = kept;
        self.attempts.retain(|t| now.saturating_sub(*t) < 86_400);
        self.reaped.retain(|_, t| now.saturating_sub(t.at) < TOMBSTONE_KEEP);
        for p in self.pods.values_mut() {
            let window = p.idle_after_secs.unwrap_or(0).max(EXPIRY_PROBE_LEAD) + OBS_KEEP_EXTRA;
            p.observations.retain(|o| now.saturating_sub(o.t) <= window);
            let extra = p.observations.len().saturating_sub(OBS_KEEP_MAX);
            p.observations.drain(..extra);
        }
        gone
    }
}

/// A shell condition that holds only on the pod `id` of `provider` — what every step of a
/// reap backup runs behind (the git step through `crate::replica::guarded`, each rsync through
/// `--rsync-path`), so a reassigned endpoint has nothing read from or written to it. RunPod:
/// `RUNPOD_POD_ID` in PID 1's environment; Hetzner: cloud-init's instance id, else the
/// metadata service's. An unreadable or empty id fails it. `None` for a provider the reaper
/// doesn't support.
pub fn identity_condition(provider: &str, id: &str) -> Option<String> {
    match provider {
        "runpod" => Some(crate::replica::identity_condition(crate::replica::POD_ID_ENVIRON, id)),
        "hetzner" => Some(format!(
            "[ \"$(cat /var/lib/cloud/data/instance-id 2>/dev/null || curl -s -m 2 http://169.254.169.254/hetzner/v1/metadata/instance-id 2>/dev/null)\" = '{}' ]",
            id.replace('\'', "'\\''")
        )),
        _ => None,
    }
}

/// Whether the provider reports `pod` protected against a terminate: RunPod's `locked`,
/// Hetzner's delete protection. `None` = unknown (a backend that doesn't say, RunPod REST v1,
/// a field missing) — the reaper treats unknown as "maybe" (rule R8: never reaped).
pub fn lock_state(pod: &Pod) -> Option<bool> {
    match pod.provider.as_str() {
        "runpod" => pod.locked,
        "hetzner" => pod.delete_protected,
        _ => None,
    }
}

#[cfg(test)]
mod tests;
