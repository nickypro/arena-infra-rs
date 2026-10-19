//! What the reaper shows and says — pure: the `ENDS` column of `pods list`, the `pods reap
//! run` plan table, the participant notice and its login banner, the cron line, the
//! notification text and the JSONL log line. Times are shown in the control box's zone (an
//! offset the CLI reads once) and as "in Xh Ym"; everything a pod said is reduced to the
//! fixed vocabulary of [`Reason`] / [`Row`] before it gets here.

use serde::{Deserialize, Serialize};

use super::judge::{idle_streak, Mode, Row};
use super::*;
use crate::selector::Naming;
use crate::table::{self, Align};

/// What a notification is about (`REAP_NOTIFY_CMD`; rate-limited per pod per kind to once per
/// [`NOTIFY_REPEAT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyKind {
    /// A pod was reaped (with its backup paths and the keys hint).
    Reaped,
    /// A pod with a pending terminate was found gone (a crash after the terminate).
    RecoveredReaped,
    /// A pod with a reap attempt under way was found gone before its terminate was sent: not
    /// reaped by the reaper; its copy, if frozen, keeps its `-attempt` name (W7LR-1).
    GoneBeforeReap,
    /// Overdue but in use: deferred.
    OverdueInUse,
    /// Overdue but couldn't be checked: deferred.
    OverdueUnreachable,
    /// Overdue but not running (stopped): terminate it by hand.
    OverdueNotRunning,
    /// Overdue with no SSH endpoint.
    OverdueNoEndpoint,
    GitFailed,
    RsyncFailed,
    DestUnhealthy,
    /// The verified backup couldn't be frozen into `reaped/` (RS1): not reaped.
    FreezeFailed,
    /// A terminate failed in an unknown way: a hold is set.
    TerminateFailed,
    /// A reaped pod is still listed and billing.
    StillBilling,
    HoldSet,
    ClockUnverified,
    StateUnreadable,
}

impl NotifyKind {
    pub fn word(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }
}

/// Whether a notification last sent at `last` may go again at `now`.
pub fn may_notify(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|t| t > now || now - t >= NOTIFY_REPEAT)
}

/// The one line a notification sends (on the command's stdin): fixed words, the pod's name,
/// and for a reap where its backup is — never raw pod output. Control characters are dropped.
pub fn notify_line(prefix: &str, kind: NotifyKind, pod: Option<&str>, detail: &str) -> String {
    let pod = pod.map(|p| format!(" {p}")).unwrap_or_default();
    let line = format!("arena reap [{prefix}] {}{pod}: {detail}", kind.word());
    line.chars().filter(|c| !c.is_control()).take(600).collect()
}

/// What a reap's git step found in progress on the pod (its backup's `BUSY`): the operation
/// ([`crate::backup::BUSY_OP_SH`]'s word), the branch, and whether paths are still unmerged — for
/// the words of [`git_busy_why`] / [`git_busy_notice`]. The reap's decision is the git step's
/// (R32, not reaped); only what is said about it is this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBusy {
    pub what: String,
    pub branch: String,
    pub unmerged: bool,
}

impl GitBusy {
    /// From the backup's report of a git step that exited `code`: `Some` only for a `BUSY` with
    /// its own exit and one of the fixed words — anything else stays the generic "git backup
    /// failed".
    pub fn of(report: &crate::backup::BackupReport, code: i32) -> Option<GitBusy> {
        let what = report.in_progress(Some(code)).filter(|w| matches!(*w, "merge" | "cherry-pick" | "revert" | "conflicts"))?;
        Some(GitBusy { what: what.to_string(), branch: report.branch.clone().unwrap_or_default(), unmerged: report.unmerged })
    }

    /// The branch as said: a plain ref name (letters, digits, `.`, `_`, `-`, `/`; at most 100) as
    /// it is, anything else `its branch` — never raw pod output in a line or a notification.
    fn on(&self) -> &str {
        let b = self.branch.as_str();
        let plain = !b.is_empty()
            && b.len() <= 100
            && !b.starts_with('-')
            && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
        if plain {
            b
        } else {
            "its branch"
        }
    }
}

/// The reap run's WHY for a pod its git step left alone because an operation is in progress
/// there (review fb2 F1): "a merge is in progress on autocommit-x — not reaped until it's finished
/// or aborted" — not a generic "git backup failed".
pub fn git_busy_why(b: &GitBusy) -> String {
    let situation = crate::backup::busy_situation(&b.what, b.on());
    match b.what.as_str() {
        "conflicts" => format!("{situation} — not reaped until they're resolved or undone"),
        _ => format!("{situation} — not reaped until it's finished or aborted"),
    }
}

/// The notification for it: [`git_busy_why`], how to end it either way on the pod (the backup's
/// own words: [`crate::backup::busy_advice`]), and when the reap is tried again.
pub fn git_busy_notice(b: &GitBusy) -> String {
    format!("{}; on the pod: {}; retried in 6h", git_busy_why(b), crate::backup::busy_advice(&b.what, b.unmerged))
}

/// `(year, month, day, hour, minute)` of `epoch` shifted by `tz` seconds.
fn parts(epoch: u64, tz: i64) -> (i64, u32, u32, u64, u64) {
    let t = epoch as i64 + tz;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400) as u64;
    let (y, m, d) = crate::schedule::civil_from_days(days);
    (y, m, d, secs / 3600, secs % 3600 / 60)
}

/// `2026-10-12 18:00` (local).
pub fn fmt_local(epoch: u64, tz: i64) -> String {
    let (y, m, d, hh, mm) = parts(epoch, tz);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// `+02:00` for an offset in seconds.
pub fn fmt_tz(tz: i64) -> String {
    let sign = if tz < 0 { '-' } else { '+' };
    let a = tz.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60)
}

/// `5h`, `1h53m`, `20m`, `2d3h` — a span for "in …" / "… ago".
pub fn fmt_span(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h{m:02}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

/// When an expiry is, for a table: within a day `18:00 (in 5h)`, else `10-12 18:00`.
fn when(at: u64, now: u64, tz: i64) -> String {
    let (_, m, d, hh, mm) = parts(at, tz);
    if at > now && at - now < 86_400 {
        format!("{hh:02}:{mm:02} (in {})", fmt_span(at - now))
    } else {
        format!("{m:02}-{d:02} {hh:02}:{mm:02}")
    }
}

/// The last decision says the pod was in use / couldn't be checked.
fn last_row(policy: &PodPolicy) -> Option<Row> {
    policy.last.as_ref().map(|l| l.row)
}

fn in_use(row: Option<Row>) -> bool {
    matches!(row, Some(Row::Active | Row::Keep | Row::NotCovered))
}

fn unreachable(row: Option<Row>) -> bool {
    matches!(row, Some(Row::ProbeFailed | Row::Identity | Row::Clock | Row::Garbled | Row::Unknown | Row::NoEndpoint | Row::BackupBusy))
}

/// The idle part of a label: `idle 3h/8h` (the current streak against the window).
fn idle_part(policy: &PodPolicy) -> Option<String> {
    let w = policy.idle_after_secs?;
    let span = idle_streak(&policy.observations).map_or(0, |s| s.span());
    Some(format!("idle {}/{}", fmt_span(span), fmt_span(w)))
}

/// The `ENDS` column of `pods list` (spec §3.4) for `pod`: `-` with no policy; `ignored
/// (off-list)`, `locked`, `pinned`/`pinned→10-12`, `kept on pod`, `not covered` where those
/// stop it; else the expiry (`10-12 18:00`, `18:00 (in 5h)`, `OVERDUE 35m`, `OVERDUE 35m (in
/// use)`, `OVERDUE 35m (unreachable)`) and/or `idle 3h/8h`.
pub fn ends_label(policy: Option<&PodPolicy>, pin: Option<&Pin>, pod: &Pod, naming: &Naming, inert: Option<&str>, now: u64, tz: i64) -> String {
    let Some(p) = policy.filter(|p| !p.is_empty()) else { return "-".into() };
    if !naming.reapable(&pod.name) {
        return "ignored (off-list)".into();
    }
    // A policy the reaper can't act on is never shown counting down (RR2).
    if let Some(why) = inert {
        return format!("not reapable ({why})");
    }
    if lock_state(pod) == Some(true) {
        return "locked".into();
    }
    if let Some(pin) = pin {
        return match pin.until {
            Some(u) => {
                let (_, m, d, _, _) = parts(u, tz);
                format!("pinned→{m:02}-{d:02}")
            }
            None => "pinned".into(),
        };
    }
    let row = last_row(p);
    match row {
        Some(Row::Keep) => return "kept on pod".into(),
        Some(Row::NotCovered) => return "not covered".into(),
        _ => {}
    }
    let mut parts_out = Vec::new();
    if let Some(e) = p.expires_at {
        if e <= now {
            let mut s = format!("OVERDUE {}", fmt_span(now - e));
            if in_use(row) {
                s.push_str(" (in use)");
            } else if unreachable(row) {
                s.push_str(" (unreachable)");
            }
            parts_out.push(s);
        } else {
            parts_out.push(when(e, now, tz));
        }
    }
    parts_out.extend(idle_part(p));
    parts_out.join(" · ")
}

/// A policy in a few words: `expiry 10-12 18:00`, `idle 8h`, `expiry 18:00 (in 5h) + idle 8h`.
pub fn policy_label(p: &PodPolicy, now: u64, tz: i64) -> String {
    let mut v = Vec::new();
    if let Some(e) = p.expires_at {
        v.push(format!("expiry {}", when(e, now, tz)));
    }
    if let Some(w) = p.idle_after_secs {
        v.push(format!("idle {}", fmt_span(w)));
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join(" + ")
    }
}

/// One row of the plan table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanRow {
    pub name: String,
    pub key: String,
    pub policy: String,
    pub state: String,
    pub row: Row,
    pub next: String,
    pub why: String,
}

/// What a decision row means for "NEXT".
pub fn next_label(row: Row) -> &'static str {
    match row {
        Row::StreakShort | Row::Scheduled | Row::ExpiryConfirming | Row::ExpiryWaitQuiet | Row::BackoffAfterFailure | Row::Capped => "wait",
        Row::WouldReap => "WOULD TERMINATE",
        Row::Candidate => "TERMINATE",
        Row::Reaped => "REAPED",
        Row::Rehearsed => "rehearsed",
        _ => "nothing",
    }
}

/// The words behind a row and its reasons, for "WHY": the reasons (fixed vocabulary) when
/// there are any, else what the row means.
pub fn why_label(row: Row, reasons: &[Reason]) -> String {
    if !reasons.is_empty() {
        let words: Vec<String> = reasons.iter().map(|r| r.word()).collect();
        let lead = match row {
            Row::NotCovered => "not covered by the backup: ",
            Row::Active | Row::Keep => "in use: ",
            Row::PostBackupNotQuiet => "not quiet after the backup: ",
            _ => "can't tell: ",
        };
        return format!("{lead}{}", words.join(", "));
    }
    match row {
        Row::NoPolicy => "no policy",
        Row::ListingFailed => "its provider's listing failed",
        Row::ProviderUnsupported => "provider not supported (Vast, Shadeform; RunPod needs RUNPOD_API=v2)",
        Row::NotOnList => "not a MACHINE_NAME_LIST machine",
        Row::DuplicateOrTwin => "name shared, or a -old/-new twin listed",
        Row::NameMismatch => "listed under another name than its policy's",
        Row::Locked => "locked / delete-protected (not probed)",
        Row::LockUnknown => "lock state unknown (not probed)",
        Row::Pinned => "pinned (not probed)",
        Row::Terminating => "a terminate is pending",
        Row::NotRunning => "not running",
        Row::NoEndpoint => "no SSH endpoint",
        Row::Scheduled => "expiry not reached",
        Row::BackupBusy => "another backup holds the pod's lock",
        Row::StreakShort => "idle streak short",
        Row::ExpiryConfirming => "expiry seen; confirming",
        Row::ExpiryWaitQuiet => "expired; waiting for 2 quiet looks",
        Row::BackoffAfterFailure => "a backup failed < 6h ago",
        Row::WouldReap => "backup first (observe run)",
        Row::Capped => "run/day cap reached",
        Row::Candidate => "backup first",
        Row::ReprobeNotIdle => "not idle when looked at again",
        Row::GitFailed => "git backup failed",
        Row::RsyncFailed => "file backup failed",
        Row::VerifyPending => "backup verify found files still to copy",
        Row::PostBackupNotQuiet => "not quiet after the backup",
        Row::Rehearsed => "rehearsal: backed up and verified, not terminated",
        Row::RelistFailed => "re-listing didn't confirm the pod",
        Row::PartialListing => "another provider's listing failed at the re-listing (a twin could be there)",
        Row::FreezeFailed => "the verified backup couldn't be frozen into reaped/",
        Row::DisabledMidRun => "REAP_ENABLED was switched off in the config file during the run",
        Row::GoneBeforeReap => "gone before the reap",
        Row::Reaped => "terminated after a verified backup",
        Row::TerminateRefusedLocked => "the provider refused: locked",
        Row::TerminateFailed => "terminate failed — hold set",
        Row::Held => "reaping is on hold",
        Row::Disabled => "reaping disabled in config",
        Row::ClockUnverified => "clock unverified",
        Row::DestUnhealthy => "backup destination unhealthy",
        _ => "-",
    }
    .to_string()
}

/// The `STATE` cell: `in 1h53m` (expiry ahead), `OVERDUE 20m`, `idle 6h12m (25 obs)`, `locked`,
/// `?` (couldn't tell), `-`.
pub fn state_label(p: &PodPolicy, row: Row, now: u64) -> String {
    match row {
        Row::Locked => return "locked".into(),
        Row::Pinned => return "pinned".into(),
        Row::ProbeFailed | Row::Identity | Row::Garbled | Row::Clock | Row::Unknown | Row::NoEndpoint => {
            return match p.expires_at {
                Some(e) if e <= now => format!("OVERDUE {}", fmt_span(now - e)),
                _ => "?".into(),
            }
        }
        _ => {}
    }
    let mut v = Vec::new();
    if let Some(e) = p.expires_at {
        v.push(if e <= now { format!("OVERDUE {}", fmt_span(now - e)) } else { format!("in {}", fmt_span(e - now)) });
    }
    if p.idle_after_secs.is_some() {
        if let Some(s) = idle_streak(&p.observations) {
            v.push(format!("idle {} ({} obs)", fmt_span(s.span()), s.count));
        }
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join(", ")
    }
}

/// The plan table's header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanHeader {
    pub now: u64,
    pub tz: i64,
    pub mode: Mode,
    /// `--apply` was asked for but the run is observe-only: why.
    pub demoted: Option<String>,
    pub max_per_run: u32,
    pub max_per_day: u32,
    pub used_today: usize,
    pub state_path: String,
}

/// The `pods reap run` table (spec §3.4).
pub fn render_plan(h: &PlanHeader, rows: &[PlanRow], footer: &[String]) -> String {
    let mode = match (h.mode, &h.demoted) {
        (Mode::Observe, Some(why)) => format!("OBSERVE ({why})"),
        (Mode::Observe, None) => "DRY RUN".to_string(),
        (Mode::Rehearse, _) => "REHEARSE".to_string(),
        (Mode::Apply, _) => "APPLY".to_string(),
    };
    let mut out = format!(
        "Reap {} {} · {mode} · caps {}/run {}/day ({} used) · state {}\n",
        fmt_local(h.now, h.tz),
        fmt_tz(h.tz),
        h.max_per_run,
        h.max_per_day,
        h.used_today,
        h.state_path
    );
    if rows.is_empty() {
        out.push_str("(no pod has a reap policy — `arena pods reap set` gives one)\n");
    } else {
        let cells: Vec<Vec<String>> =
            rows.iter().map(|r| vec![r.name.clone(), r.policy.clone(), r.state.clone(), r.next.clone(), r.why.clone()]).collect();
        out.push_str(&table::render(&["NAME", "POLICY", "STATE", "NEXT", "WHY"], &[Align::Left; 5], &cells));
    }
    for f in footer {
        out.push_str(f);
        out.push('\n');
    }
    out
}

/// The managed line `reap set` adds to `~/.bashrc` / `~/.zshrc`: the notice, on stderr, for
/// INTERACTIVE shells only — `case $- in *i*)` — so a `.bashrc` sourced for a non-interactive
/// sshd command (rsync, scp, our own probes) can never write to stdout and break them.
pub const BANNER_LINE: &str = r#"case $- in *i*) [ -r "$HOME/.arena/NOTICE" ] && cat "$HOME/.arena/NOTICE" >&2 ;; esac # arena reap notice"#;

/// The participant notice (`~/.arena/NOTICE`), plain text.
pub fn notice_text(name: &str, p: &PodPolicy, tz: i64) -> String {
    let mut s = format!("─── arena: {name} is set to be shut down automatically ───\n");
    if let Some(e) = p.expires_at {
        s.push_str(&format!("  • at {} (UTC{}) — its end time;\n", fmt_local(e, tz), fmt_tz(tz)));
    }
    if let Some(w) = p.idle_after_secs {
        s.push_str(&format!(
            "  • after {} with no activity (no SSH or VS Code session, no running job, no GPU use, no file changes).\n",
            fmt_span(w)
        ));
    }
    // What the reap's backup leaves out: the one wording code, notice and README share.
    s.push_str(&format!(
        "  Your home directory is backed up first ({}) — only your home. Nothing happens while the machine is in use, \
         while a TEMP_FOLDER_FOR_SWEEPS/ holds any results, or while you have data outside your home (/data, /tmp, \
         /workspace, a link out of the home …).\n  \
         To keep it running, create ~/.arena/keep (`mkdir -p ~/.arena && touch ~/.arena/keep`) and tell the organisers.\n",
        crate::pull::REAP_NOT_KEPT_SUMMARY
    ));
    s
}

/// Single-quote for `sh`.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Write the notice on the pod and add [`BANNER_LINE`] to the shell rc files that exist
/// (idempotently: only if that exact line isn't there). The text goes base64 — no quoting of
/// its own. Best effort: the caller ignores a failure.
pub fn notice_write_command(text: &str) -> String {
    let b64 = crate::base64::encode(text.as_bytes());
    let line = sq(BANNER_LINE);
    format!(
        "mkdir -p \"$HOME/.arena\" && printf %s {b64} | base64 -d > \"$HOME/.arena/NOTICE.tmp\" && mv -f \"$HOME/.arena/NOTICE.tmp\" \"$HOME/.arena/NOTICE\" && \
         for f in \"$HOME/.bashrc\" \"$HOME/.zshrc\"; do [ -f \"$f\" ] || continue; grep -qxF -- {line} \"$f\" || printf '%s\\n' {line} >> \"$f\"; done"
    )
}

/// Remove the notice (the banner line stays: with no notice it prints nothing).
pub fn notice_clear_command() -> String {
    "rm -f \"$HOME/.arena/NOTICE\"".to_string()
}

/// The one `wall` before a reap (apply mode only, best effort): fixed text, no pod input.
pub fn wall_command(name: &str, trigger: Trigger, in_secs: u64) -> String {
    let why = match trigger {
        Trigger::Expiry => "its end time",
        Trigger::Idle => "no activity",
    };
    let msg = format!(
        "arena: {name} will be shut down in about {} ({why}) unless it is in use. Your home is backed up first. \
         To keep it: mkdir -p ~/.arena && touch ~/.arena/keep",
        fmt_span(in_secs)
    );
    format!("command -v wall >/dev/null 2>&1 && printf '%s\\n' {} | wall 2>/dev/null; true", sq(&msg))
}

/// The cron schedule of the reap line: four times an hour, on minutes no other arena line uses —
/// off the `*/15` backup's ticks (its rsyncs hold the pods' backup locks: a probe meanwhile is
/// skipped), off the backup archive's pushes (`archive sync`: snap at :07/:22/:37/:52, big at
/// :40), which read the same backup roots a reap backup writes (live W7 step 0: the reap line had
/// the archive snap line's very minutes), off the `*/5` proxy re-sync (a reap syncs the proxy
/// too) and off the `*/2` dashboard snapshot: odd minutes, none a multiple of 5 — 11 or 12 after
/// a backup tick, 4 or 5 after an archive push.
pub const CRON_SCHEDULE: &str = "11,27,41,57 * * * *";

/// `PATH` for the reap line (cron's is `/usr/bin:/bin`; nginx and friends live in sbin).
pub const CRON_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The reap cron line (spec §12.2): observe (`pods reap run`) or apply (`--apply --yes`),
/// with `env` (`ARENA_STATE_DIR=…`, `ARENA_START_DATE=…` — the ones the config file doesn't
/// set) inline, under `flock -n -o` so a tick never stacks on a long reap backup — `-o`
/// keeps the lock from being inherited by what arena spawns (an rsync, ssh, the notify
/// command): one of them outliving an arena that was killed would otherwise hold it, and every
/// later tick would skip without a word (RR4; the backup line has had `-o` for that reason).
/// Its lock is in arena's cron folder, and so is its log unless `log` keeps one the installed
/// line already used elsewhere; the line recreates both folders first ([`crate::cron::mkdir`]:
/// without it, a deleted folder would silently stop the reaper).
pub fn cron_line(apply: bool, exe: &str, config: &str, home: &str, log: Option<&str>, env: &[(String, String)]) -> String {
    use crate::cron::{self, Job};
    let env: String = env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
    let act = if apply { " --apply --yes" } else { "" };
    let log = log.map_or_else(|| cron::log(home, Job::Reap), String::from);
    let (mkdir, lock) = (cron::mkdir(home, &log), cron::lock(home, Job::Reap));
    format!("{CRON_SCHEDULE} {mkdir}{CRON_PATH} {env}flock -n -o {lock} {exe} --config {config} pods reap run{act} >> {log} 2>&1")
}

/// One line of `reap-log.jsonl` — fixed vocabulary only (spec I15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogLine {
    pub t: u64,
    pub run: String,
    pub mode: Mode,
    /// `eval` (a pod's decision), `event` (reconciling, the run's own), `act` (a step of a reap).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub row: Row,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streak_obs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streak_secs: Option<u64>,
    /// Backup destinations (control-side paths) for a rehearsal or a reap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl LogLine {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
