//! An operation left in progress on a pod — a conflicted merge, cherry-pick or revert, or a `git
//! stash pop`'s conflicts (`BUSY`: [`crate::backup::BUSY_OP_SH`]) — stops its git backup until the
//! participant ends it. Most do; one left alone for a while (`BUSY_AUTO_RESOLVE_HOURS`, 3 h by
//! default) is finished by `pods backup` by a plain rule — the side whose commit is newer, a file
//! the participant resolved by hand kept as it is — once the pod has a copy taken after its last
//! change, and backed up again in the same run (the human's call, 2026-10-19: "backup files + some
//! automated heuristic … whatever changes are most recent … after like 3h of no activity").
//!
//! Meanwhile such a pod is copied by rsync instead of its git backup, but not every 15-minute
//! tick: only when nothing since the operation started is recorded, when its work tree changed
//! since the last recorded copy, or when that copy is an hour old ([`copy_needed`]) — so a course
//! push that leaves many pods conflicted can't make one tick outlast the next (whose `flock -n`
//! then skips the healthy pods' backups too).
//!
//! Pure here: the setting, the scripts (the read-only look and the resolve) and their parsers, the
//! copy record and the decisions. The run step is the CLI's (`crates/cli/src/resolve.rs`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ssh::shell_word;

/// The setting: hours of quiet after which `pods backup` finishes an operation in progress.
pub const AUTO_RESOLVE_KEY: &str = "BUSY_AUTO_RESOLVE_HOURS";
/// Its default, the human's "like 3h".
pub const DEFAULT_AUTO_RESOLVE_HOURS: u64 = 3;
/// Its largest value: 30 days (longer is the same as off for a course).
pub const MAX_AUTO_RESOLVE_HOURS: u64 = 720;

/// `BUSY_AUTO_RESOLVE_HOURS`: unset/empty = [`DEFAULT_AUTO_RESOLVE_HOURS`]; `0` or `off` = never
/// (`None`); else whole hours, 1..=[`MAX_AUTO_RESOLVE_HOURS`]. Anything else is a config error
/// naming the value — a typo must not quietly turn it on or off.
pub fn auto_resolve_hours(config_value: Option<&str>) -> Result<Option<u64>> {
    let Some(raw) = config_value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Some(DEFAULT_AUTO_RESOLVE_HOURS));
    };
    if raw.eq_ignore_ascii_case("off") || raw == "0" {
        return Ok(None);
    }
    raw.parse::<u64>().ok().filter(|n| (1..=MAX_AUTO_RESOLVE_HOURS).contains(n)).map(Some).ok_or_else(|| {
        Error::Config(format!(
            "{AUTO_RESOLVE_KEY} must be a whole number of hours, 1..={MAX_AUTO_RESOLVE_HOURS}, or 0/off to never finish an \
             operation left in progress (got `{raw}`)"
        ))
    })
}

/// `config check`'s words for the setting: `after 3h of quiet (default)`, `off`.
pub fn auto_resolve_words(hours: Option<u64>, set: bool) -> String {
    match hours {
        Some(h) => format!("after {h}h with nothing changed{}", if set { "" } else { " (default)" }),
        None => "off — an operation left in progress stays until the participant ends it".to_string(),
    }
}

/// The lines of the scripts that [`look_script`] and [`resolve_script`] replace.
const BUSY_OP_MARK: &str = "# @ARENA_BUSY_OP_SH@";
const LOOK_SH_MARK: &str = "# @ARENA_LOOK_SH@";

/// The look's shell function (`resolve_look.sh`): what is in progress and when anything in the
/// checkout last changed, read-only. Its header lists the lines it prints ([`parse_look`]).
fn look_sh() -> &'static str {
    include_str!("resolve_look.sh")
}

/// The look as a script of its own: `sh -c` it with the repo path. From the checkout's top.
pub fn look_script() -> String {
    format!(
        "set -u\ncd \"$1\" || exit 2\ncd \"$(git rev-parse --show-toplevel)\" || exit 2\nG=$(git rev-parse --absolute-git-dir) || exit 2\n\
         {}\n{}\narena_look\n",
        crate::backup::BUSY_OP_SH,
        look_sh().trim_end()
    )
}

/// The look a `pods backup` takes of a pod whose backup said BUSY ([`look_script`] on `repo`):
/// what the copy and the resolve go by. Read-only.
pub fn look_command(repo: &str) -> String {
    format!("sh -c {} arena-look {}", shell_word(&look_script()), shell_word(repo))
}

/// The resolve script (`resolve.sh`, its header documents it), the look rendered in.
pub fn resolve_script() -> String {
    include_str!("resolve.sh").replacen(BUSY_OP_MARK, crate::backup::BUSY_OP_SH, 1).replacen(LOOK_SH_MARK, look_sh().trim_end(), 1)
}

/// The resolve of one pod: [`resolve_script`] with the repo, the quiet period (`hours`), the
/// operation and the files as the look before the copy saw them ([`Look::op_key`],
/// [`Look::print`]), and the copy's name for the commit's words (`copy`: `w2d1/devtest-alice`).
pub fn resolve_command(repo: &str, hours: u64, op: &str, print: &str, copy: &str) -> String {
    format!(
        "sh -c {} arena-resolve {} {} {} {} {hours} {}",
        shell_word(&resolve_script()),
        shell_word(repo),
        hours * 3600,
        shell_word(op),
        shell_word(print),
        shell_word(copy)
    )
}

/// The resolve's exit when it touched nothing (`REFUSED`).
pub const RESOLVE_REFUSED_EXIT: i32 = 7;
/// The resolve's exit when a step failed midway (`FAILED`).
pub const RESOLVE_FAILED_EXIT: i32 = 8;

/// What a look saw ([`parse_look`]; `resolve_look.sh` documents each line). Every time is the
/// pod's own clock, so no skew between it and this machine matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    /// The pod's clock.
    pub now: u64,
    /// The checkout's top.
    pub top: String,
    /// The login's home.
    pub home: String,
    /// What is in progress: `busy_op`'s word, or `none`.
    pub what: String,
    /// When it started: its `*_HEAD` file's mtime (the index's, for conflicts) — `None` for what
    /// is never finished here (am, rebase, sequence) or nothing.
    pub since: Option<u64>,
    /// Its commit (MERGE_HEAD's, …; the newest stash entry's for conflicts; `-`: none).
    pub head: String,
    /// A sequence (`sequencer/`) is kept beside it: a multi-commit cherry-pick or revert.
    pub sequence: bool,
    /// The newest mtime of the files git lists (tracked; untracked, not ignored), 0 for none.
    pub newest: u64,
    /// How many files that is.
    pub files: u64,
    /// A checksum of every such file's mtime, size and path.
    pub print: String,
    /// The latest keyboard input on a terminal login, if anyone is logged in.
    pub input: Option<u64>,
}

impl Look {
    /// The operation's identity: `<what> <started> <commit>` — what the copy record is for, and
    /// what the resolve checks is still there.
    pub fn op_key(&self) -> String {
        format!("{} {} {}", self.what, self.since.map_or_else(|| "-".to_string(), |s| s.to_string()), self.head)
    }
}

/// Parse a look's stdout. `None` when a line it always prints is missing or unreadable (the
/// look failed, or isn't one): then the pod is copied as before and never resolved.
pub fn parse_look(stdout: &str) -> Option<Look> {
    let mut v: BTreeMap<&str, &str> = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        if matches!(word, "NOW" | "TOP" | "HOME" | "OP" | "NEWEST" | "PRINT" | "INPUT") {
            v.entry(word).or_insert(rest);
        }
    }
    let num = |s: &str| s.trim().parse::<u64>().ok();
    let op: Vec<&str> = v.get("OP")?.split_whitespace().collect();
    let (what, since, head, sequence) = match op[..] {
        [what] => (what.to_string(), None, "-".to_string(), false),
        [what, since, head, seq] => (what.to_string(), Some(num(since)?), head.to_string(), seq == "1"),
        _ => return None,
    };
    let newest: Vec<&str> = v.get("NEWEST")?.split_whitespace().collect();
    let [newest, files] = newest[..] else { return None };
    let print = v.get("PRINT")?.trim();
    if print.is_empty() {
        return None;
    }
    Some(Look {
        now: num(v.get("NOW")?)?,
        top: v.get("TOP")?.to_string(),
        home: v.get("HOME").map(|h| h.to_string()).unwrap_or_default(),
        what,
        since,
        head,
        sequence,
        newest: num(newest)?,
        files: num(files)?,
        print: print.to_string(),
        input: v.get("INPUT").and_then(|i| num(i)),
    })
}

/// The operations this finishes ([`BUSY_OP_SH`](crate::backup::BUSY_OP_SH)'s words): a merge, a
/// single cherry-pick or revert, a `git stash pop`'s conflicts. Never `git am`, a rebase or a
/// sequence: what they'd go on to do is the participant's.
pub fn finishes(what: &str) -> bool {
    matches!(what, "merge" | "cherry-pick" | "revert" | "conflicts")
}

/// Why a look says the pod isn't quiet for `hours` — all on the pod's clock: the operation
/// started less than that ago, a file git lists changed since, or a terminal login typed since
/// (the one activity signal the pod has for free: `who`'s ttys). `None`: quiet. A time in the
/// future (a clock set back) is never quiet. Pure.
pub fn not_quiet(look: &Look, hours: u64) -> Option<String> {
    let quiet = |t: u64| look.now.saturating_sub(t) >= hours * 3600 && t <= look.now;
    let Some(since) = look.since else { return Some(format!("no start time for the {}", look.what)) };
    if !quiet(since) {
        return Some(format!("the {} started less than {hours}h ago", look.what));
    }
    if !quiet(look.newest) {
        return Some(format!("a file changed less than {hours}h ago"));
    }
    if look.input.is_some_and(|i| !quiet(i)) {
        return Some(format!("a terminal login typed less than {hours}h ago"));
    }
    None
}

/// Whether the copy a pull makes holds the pod's checkout — its `.git` with it: the pull is the
/// home (`remote_path` empty) or a directory the checkout's top is in (or is), the home being
/// the one the look saw (else `/root` for root, `/home/<user>`). A pull of another path
/// (`BACKUP_REMOTE_PATH`) may hold none of it, and a resolve then would leave the state before
/// nowhere but on the pod. Pure.
pub fn copy_holds(remote_path: &str, look: &Look, user: &str) -> bool {
    let home = match look.home.trim() {
        "" if user == "root" => "/root".to_string(),
        "" => format!("/home/{user}"),
        h => h.to_string(),
    };
    let Some((root, _)) = crate::volume::resolve_remote_path(remote_path, &home) else { return false };
    let (root, top) = (root.trim_end_matches('/'), look.top.trim_end_matches('/'));
    top.starts_with('/') && (top == root || root.is_empty() || top.starts_with(&format!("{root}/")))
}

/// The copy record's file, in the fleet's state dir ([`crate::snapshot::state_file_path`]).
pub const BUSY_COPIES_FILE: &str = "busy-copies.json";
const BUSY_COPIES_VERSION: u32 = 1;

/// The last copy of a BUSY pod that succeeded: for which operation ([`Look::op_key`]), the work
/// tree it held ([`Look::print`], [`Look::newest`]: what the look before it saw), when (this
/// machine's clock), and where (`to`: the snapshot dir; `label`: `<wNdM>/<pod>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusyCopy {
    pub op: String,
    pub print: String,
    pub newest: u64,
    pub at: u64,
    pub to: String,
    pub label: String,
}

/// Every pod's [`BusyCopy`], by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusyCopies {
    pub version: u32,
    pub pods: BTreeMap<String, BusyCopy>,
}

impl Default for BusyCopies {
    fn default() -> Self {
        BusyCopies { version: BUSY_COPIES_VERSION, pods: BTreeMap::new() }
    }
}

impl BusyCopies {
    /// Load tolerantly: missing = none recorded; unreadable or corrupt = none recorded plus a
    /// warning — every BUSY pod is then copied again, the safe side.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<BusyCopies>(&text) {
                Ok(c) if c.version == BUSY_COPIES_VERSION => (c, None),
                Ok(c) => (Self::default(), Some(format!("warning: ignoring {} (version {})", path.display(), c.version))),
                Err(e) => (Self::default(), Some(format!("warning: ignoring {} ({e}) — copying every BUSY pod again", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(format!("warning: can't read {} ({e}) — copying every BUSY pod again", path.display()))),
        }
    }
}

/// A record this old (by this machine's clock) is a pod's that is no longer BUSY — a BUSY pod's is
/// renewed at least every [`COPY_EVERY_SECS`] — and is dropped at the next write.
pub const FORGET_AFTER_SECS: u64 = 24 * 3600;

/// Apply `changes` to the record at `path` — each pod's new copy, or `None` to forget it (a pod
/// finished) — drop the records [`FORGET_AFTER_SECS`] old at `now`, and save it atomically
/// (owner-only), under an exclusive lock on the state dir so two runs (the cron's and an
/// operator's) can't drop each other's records. `Ok` carries the load warning, if the old file had
/// to be ignored.
pub fn record_busy_copies(path: &Path, changes: &[(String, Option<BusyCopy>)], now: u64) -> std::io::Result<Option<String>> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir)?;
    let lock = std::fs::File::open(dir).ok().filter(|f| f.lock().is_ok());
    let (mut copies, warning) = BusyCopies::load(path);
    copies.pods.retain(|_, c| now.saturating_sub(c.at) < FORGET_AFTER_SECS);
    for (pod, copy) in changes {
        match copy {
            Some(c) => {
                copies.pods.insert(pod.clone(), c.clone());
            }
            None => {
                copies.pods.remove(pod);
            }
        }
    }
    let mut json = serde_json::to_string_pretty(&copies).unwrap_or_default();
    json.push('\n');
    let saved = crate::snapshot::write_atomic(path, json.as_bytes(), 0o600);
    if let Some(l) = &lock {
        let _ = l.unlock();
    }
    saved.map(|()| warning)
}

/// A BUSY pod is copied again at least this often (its last copy older: copied).
pub const COPY_EVERY_SECS: u64 = 3600;

/// Why a BUSY pod is copied this tick ([`copy_needed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyWhy {
    /// No look (it failed): copied as before.
    NoLook,
    /// No copy recorded since this operation started (none, or one of another operation).
    First,
    /// Its work tree changed since the last recorded copy.
    Changed,
    /// The last recorded copy is [`COPY_EVERY_SECS`] old.
    Stale,
}

/// Whether to copy a BUSY pod this tick, given its look and its last recorded copy (`now`: this
/// machine's clock, the record's): `Some(why)`, or `None` — copied since the operation started,
/// unchanged since, under an hour ago. Pure.
pub fn copy_needed(look: Option<&Look>, last: Option<&BusyCopy>, now: u64) -> Option<CopyWhy> {
    let Some(look) = look else { return Some(CopyWhy::NoLook) };
    let Some(last) = last.filter(|l| l.op == look.op_key()) else { return Some(CopyWhy::First) };
    if last.print != look.print {
        return Some(CopyWhy::Changed);
    }
    if now.saturating_sub(last.at) >= COPY_EVERY_SECS || last.at > now {
        return Some(CopyWhy::Stale);
    }
    None
}

/// This run's copy of a BUSY pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopiedNow {
    /// Copied, and the copy held its checkout ([`copy_holds`]).
    Yes,
    /// Tried, and failed (or it didn't hold the checkout): never resolved this run.
    Failed,
    /// Not copied this tick ([`copy_needed`]: unchanged since the recorded copy).
    No,
}

/// Why a BUSY pod is not finished this run ([`hold`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// `BUSY_AUTO_RESOLVE_HOURS` is off.
    Off,
    /// The look failed.
    NoLook,
    /// What is in progress is never finished here (am, rebase, a sequence, …).
    Never(String),
    /// Not quiet long enough ([`not_quiet`]).
    Recent(String),
    /// This run's copy failed.
    CopyFailed,
    /// No recorded copy holds its work tree as it is now.
    NoCopy,
}

/// Whether to finish a BUSY pod's operation now (`Ok`, with the copy that holds its state before),
/// or why not. Every condition: the setting on (`hours`), a look, an operation this finishes and
/// not a sequence, quiet for `hours` ([`not_quiet`]), this run's copy not failed, and a recorded
/// copy of this operation taken after the work tree's last change — its look's files the same as
/// now (`last`, which this run's own copy has just updated when there was one). Pure.
pub fn hold<'a>(hours: Option<u64>, look: Option<&Look>, copied: CopiedNow, last: Option<&'a BusyCopy>) -> std::result::Result<&'a BusyCopy, Hold> {
    let Some(hours) = hours else { return Err(Hold::Off) };
    let Some(look) = look else { return Err(Hold::NoLook) };
    if !finishes(&look.what) {
        return Err(Hold::Never(look.what.clone()));
    }
    if look.sequence {
        return Err(Hold::Never("sequence".into()));
    }
    if let Some(why) = not_quiet(look, hours) {
        return Err(Hold::Recent(why));
    }
    if copied == CopiedNow::Failed {
        return Err(Hold::CopyFailed);
    }
    last.filter(|l| l.op == look.op_key() && l.print == look.print).ok_or(Hold::NoCopy)
}

/// What a resolve said ([`parse_resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveReport {
    /// `(what, newer side, HEAD's committer time, the other side's)` once finished.
    pub resolved: Option<(String, String, u64, u64)>,
    /// Paths taken from the newer side (`gone` among them: removed, that side deleted them).
    pub newer: Vec<String>,
    pub gone: Vec<String>,
    /// Paths kept as the participant resolved them.
    pub hand: Vec<String>,
    /// Where the files were kept as they were (the pod's git dir).
    pub saved: Option<String>,
    /// Why it touched nothing.
    pub refused: Option<String>,
    /// The step that failed midway.
    pub failed: Option<String>,
}

/// Parse a resolve's stdout (`resolve.sh`'s header lists the lines). Unknown lines (git's own)
/// are ignored.
pub fn parse_resolve(stdout: &str) -> ResolveReport {
    let mut r = ResolveReport::default();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "NEWER" if !rest.is_empty() => r.newer.push(rest.to_string()),
            "GONE" if !rest.is_empty() => {
                r.newer.push(rest.to_string());
                r.gone.push(rest.to_string());
            }
            "HAND" if !rest.is_empty() => r.hand.push(rest.to_string()),
            "SAVED" if !rest.is_empty() => r.saved = Some(rest.to_string()),
            "REFUSED" => r.refused = Some(rest.trim().to_string()),
            "FAILED" => r.failed = Some(rest.trim().to_string()),
            "RESOLVED" => {
                if let [what, side, ours, theirs] = rest.split_whitespace().collect::<Vec<_>>()[..] {
                    if let (Ok(o), Ok(t)) = (ours.parse(), theirs.parse()) {
                        r.resolved = Some((what.to_string(), side.to_string(), o, t));
                    }
                }
            }
            _ => {}
        }
    }
    r
}

/// `1 file`, `2 files`.
fn files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// The side an operation brings in, as the resolve's words name it: `MERGE_HEAD`, …, `the stash`.
fn other_side(what: &str) -> &'static str {
    match what {
        "merge" => "MERGE_HEAD",
        "cherry-pick" => "CHERRY_PICK_HEAD",
        "revert" => "REVERT_HEAD",
        _ => "the stash",
    }
}

/// What a finished resolve did, for the pod's line (`hours`: the quiet period; `before`: the copy
/// that holds the state before): `finished a merge left for 3h+ (kept the newer side, HEAD's, for 2
/// files and the hand-resolved version of 1; before: /b/w2d1/devtest-alice, and
/// /root/ARENA_materials/.git/arena-resolved/20261019T140200Z on the pod)`. For a `git stash
/// pop`'s conflicts, that its stash entry is kept. Pure.
pub fn resolved_note(r: &ResolveReport, hours: u64, before: &str) -> String {
    let (what, side) = r.resolved.as_ref().map_or(("merge", "ours"), |(w, s, ..)| (w.as_str(), s.as_str()));
    let done = match what {
        "conflicts" => format!("resolved the conflicts a `git stash pop` left for {hours}h+ (its stash entry kept)"),
        w => format!("finished a {w} left for {hours}h+"),
    };
    let mut kept = Vec::new();
    if !r.newer.is_empty() {
        let newer = if side == "ours" { "HEAD's".to_string() } else { format!("{}'s", other_side(what)) };
        kept.push(format!("kept the newer side, {newer}, for {}", files(r.newer.len())));
    }
    if !r.hand.is_empty() {
        kept.push(format!("the hand-resolved version of {}", r.hand.len()));
    }
    let kept = if kept.is_empty() { "nothing was left in conflict".to_string() } else { kept.join(" and ") };
    let saved = r.saved.as_ref().map_or_else(String::new, |s| format!(", and {s} on the pod"));
    format!("{done} ({kept}; before: {before}{saved})")
}

#[cfg(test)]
mod tests;
