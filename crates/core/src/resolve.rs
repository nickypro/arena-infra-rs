//! An operation left in progress on a pod — a conflicted merge or cherry-pick, or a `git stash
//! pop`'s conflicts (`BUSY`: [`crate::backup::BUSY_OP_SH`]) — stops its git backup until the
//! participant ends it. Most do; one left alone for a while (`BUSY_AUTO_RESOLVE_HOURS`, 3 h by
//! default) is finished by `pods backup` by a plain rule — per conflicted file, the side whose last
//! change to it is the more recent, per hunk where the file is text (the clean changes of both
//! sides kept); a file the participant resolved by hand kept as it is — once the pod has a copy
//! taken after its last change, and backed up again in the same run (the human's call,
//! 2026-10-19: "backup files + some automated heuristic … whatever changes are most recent … after
//! like 3h of no activity"; applied per file and per hunk so that neither side's work is silently
//! lost). A revert is never finished: its other side is the commit it undoes, always the older.
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
const REDACT_SH_MARK: &str = "# @ARENA_REDACT_SH@";
const REVERT_WORDS_MARK: &str = "@ARENA_REVERT_WORDS@";

/// Why a revert is never finished, as the resolve refuses it and a pod's line says it.
pub const REVERT_WORDS: &str = "arena won't finish a revert (its other side is the commit it undoes, always older than HEAD, \
     so the newer side would undo nothing) — end it on the pod: `git revert --continue` or `git revert --abort`";

/// The look's shell function (`resolve_look.sh`): what is in progress, when anything in the
/// checkout last changed and whether anyone is at work in it, read-only. Its header lists the
/// lines it prints ([`parse_look`]).
fn look_sh() -> &'static str {
    include_str!("resolve_look.sh")
}

/// The look as a script of its own: `sh -c` it with the repo path (as configured: `REPO`). From
/// the checkout's top.
pub fn look_script() -> String {
    format!(
        "set -u\nREPO=$1\ncd \"$REPO\" || exit 2\ncd \"$(git rev-parse --show-toplevel)\" || exit 2\nG=$(git rev-parse --absolute-git-dir) || exit 2\n\
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

/// The resolve script (`resolve.sh`, its header documents it), the look, the redaction
/// ([`crate::backup::redact_sh`], as the backup commits) and [`REVERT_WORDS`] rendered in.
pub fn resolve_script() -> String {
    include_str!("resolve.sh")
        .replacen(BUSY_OP_MARK, crate::backup::BUSY_OP_SH, 1)
        .replacen(LOOK_SH_MARK, look_sh().trim_end(), 1)
        .replacen(REDACT_SH_MARK, crate::backup::redact_sh().trim_end(), 1)
        .replacen(REVERT_WORDS_MARK, &shell_word(REVERT_WORDS), 1)
}

/// The resolve of one pod: [`resolve_command_limited`] with the backup's own size limit
/// ([`crate::backup::GIT_MAX_FILE_BYTES`]): a resolved file over it is never committed.
pub fn resolve_command(repo: &str, hours: u64, op: &str, print: &str, copy: &str) -> String {
    resolve_command_limited(repo, hours, op, print, copy, crate::backup::GIT_MAX_FILE_BYTES)
}

/// The resolve of one pod: [`resolve_script`] with the repo, the quiet period (`hours`), the
/// operation and the files as the look before the copy saw them ([`Look::op_key`],
/// [`Look::print`]), the copy's name for the commit's words (`copy`: `w2d1/devtest-alice`), and
/// the largest file it may commit (`max_bytes`; tests pass a small one).
pub fn resolve_command_limited(repo: &str, hours: u64, op: &str, print: &str, copy: &str, max_bytes: u64) -> String {
    format!(
        "sh -c {} arena-resolve {} {} {} {} {hours} {} {max_bytes}",
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
    /// The checkout's top, every link resolved.
    pub top: String,
    /// The configured repo path, when it names that top (a link onto the volume, or the top
    /// itself): where a pull of the home finds the checkout.
    pub repo: Option<String>,
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
    /// A checksum of every such file's mtime, size, ctime and path.
    pub print: String,
    /// The index's mtime: something staged since.
    pub index: Option<u64>,
    /// git's index is locked (`.git/index.lock`): a git command is at work.
    pub locked: bool,
    /// The latest keyboard input on a terminal login, if anyone is logged in.
    pub input: Option<u64>,
    /// The latest use of any pseudo-terminal (`/dev/pts/N`), if there is one.
    pub tty: Option<u64>,
    /// A process at work in git (`<pid> <name>`): a `git` running, or an editor on its message.
    pub at_work: Option<String>,
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
        if matches!(word, "NOW" | "TOP" | "REPO" | "HOME" | "OP" | "NEWEST" | "PRINT" | "INDEX" | "LOCKED" | "INPUT" | "TTY" | "PROC") {
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
    let text = |k: &str| v.get(k).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    Some(Look {
        now: num(v.get("NOW")?)?,
        top: v.get("TOP")?.to_string(),
        repo: text("REPO"),
        home: v.get("HOME").map(|h| h.to_string()).unwrap_or_default(),
        what,
        since,
        head,
        sequence,
        newest: num(newest)?,
        files: num(files)?,
        print: print.to_string(),
        index: v.get("INDEX").and_then(|i| num(i)),
        locked: v.contains_key("LOCKED"),
        input: v.get("INPUT").and_then(|i| num(i)),
        tty: v.get("TTY").and_then(|i| num(i)),
        at_work: text("PROC"),
    })
}

/// The operations this finishes ([`BUSY_OP_SH`](crate::backup::BUSY_OP_SH)'s words): a merge, a
/// single cherry-pick, a `git stash pop`'s conflicts. Never `git am`, a rebase or a sequence: what
/// they'd go on to do is the participant's. Never a revert ([`REVERT_WORDS`]).
pub fn finishes(what: &str) -> bool {
    matches!(what, "merge" | "cherry-pick" | "conflicts")
}

/// Why a look says the pod isn't quiet for `hours` — all on the pod's clock: the operation
/// started less than that ago, a file git lists changed since, something was staged since (the
/// index), git's index is locked, a terminal login typed or any terminal was used since, or a git
/// command (or an editor one waits on) is running. `None`: quiet. A time in the future (a clock
/// set back) is never quiet. Unsaved buffers in an editor or a notebook can't be seen: what
/// reaches the disk is all a look knows, and the copy and `.git/arena-resolved/` keep the state
/// before. Pure.
pub fn not_quiet(look: &Look, hours: u64) -> Option<String> {
    let quiet = |t: u64| look.now.saturating_sub(t) >= hours * 3600 && t <= look.now;
    let Some(since) = look.since else { return Some(format!("no start time for the {}", look.what)) };
    if !quiet(since) {
        return Some(format!("the {} started less than {hours}h ago", look.what));
    }
    if !quiet(look.newest) {
        return Some(format!("a file changed less than {hours}h ago"));
    }
    if look.index.is_some_and(|i| !quiet(i)) {
        return Some(format!("git's index changed less than {hours}h ago (something was staged)"));
    }
    if look.locked {
        return Some("git's index is locked (.git/index.lock): a git command is at work, or died holding it".to_string());
    }
    if look.input.is_some_and(|i| !quiet(i)) {
        return Some(format!("a terminal login typed less than {hours}h ago"));
    }
    if look.tty.is_some_and(|i| !quiet(i)) {
        return Some(format!("a terminal was used less than {hours}h ago"));
    }
    if let Some(p) = &look.at_work {
        let (pid, name) = p.split_once(' ').unwrap_or((p.as_str(), "?"));
        return Some(format!("a git command, or an editor one waits on, is running (pid {pid}: {name})"));
    }
    None
}

/// Where a pull's copy holds the pod's checkout — its `.git` with it — relative to the pod's
/// directory in the copy (`ARENA_materials`; empty when the copy is the checkout itself), or
/// `None` when it holds none of it. The pull is the home (`remote_path` empty) or a directory
/// (`BACKUP_REMOTE_PATH`), the home being the one the look saw (else `/root` for root,
/// `/home/<user>`). The checkout is found in it at its top, every link resolved ([`Look::top`]) —
/// or, where the configured repo path names the top through a link ([`Look::repo`]: setup's
/// `/root/ARENA_materials -> /workspace/ARENA_materials`), at that path, which the pull's own
/// probe splits out and copies as a tree in its place (`volume::repo_pull_from`). A pull of
/// another path may hold none of it, and a resolve then would leave the state before nowhere but
/// on the pod. Pure.
pub fn copy_place(remote_path: &str, look: &Look, user: &str) -> Option<String> {
    let home = match look.home.trim() {
        "" if user == "root" => "/root".to_string(),
        "" => format!("/home/{user}"),
        h => h.to_string(),
    };
    let (root, contents) = crate::volume::resolve_remote_path(remote_path, &home)?;
    let root = root.trim_end_matches('/');
    // rsync names a source without a trailing slash by its last component, under which it lands.
    let named = root.rsplit('/').next().unwrap_or_default();
    [Some(look.top.as_str()), look.repo.as_deref()].into_iter().flatten().find_map(|top| {
        let top = top.trim_end_matches('/');
        if !top.starts_with('/') {
            return None;
        }
        let inner = if top == root {
            ""
        } else if root.is_empty() {
            &top[1..]
        } else {
            top.strip_prefix(root)?.strip_prefix('/')?
        };
        Some(match (contents, inner) {
            (true, inner) => inner.to_string(),
            (false, "") => named.to_string(),
            (false, inner) => format!("{named}/{inner}"),
        })
    })
}

/// Whether the copy a pull makes holds the pod's checkout, `.git` and all ([`copy_place`]). Pure.
pub fn copy_holds(remote_path: &str, look: &Look, user: &str) -> bool {
    copy_place(remote_path, look, user).is_some()
}

/// The copy record's file, in the fleet's state dir ([`crate::snapshot::state_file_path`]).
pub const BUSY_COPIES_FILE: &str = "busy-copies.json";
const BUSY_COPIES_VERSION: u32 = 1;

/// The last copy of a BUSY pod that succeeded: for which operation ([`Look::op_key`]), the work
/// tree it held ([`Look::print`], [`Look::newest`]: what the look before it saw), when (this
/// machine's clock), where (`to`: the snapshot dir; `label`: `<wNdM>/<pod>`), and where in it the
/// checkout's `.git` is (`git`, on this machine: [`copy_still_there`]; empty in a record written
/// before it was kept, which then counts as no copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusyCopy {
    pub op: String,
    pub print: String,
    pub newest: u64,
    pub at: u64,
    pub to: String,
    pub label: String,
    #[serde(default)]
    pub git: String,
}

/// Whether an earlier run's record of a copy may still be trusted — to skip this tick's copy, or
/// to resolve on: its copy is under the backup dir as it is configured now (`backup_dir`:
/// `LOCAL_BACKUP_DIR` — one changed since means the record is of another tree), and the copy's
/// `.git` is still there on this machine (a copy moved or deleted since is no copy). Else the pod
/// is copied now. Reads the disk (one `stat`).
pub fn copy_still_there(last: &BusyCopy, backup_dir: &str) -> bool {
    let (to, git) = (Path::new(&last.to), Path::new(&last.git));
    let dir = backup_dir.trim_end_matches('/');
    !dir.is_empty() && to.starts_with(dir) && to != Path::new(dir) && git.starts_with(to) && git.is_dir()
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

/// How long [`record_busy_copies`] waits for the state dir's lock. Its other holders (the balance
/// state, the health cache) hold it for a load and an atomic write; one held longer means
/// something is wrong, and this run's record is then skipped (said) — every BUSY pod is copied
/// again next run, the safe side — rather than the backup hanging on it.
pub const RECORD_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Apply `changes` to the record at `path` — each pod's new copy, or `None` to forget it (a pod
/// finished) — drop the records [`FORGET_AFTER_SECS`] old at `now`, and save it atomically
/// (owner-only), under an exclusive lock on the state dir so two runs (the cron's and an
/// operator's) can't drop each other's records. The lock is tried, not waited on, for up to
/// `wait` (blocking I/O: an async caller runs this off its runtime, `spawn_blocking`); still held
/// then — `Err` (`WouldBlock`), nothing written. A filesystem with no `flock` still gets the
/// atomic write. `Ok` carries the load warning, if the old file had to be ignored.
pub fn record_busy_copies(
    path: &Path,
    changes: &[(String, Option<BusyCopy>)],
    now: u64,
    wait: std::time::Duration,
) -> std::io::Result<Option<String>> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir)?;
    let lock = match std::fs::File::open(dir) {
        Ok(f) => {
            let deadline = std::time::Instant::now() + wait;
            loop {
                match f.try_lock() {
                    Ok(()) => break Some(f),
                    Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(std::fs::TryLockError::WouldBlock) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            format!("{} stayed locked by another arena for {}s", dir.display(), wait.as_secs_f32()),
                        ))
                    }
                    Err(std::fs::TryLockError::Error(_)) => break None,
                }
            }
        }
        Err(_) => None,
    };
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
    /// Copied whole — every rsync of both tiers, the snapshot's uncapped `.git` pass among them —
    /// and the copy held its checkout ([`copy_place`]).
    Yes,
    /// Tried, and failed in any part (or it didn't hold the checkout): never resolved this run.
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
    /// What is in progress is never finished here (am, rebase, a sequence, a revert, …).
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
/// now (`last`, which this run's own copy has just updated when there was one; the caller passes
/// only a record whose copy is still there, [`copy_still_there`]). Pure.
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

/// How a conflicted path was taken from its newer side ([`Choice`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// That side of each conflict, the clean hunks of both kept (`git merge-file`).
    Hunks,
    /// The whole file (a binary file, a symlink, a modify/delete).
    File,
    /// The whole notebook: merged per hunk, it wasn't valid JSON (Jupyter wouldn't open it).
    NotJson,
    /// The whole notebook: nothing on the pod (python3, jq) could check one merged per hunk.
    NoCheck,
    /// Removed: that side deleted it.
    Gone,
}

/// One conflicted path taken from its newer side: which side (`ours`: HEAD's; `theirs`), how, and
/// each side's last change to it (`ours`, `theirs`: committer times).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub path: String,
    pub side: String,
    pub taken: Taken,
    pub ours: u64,
    pub theirs: u64,
}

/// What a resolve said ([`parse_resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveReport {
    /// What it finished (`merge`, `cherry-pick`, `conflicts`), once finished.
    pub resolved: Option<String>,
    /// The paths taken from their newer side, removed ones among them.
    pub newer: Vec<Choice>,
    /// Paths kept as the participant resolved them.
    pub hand: Vec<String>,
    /// Paths staged with keys redacted (the file on disk keeps them).
    pub redacted: Vec<String>,
    /// Where the state before was kept (the pod's git dir).
    pub saved: Option<String>,
    /// Why it touched nothing.
    pub refused: Option<String>,
    /// The step that failed midway.
    pub failed: Option<String>,
}

/// Parse a resolve's stdout (`resolve.sh`'s header lists the lines). Unknown lines (git's own)
/// are ignored, and so is a NEWER or GONE line it can't read.
pub fn parse_resolve(stdout: &str) -> ResolveReport {
    let mut r = ResolveReport::default();
    let choice = |side: &str, taken: Taken, ours: &str, theirs: &str, path: &str| -> Option<Choice> {
        if !matches!(side, "ours" | "theirs") || path.is_empty() {
            return None;
        }
        Some(Choice { path: path.to_string(), side: side.to_string(), taken, ours: ours.parse().ok()?, theirs: theirs.parse().ok()? })
    };
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "NEWER" => {
                let f: Vec<&str> = rest.splitn(5, ' ').collect();
                if let [side, how, ours, theirs, path] = f[..] {
                    let taken = match how {
                        "hunks" => Some(Taken::Hunks),
                        "file" => Some(Taken::File),
                        "notjson" => Some(Taken::NotJson),
                        "nocheck" => Some(Taken::NoCheck),
                        _ => None,
                    };
                    r.newer.extend(taken.and_then(|t| choice(side, t, ours, theirs, path)));
                }
            }
            "GONE" => {
                if let [side, ours, theirs, path] = rest.splitn(4, ' ').collect::<Vec<_>>()[..] {
                    r.newer.extend(choice(side, Taken::Gone, ours, theirs, path));
                }
            }
            "HAND" if !rest.is_empty() => r.hand.push(rest.to_string()),
            "REDACTED" => {
                if let Some((_, path)) = rest.split_once(' ').filter(|(_, p)| !p.is_empty()) {
                    r.redacted.push(path.to_string());
                }
            }
            "SAVED" if !rest.is_empty() => r.saved = Some(rest.to_string()),
            "REFUSED" => r.refused = Some(rest.trim().to_string()),
            "FAILED" => r.failed = Some(rest.trim().to_string()),
            "RESOLVED" if !rest.trim().is_empty() => r.resolved = Some(rest.trim().to_string()),
            _ => {}
        }
    }
    r
}

/// The side an operation brings in, as the words name it: `MERGE_HEAD`, …, `the stash`.
fn other_side(what: &str) -> &'static str {
    match what {
        "merge" => "MERGE_HEAD",
        "cherry-pick" => "CHERRY_PICK_HEAD",
        "revert" => "REVERT_HEAD",
        _ => "the stash",
    }
}

/// A time as the words give it: UTC, to the minute (`2026-10-20 13:05Z`).
pub fn minute_words(t: u64) -> String {
    format!("{} {:02}:{:02}Z", crate::schedule::ymd_string((t / 86_400) as i64), t % 86_400 / 3600, t % 3600 / 60)
}

/// What a finished resolve did, for the pod's line (`hours`: the quiet period; `copy`: today's
/// copy, on this machine) — each path's way with both sides' last change to it, then where the
/// state before is: first the folder on the pod that no later copy overwrites, then today's copy
/// (which a later copy that day updates): `finished a merge left for 3h+ (README.md:
/// MERGE_HEAD's side of each conflict, HEAD 2026-10-09 12:00Z / MERGE_HEAD 2026-10-14 08:30Z;
/// notes.py: as resolved by hand; before: /root/ARENA_materials/.git/arena-resolved/20261019T140200Z/
/// on the pod; today's copy: /b/w2d1/devtest-alice)`. For a `git stash pop`'s conflicts, that its
/// stash entry is kept; keys redacted in the commit, named. Pure.
pub fn resolved_note(r: &ResolveReport, hours: u64, copy: &str) -> String {
    let what = r.resolved.as_deref().unwrap_or("merge");
    let done = match what {
        "conflicts" => format!("resolved the conflicts a `git stash pop` left for {hours}h+ (its stash entry kept)"),
        w => format!("finished a {w} left for {hours}h+"),
    };
    let other = other_side(what);
    let mut ways: Vec<String> = r
        .newer
        .iter()
        .map(|c| {
            let side = if c.side == "ours" { "HEAD" } else { other };
            let way = match c.taken {
                Taken::Hunks => format!("{side}'s side of each conflict"),
                Taken::File => format!("{side}'s version"),
                Taken::NotJson => format!("{side}'s version, whole: merged per hunk the notebook wasn't valid JSON"),
                Taken::NoCheck => format!("{side}'s version, whole: no python3 or jq on the pod to check the notebook merged per hunk"),
                Taken::Gone => format!("removed, as on {side}"),
            };
            format!("{}: {way}, HEAD {} / {other} {}", c.path, minute_words(c.ours), minute_words(c.theirs))
        })
        .collect();
    ways.extend(r.hand.iter().map(|p| format!("{p}: as resolved by hand")));
    let ways = if ways.is_empty() { "nothing was left in conflict".to_string() } else { ways.join("; ") };
    let keys = if r.redacted.is_empty() { String::new() } else { format!("; keys redacted in the commit (kept on disk): {}", r.redacted.join(", ")) };
    let before = r.saved.as_ref().map_or_else(String::new, |s| format!("before: {}/ on the pod; ", s.trim_end_matches('/')));
    format!("{done} ({ways}{keys}; {before}today's copy: {copy})")
}

#[cfg(test)]
mod tests;
