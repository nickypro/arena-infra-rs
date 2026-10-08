//! Detached course-test jobs on pods: `pods run --background`, `pods jobs`, `pods logs`.
//!
//! A course-test run (the week's notebooks under pytest, a training smoke test) outlasts
//! any SSH session worth holding open, and a dropped laptop connection must not kill it.
//! So a *job* is started detached on each pod, and everything about it stays on the pod,
//! under `~/.arena/jobs/<id>/`:
//!
//! ```text
//!   cmd         the command as typed         log   its stdout + stderr
//!   run         the wrapper that runs it     pid   the wrapper's pid (= its process group)
//!   started_at  UTC start (the pod's clock)  exit  the exit code, written when it ends
//! ```
//!
//! Nothing about a job lives on the control machine: `pods jobs` / `pods logs` read these
//! files back, so a second operator — or this one after a laptop crash — sees the same state.
//!
//! **Detaching**: `setsid -f nohup sh run >log 2>&1 </dev/null`. `setsid` gives the wrapper
//! its own session and process group (no controlling terminal; the group is what `--kill`
//! signals), `-f` forks it off so the start script carries on at once, `nohup` makes it
//! immune to a hangup, and redirecting all three standard streams is what lets the SSH call
//! return at once — sshd keeps the session open while anything still holds its pipes. Not
//! `setsid … &`: a non-interactive shell starts an `&` list with SIGINT and SIGQUIT
//! *ignored*, a shell can't undo an ignore it inherited, and so the command — python, its
//! subprocesses — would run deaf to SIGINT (no `KeyboardInterrupt`, a test that interrupts
//! a child hangs), unlike under a foreground `pods run`. Only a `setsid` without `-f`
//! (util-linux < 2.31, busybox) falls back to `&`, with that limitation.
//!
//! The wrapper runs the command under the same login-shell/conda wrap as `pods run`
//! ([`crate::ssh::login_shell_wrap`]), records its own pid first and the exit code last
//! (each via a rename, so a reader never sees half a file), and traps TERM so that killing
//! the group stops the *command* while the wrapper lives on to record `143`. If it can't
//! record its pid it doesn't run the command at all (exit `125`): a job nobody can see
//! running, or stop, is worse than one that didn't start. `PYTHONUNBUFFERED=1`: python
//! block-buffers a redirected stdout, so without it a job's log — and `logs --follow` —
//! would sit silent for minutes.
//!
//! **Quoting**: the command and the wrapper travel base64-encoded inside the start script
//! (no quote, `$`, backslash or newline in a command can break out of anything), every
//! script runs under `sh -c` (POSIX sh, whatever the pod's login shell is), and the only
//! other values spliced in are a [`JobId`] (alphabet `[0-9a-z-]`) and integers.
//!
//! **Status** is judged on the pod, by one shell function shared by every read: an `exit`
//! file → exited with that code; else the recorded pid is alive *and is still this job's
//! wrapper* (its cmdline names this job's `run` — a bare pid could have been recycled) →
//! running; else the `exit` file is looked for *again* — the wrapper writes it just before
//! it exits, so a job that ended between the two looks has it now and mustn't read as
//! lost; else a pid but no exit code → lost (SIGKILLed, OOM-killed, or the pod restarted);
//! else no pid yet → starting, for a minute or so — a wrapper records its pid first thing
//! or not at all, so a job without one after that never will be tracked, and is lost too
//! (else `logs -f` would follow it forever).
//!
//! **Logs** come back base64-encoded with their byte offsets, so `--follow` can ask for
//! exactly the bytes it hasn't seen (no duplicated or dropped lines, whatever the log
//! holds), and a log line can never be mistaken for one of the reply's marker lines.
//! Lines are made safe for the operator's terminal before printing ([`sanitize`]): a
//! participant's program can print escape sequences, and those must not reach — let alone
//! drive — the operator's terminal.
//!
//! Everything here is pure (command builders, reply parsers, renderers) and table-tested;
//! the CLI does the SSH, every call bounded.

use std::fmt;

use crate::base64;
use crate::table::{self, Align};

/// Where a pod keeps its jobs, under `$HOME`.
pub const JOBS_DIR: &str = ".arena/jobs";

/// Most log bytes one read returns (before base64). A snapshot shows the last lines of
/// at most this much; a follow that falls further behind than this between two reads
/// skips ahead (and says so) rather than paging through a flood.
pub const LOG_READ_CAP: u64 = 1024 * 1024;

/// Longest command slug in a [`JobId`].
const SLUG_MAX: usize = 24;

/// Longest id accepted from the operator (a generated one is at most 40).
const ID_MAX: usize = 64;

/// Longest unfinished line a follow holds back waiting for its newline (a progress bar
/// redrawn with `\r` never prints one): past this it is shown as is, so memory stays
/// bounded.
const PARTIAL_MAX: usize = 64 * 1024;

/// A job's id: `YYYYMMDD-HHMMSS-<slug>` (UTC start time, then a slug of the command), e.g.
/// `20261008-142301-pytest-x-tests`. Sorting ids sorts jobs by start time, the same id
/// names the run on every pod of a fleet start, and the alphabet (`[0-9a-z-]`) is safe to
/// splice into a remote path or shell word — which is why operator input goes through
/// [`JobId::parse`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(String);

impl JobId {
    /// The id for `cmd` started at `unix_secs` (UTC).
    pub fn generate(unix_secs: u64, cmd: &str) -> JobId {
        let (y, m, d) = crate::schedule::civil_from_days((unix_secs / 86_400) as i64);
        let s = unix_secs % 86_400;
        JobId(format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{}", s / 3600, s % 3600 / 60, s % 60, slug(cmd)))
    }

    /// An id the operator typed: must have the generated shape and alphabet — anything else
    /// is refused before it gets near a remote path.
    pub fn parse(s: &str) -> Result<JobId, String> {
        let ok = looks_like_job_id(s)
            && s.len() <= ID_MAX
            && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-');
        if ok {
            Ok(JobId(s.to_string()))
        } else {
            Err(format!(
                "`{s}` is not a job id — ids look like 20261008-142301-pytest (see `arena pods jobs`)"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Does `s` have a job id's shape (`8 digits - 6 digits`, then optionally `-…`)? How
/// `pods logs` tells a JOB argument from a pod target: no pod name or provider id looks
/// like this.
pub fn looks_like_job_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 15
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && (b.len() == 15 || b[15] == b'-')
}

/// Split `pods logs`' positional words into pod targets and at most one JOB (told apart by
/// [`looks_like_job_id`], so they can come in any order). A job-shaped word that isn't a
/// valid id, or two of them, is an error.
pub fn split_job_arg(words: &[String]) -> Result<(Vec<String>, Option<JobId>), String> {
    let (jobs, targets): (Vec<&String>, Vec<&String>) = words.iter().partition(|w| looks_like_job_id(w));
    let job = match jobs.as_slice() {
        [] => None,
        [one] => Some(JobId::parse(one)?),
        many => {
            let many: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
            return Err(format!("one job at a time — got {}", many.join(", ")));
        }
    };
    Ok((targets.into_iter().cloned().collect(), job))
}

/// The id's tail: the command's words, lowercased, as `[a-z0-9]` runs joined by `-`,
/// at most [`SLUG_MAX`] long (a word the limit cuts in half is dropped, unless it's the
/// only one) — just enough to recognise the job in a listing.
fn slug(cmd: &str) -> String {
    let mut s = String::new();
    for c in cmd.chars() {
        if s.len() >= SLUG_MAX {
            if c.is_ascii_alphanumeric() && !s.ends_with('-') {
                if let Some(cut) = s.rfind('-') {
                    s.truncate(cut);
                }
            }
            break;
        }
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-');
    if s.is_empty() {
        "job".to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------------------
// Remote commands
// ---------------------------------------------------------------------------------------

/// `sh -c '<script>'`, single-quoted for whatever login shell sshd hands it to.
fn sh_c(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', "'\\''"))
}

/// Shell functions every read shares. `ours PID ID`: is PID alive and still this job's
/// wrapper? `judge ID`: set `$state`/`$code`/`$pid`/`$started` (see the module doc for the
/// order of the looks — the second `ended` closes the race with a wrapper that exits
/// between the first look and the liveness check). `show ID`: print the judged job's
/// `ARENA_JOB` status line; `info ID` = both. Each value is filtered to the characters it
/// may contain, and the command is clipped and flattened to one line, so a damaged file
/// can't break the reply. `find -mmin +1`: the job directory is over a minute old (GNU
/// find; busybox rounds down, so two) — it last changed when the job was started, since a
/// job without a pid wrote nothing after.
/// `LC_ALL=C`: byte-wise globs, `tr` and `sort` (only the reads use this — never a job).
const PRELUDE: &str = r#"LC_ALL=C; export LC_ALL
J="$HOME/.arena/jobs"
ours() {
  [ -r "/proc/$1/cmdline" ] && tr '\000' ' ' < "/proc/$1/cmdline" 2>/dev/null | grep -qF -- "/.arena/jobs/$2/run"
}
ended() {
  [ -f "$d/exit" ] || return 1
  state=exit; code=$(head -c 20 "$d/exit" 2>/dev/null | tr -cd '0-9-')
}
judge() {
  d="$J/$1"
  started=$(head -c 40 "$d/started_at" 2>/dev/null | tr -cd '0-9TZ:-')
  pid=$(head -c 20 "$d/pid" 2>/dev/null | tr -cd '0-9')
  code=-
  if ended; then
    :
  elif [ -n "$pid" ] && ours "$pid" "$1"; then
    state=running
  elif ended; then
    :
  elif [ -n "$pid" ]; then
    state=lost
  elif [ -n "$(find "$d" -prune -mmin +1 2>/dev/null)" ]; then
    state=lost
  else
    state=starting
  fi
}
show() {
  cmd=$(head -c 300 "$d/cmd" 2>/dev/null | tr '\t\r\n' '   ')
  printf 'ARENA_JOB\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "${started:--}" "$state" "${code:--}" "${pid:--}" "$cmd"
}
info() {
  judge "$1"; show "$1"
}
"#;

/// Start a job (see the module doc). Refuses — before creating anything — when a tool it
/// needs is missing, and never reuses a job directory (`mkdir` without `-p`): an id clash
/// is an error, not an overwrite. The job's directory is private (umask 077: its log can
/// hold tokens a test printed), but the job itself runs under the login's own umask.
/// Detaches with `setsid -f` where `setsid` has it (probed by running `true` with it), else
/// `&` (see the module doc for why that's only the fallback). Waits up to 5s for the
/// wrapper's pid — or its exit code, which without a pid means it refused to run the
/// command. No pid and no exit code by then is *not* a clean failure: the wrapper may
/// still be about to run it, so it is reported as `ARENA_JOB_UNSURE`, never as "didn't
/// start" (a retry would run a second copy).
const START: &str = r#"mask=$(umask); umask 077
J="$HOME/.arena/jobs"; d="$J/@ID@"
for t in setsid nohup base64 head tail; do
  command -v "$t" >/dev/null 2>&1 || { echo "ARENA_JOB_ERR $t not found on the pod"; exit 3; }
done
mkdir -p "$J" 2>/dev/null || { echo "ARENA_JOB_ERR cannot create $J"; exit 3; }
mkdir "$d" 2>/dev/null || { echo "ARENA_JOB_ERR job @ID@ already exists on this pod (or $J is not writable)"; exit 3; }
if ! { printf %s @CMD64@ | base64 -d > "$d/cmd" && printf %s @RUN64@ | base64 -d > "$d/run"; }; then
  echo "ARENA_JOB_ERR cannot write the job files in $d"; exit 3
fi
date -u +%Y-%m-%dT%H:%M:%SZ > "$d/started_at"
umask "$mask"
if setsid -f true > /dev/null 2>&1; then
  setsid -f nohup sh "$d/run" > "$d/log" 2>&1 < /dev/null
else
  setsid nohup sh "$d/run" > "$d/log" 2>&1 < /dev/null &
fi
i=0
while [ ! -s "$d/pid" ] && [ ! -f "$d/exit" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
pid=$(head -c 20 "$d/pid" 2>/dev/null | tr -cd '0-9')
if [ -z "$pid" ]; then
  if [ -f "$d/exit" ]; then
    echo "ARENA_JOB_ERR the job could not record its pid, so it did not run the command (see $d/log)"; exit 3
  fi
  echo "ARENA_JOB_UNSURE $d/log"; exit 3
fi
echo "ARENA_JOB_STARTED @ID@ $pid"
"#;

/// The wrapper (`run`): pid first (one it can't record means exit `125` without running the
/// command — see the module doc), exit code last, TERM trapped (a handler, not ignored: an
/// ignored signal would be inherited by the command and make it unkillable).
const RUN: &str = r#"# arena job @ID@ — started by `arena pods run --background`; see `arena pods logs @ID@`.
d=$(dirname -- "$0")
trap : INT TERM
if ! { echo $$ > "$d/pid.tmp" && mv -f "$d/pid.tmp" "$d/pid"; }; then
  echo "arena: could not record this job's pid in $d — not running it (exit 125)" >&2
  echo 125 > "$d/exit.tmp" && mv -f "$d/exit.tmp" "$d/exit"
  exit 125
fi
export PYTHONUNBUFFERED=1
@WRAPPED@
code=$?
echo "$code" > "$d/exit.tmp" && mv -f "$d/exit.tmp" "$d/exit"
"#;

/// Every job's status line, then the end marker (its absence = the script didn't finish).
const LIST: &str = r#"if [ -d "$J" ]; then
  for p in "$J"/*; do
    [ -d "$p" ] || continue
    id=${p##*/}
    case "$id" in *[!0-9a-z-]*) continue ;; esac
    case "$id" in [0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9]*) info "$id" ;; esac
  done
fi
echo ARENA_JOBS_END
"#;

/// One job's status plus a window of its log: bytes `[start, size)`, base64 on one line.
/// `start` is `from` (the follow's offset) — or, on a first read, after a truncation, or
/// when more than the cap is waiting, the last `cap` bytes.
const READ: &str = r#"id=@ID@
if [ -z "$id" ]; then
  id=$(ls -1 "$J" 2>/dev/null | grep -E '^[0-9]{8}-[0-9]{6}(-[0-9a-z-]*)?$' | LC_ALL=C sort | tail -n 1)
fi
if [ -z "$id" ]; then echo ARENA_JOBS_NONE; echo ARENA_JOBS_END; exit 0; fi
if [ ! -d "$J/$id" ]; then echo ARENA_JOB_MISSING; echo ARENA_JOBS_END; exit 0; fi
info "$id"
f="$J/$id/log"
size=$(wc -c < "$f" 2>/dev/null | tr -cd '0-9'); size=${size:-0}
from=@FROM@; cap=@CAP@
if [ -z "$from" ] || [ "$from" -gt "$size" ] || [ $((size - from)) -gt "$cap" ]; then
  if [ "$size" -gt "$cap" ]; then start=$((size - cap)); else start=0; fi
else
  start=$from
fi
printf 'ARENA_LOG\t%s\t%s\n' "$start" "$size"
tail -c +$((start + 1)) "$f" 2>/dev/null | head -c $((size - start)) | base64 | tr -d '\n'
echo
echo ARENA_JOBS_END
"#;

/// SIGTERM to a running job's process group — only once `judge` has confirmed the pid is
/// still this job's wrapper, and only if that pid leads its own group (`setsid` made it
/// so; anything else means it isn't ours to signal). A job that wasn't signalled is judged
/// again before that's called a failure: one that ended in between (its `/proc` entry gone,
/// so no group to read, or nothing left to signal) was simply not running any more. The
/// status line printed is the last judgement.
const KILL: &str = r#"id=@ID@
if [ ! -d "$J/$id" ]; then echo ARENA_JOB_MISSING; echo ARENA_JOBS_END; exit 0; fi
judge "$id"
verdict=SKIPPED
if [ "$state" = running ]; then
  pgrp=$(sed -e 's/^.*) //' "/proc/$pid/stat" 2>/dev/null | cut -d' ' -f3)
  if [ "$pgrp" = "$pid" ] && kill -s TERM -- "-$pid" 2>/dev/null; then
    verdict=SENT
  else
    judge "$id"
    if [ "$state" != running ]; then
      verdict=SKIPPED
    elif [ "$pgrp" != "$pid" ]; then
      verdict=REFUSED
    else
      verdict=FAILED
    fi
  fi
fi
show "$id"
echo "ARENA_KILL_$verdict"
echo ARENA_JOBS_END
"#;

/// The wrapper script a job runs (`~/.arena/jobs/<id>/run`): `cmd` under the login-shell /
/// conda wrap `pods run` uses. Shown by `--dry-run`.
pub fn run_script(id: &JobId, cmd: &str, conda_env: Option<&str>) -> String {
    RUN.replace("@ID@", id.as_str()).replace("@WRAPPED@", &crate::ssh::login_shell_wrap(cmd, conda_env))
}

/// The remote command that starts `cmd` as job `id` (see [`START`]). Its reply is read by
/// [`parse_started`].
pub fn start_command(id: &JobId, cmd: &str, conda_env: Option<&str>) -> String {
    sh_c(&start_script(id, cmd, conda_env))
}

fn start_script(id: &JobId, cmd: &str, conda_env: Option<&str>) -> String {
    // The base64 payloads can't contain `@`, so a command holding the text `@ID@` stays as
    // typed (it is encoded before the id is filled in).
    START
        .replace("@CMD64@", &base64::encode(cmd.as_bytes()))
        .replace("@RUN64@", &base64::encode(run_script(id, cmd, conda_env).as_bytes()))
        .replace("@ID@", id.as_str())
}

/// The remote command listing a pod's jobs; read by [`parse_list`].
pub fn list_command() -> String {
    sh_c(&list_script())
}

fn list_script() -> String {
    format!("{PRELUDE}{LIST}")
}

/// The remote command reading job `job` (default: the pod's newest) — its status and a
/// window of its log from byte `from` (default: the last [`LOG_READ_CAP`] bytes); read by
/// [`parse_read`].
pub fn read_command(job: Option<&JobId>, from: Option<u64>) -> String {
    sh_c(&read_script(job, from))
}

fn read_script(job: Option<&JobId>, from: Option<u64>) -> String {
    let script = READ
        .replace("@ID@", job.map_or("", JobId::as_str))
        .replace("@FROM@", &from.map(|f| f.to_string()).unwrap_or_default())
        .replace("@CAP@", &LOG_READ_CAP.to_string());
    format!("{PRELUDE}{script}")
}

/// The remote command that stops job `id` (SIGTERM to its process group); read by
/// [`parse_kill`].
pub fn kill_command(id: &JobId) -> String {
    sh_c(&kill_script(id))
}

fn kill_script(id: &JobId) -> String {
    format!("{PRELUDE}{}", KILL.replace("@ID@", id.as_str()))
}

// ---------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------

/// Where a job is in its life, as the pod judged it (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Started, but the wrapper hasn't recorded its pid yet (for a minute or so; then lost).
    Starting,
    Running,
    /// Ended, with this exit code (`128 + n` = killed by signal `n`).
    Exited(i32),
    /// Ended without an exit code: killed with SIGKILL, OOM-killed, or the pod restarted —
    /// or never tracked (no pid recorded a minute after the start).
    Lost,
}

impl JobState {
    /// Over, one way or another — nothing more will be written.
    pub fn is_over(self) -> bool {
        matches!(self, JobState::Exited(_) | JobState::Lost)
    }

    /// Over, and not with exit 0.
    pub fn failed(self) -> bool {
        matches!(self, JobState::Exited(c) if c != 0) || self == JobState::Lost
    }
}

/// One job, as a pod reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: JobId,
    /// `2026-10-08T14:23:01Z` (the pod's clock); `None` if the file was missing.
    pub started: Option<String>,
    pub state: JobState,
    pub pid: Option<u32>,
    /// The command (first 300 bytes, on one line, made terminal-safe).
    pub cmd: String,
}

/// A window of a job's log: bytes `[start, start + bytes.len())` of a log `size` long.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogChunk {
    pub start: u64,
    pub size: u64,
    pub bytes: Vec<u8>,
}

impl LogChunk {
    /// The offset just past this window — where the next read starts.
    pub fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }
}

/// A pod's answer to [`read_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadReply {
    /// The pod has no jobs at all (asked for its newest).
    NoJobs,
    /// The pod has no job with the asked-for id.
    Missing,
    Job { info: JobInfo, chunk: LogChunk },
}

/// A pod's answer to [`kill_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillReply {
    /// No such job on this pod.
    Missing,
    /// SIGTERM went to the job's process group.
    Sent(JobInfo),
    /// Not running (already over, or not started), so nothing was signalled.
    Skipped(JobInfo),
    /// The pid doesn't lead its own process group, so it wasn't signalled.
    Refused(JobInfo),
    /// `kill` itself failed.
    Failed(JobInfo),
}

/// The lines of a reply up to its end marker. Without the marker the script didn't run to
/// the end (or something else answered), so nothing in it is trusted.
fn until_end(stdout: &str) -> Result<Vec<&str>, String> {
    let lines: Vec<&str> = stdout.lines().map(|l| l.trim_end_matches('\r')).collect();
    let end = lines
        .iter()
        .position(|l| *l == "ARENA_JOBS_END")
        .ok_or_else(|| format!("unexpected reply from the pod: {}", first_words(stdout)))?;
    Ok(lines[..end].to_vec())
}

/// The start of an unexpected reply, for an error message (clipped, made safe).
fn first_words(stdout: &str) -> String {
    let t = sanitize(stdout.trim());
    if t.is_empty() {
        "(no output)".into()
    } else {
        crate::fleet::clip(&t.replace('\n', " | "), 120)
    }
}

/// One `ARENA_JOB` status line. Strict: every field must have the shape the script prints.
fn parse_job_line(line: &str) -> Result<JobInfo, String> {
    let bad = || format!("malformed job line from the pod: {}", crate::fleet::clip(&sanitize(line), 120));
    let f: Vec<&str> = line.splitn(7, '\t').collect();
    if f.len() != 7 || f[0] != "ARENA_JOB" {
        return Err(bad());
    }
    let id = JobId::parse(f[1]).map_err(|_| bad())?;
    let state = match f[3] {
        "starting" => JobState::Starting,
        "running" => JobState::Running,
        "lost" => JobState::Lost,
        "exit" => JobState::Exited(f[4].parse().map_err(|_| bad())?),
        _ => return Err(bad()),
    };
    let pid = match f[5] {
        "-" => None,
        p => Some(p.parse().map_err(|_| bad())?),
    };
    let started = (f[2] != "-").then(|| sanitize(f[2]));
    Ok(JobInfo { id, started, state, pid, cmd: sanitize(f[6]).trim().to_string() })
}

/// Lines that are ours (`ARENA_…`) — anything else is noise from the pod's shell startup
/// and is skipped; an `ARENA_` line we don't know is an error.
fn is_marker(line: &str) -> bool {
    line.starts_with("ARENA_")
}

/// [`list_command`]'s reply: the pod's jobs, newest first.
pub fn parse_list(stdout: &str) -> Result<Vec<JobInfo>, String> {
    let mut jobs = Vec::new();
    for line in until_end(stdout)?.into_iter().filter(|l| is_marker(l)) {
        jobs.push(parse_job_line(line)?);
    }
    jobs.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(jobs)
}

/// [`read_command`]'s reply. Checks the log window is consistent (`start ≤ size`, no more
/// bytes than the window holds) — a garbled reply is an error, never shown as log.
pub fn parse_read(stdout: &str) -> Result<ReadReply, String> {
    let lines = until_end(stdout)?;
    let mut ours = lines.iter().enumerate().filter(|(_, l)| is_marker(l));
    let Some((_, first)) = ours.next() else {
        return Err(format!("unexpected reply from the pod: {}", first_words(stdout)));
    };
    match *first {
        "ARENA_JOBS_NONE" => return Ok(ReadReply::NoJobs),
        "ARENA_JOB_MISSING" => return Ok(ReadReply::Missing),
        _ => {}
    }
    let info = parse_job_line(first)?;
    let Some((at, log)) = ours.next() else {
        return Err("the pod sent the job's status but not its log".into());
    };
    let bad = || format!("malformed log header from the pod: {}", crate::fleet::clip(&sanitize(log), 80));
    let f: Vec<&str> = log.split('\t').collect();
    let [marker, start, size] = f.as_slice() else { return Err(bad()) };
    if *marker != "ARENA_LOG" {
        return Err(bad());
    }
    let (start, size): (u64, u64) = (start.parse().map_err(|_| bad())?, size.parse().map_err(|_| bad())?);
    let payload = lines.get(at + 1).copied().unwrap_or("");
    let bytes = base64::decode(payload.trim()).map_err(|e| format!("the pod's log bytes don't decode ({e})"))?;
    if start > size || bytes.len() as u64 > size - start {
        return Err(format!("inconsistent log window from the pod (start {start}, size {size}, {} bytes)", bytes.len()));
    }
    Ok(ReadReply::Job { info, chunk: LogChunk { start, size, bytes } })
}

/// [`start_command`]'s reply: the job's pid. The script's own complaint (`ARENA_JOB_ERR`)
/// is the error when it has one, and a start it can't vouch for either way
/// (`ARENA_JOB_UNSURE`) says the job may be running; a reply naming a different id is
/// refused.
pub fn parse_started(stdout: &str, id: &JobId) -> Result<u32, String> {
    for line in stdout.lines().map(|l| l.trim_end_matches('\r')) {
        if let Some(why) = line.strip_prefix("ARENA_JOB_ERR ") {
            return Err(sanitize(why));
        }
        if let Some(log) = line.strip_prefix("ARENA_JOB_UNSURE ") {
            return Err(format!(
                "no pid from the job within 5s — it may have started anyway: check `arena pods jobs` before \
                 retrying (its log: {})",
                sanitize(log)
            ));
        }
        if let Some(rest) = line.strip_prefix("ARENA_JOB_STARTED ") {
            return match rest.split_once(' ') {
                Some((got, pid)) if got == id.as_str() => {
                    pid.parse().map_err(|_| format!("the pod reported a bad pid: {}", sanitize(pid)))
                }
                _ => Err(format!("unexpected start reply: {}", sanitize(line))),
            };
        }
    }
    Err(format!("no start confirmation from the pod: {}", first_words(stdout)))
}

/// [`kill_command`]'s reply.
pub fn parse_kill(stdout: &str) -> Result<KillReply, String> {
    let lines = until_end(stdout)?;
    let ours: Vec<&str> = lines.into_iter().filter(|l| is_marker(l)).collect();
    match ours.as_slice() {
        ["ARENA_JOB_MISSING"] => Ok(KillReply::Missing),
        [job, verdict] => {
            let info = parse_job_line(job)?;
            match *verdict {
                "ARENA_KILL_SENT" => Ok(KillReply::Sent(info)),
                "ARENA_KILL_SKIPPED" => Ok(KillReply::Skipped(info)),
                "ARENA_KILL_REFUSED" => Ok(KillReply::Refused(info)),
                "ARENA_KILL_FAILED" => Ok(KillReply::Failed(info)),
                other => Err(format!("unexpected kill reply: {}", sanitize(other))),
            }
        }
        _ => Err(format!("unexpected kill reply: {}", first_words(stdout))),
    }
}

// ---------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------

/// Make text from a pod safe to print on the operator's terminal: ANSI/OSC escape
/// sequences are removed (colours included — they're cosmetic, and the same channel can
/// retitle a window or write the clipboard), and other control characters are dropped,
/// tabs aside.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                // CSI: parameters up to a final byte in @..~.
                Some('[') => {
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC (and the other string sequences): up to BEL or ESC \.
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = it.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && it.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // A two-character escape (or a lone ESC at the end).
                _ => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// What a terminal would show for one raw log line: invalid UTF-8 replaced, a CRLF's `\r`
/// dropped, only the text after the last `\r` kept (a progress bar redraws itself that
/// way — this is its final state), and made safe ([`sanitize`]).
pub fn display_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    sanitize(text.rsplit('\r').next().unwrap_or(""))
}

/// Complete (`\n`-terminated) lines, without their `\n`, and the unfinished rest.
fn split_lines(bytes: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    let rest = lines.pop().unwrap_or(&[]);
    (lines, rest)
}

/// `signal` names for the exit codes a killed job reports (`128 + n`).
fn signal_name(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// A job's status for a listing: `running (pid 812)`, `exit 0`, `exit 143 (SIGTERM)`, …
pub fn state_label(info: &JobInfo) -> String {
    match info.state {
        JobState::Starting => "starting".into(),
        JobState::Running => match info.pid {
            Some(pid) => format!("running (pid {pid})"),
            None => "running".into(),
        },
        JobState::Exited(c) => match if c > 128 { signal_name(c - 128) } else { None } {
            Some(sig) => format!("exit {c} ({sig})"),
            None => format!("exit {c}"),
        },
        JobState::Lost => "lost (ended without an exit code)".into(),
    }
}

/// The `pods jobs` report: one table row per job (pods by name, each pod's jobs newest
/// first; `only` keeps one job), then a line for each pod with none and each pod that
/// couldn't be read. Returns the text and how many pods failed.
pub fn render_jobs(pods: &[(String, Result<Vec<JobInfo>, String>)], only: Option<&JobId>) -> (String, usize) {
    let mut sorted: Vec<&(String, Result<Vec<JobInfo>, String>)> = pods.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut rows, mut notes, mut failed, mut running, mut jobs) = (Vec::new(), Vec::new(), 0, 0, 0);
    for (name, result) in sorted {
        match result {
            Err(why) => {
                failed += 1;
                notes.push(format!("✗ {name}: {why}"));
            }
            Ok(list) => {
                let list: Vec<&JobInfo> = list.iter().filter(|j| only.is_none_or(|o| &j.id == o)).collect();
                if list.is_empty() {
                    notes.push(match only {
                        Some(id) => format!("{name}: no job {id}"),
                        None => format!("{name}: no jobs"),
                    });
                }
                for j in list {
                    jobs += 1;
                    running += usize::from(j.state == JobState::Running);
                    rows.push(vec![
                        name.clone(),
                        j.id.to_string(),
                        j.started.clone().unwrap_or_else(|| "-".into()),
                        state_label(j),
                        crate::fleet::clip(&j.cmd, 60),
                    ]);
                }
            }
        }
    }
    let mut out = String::new();
    if !rows.is_empty() {
        out.push_str(&table::render(&["POD", "JOB", "STARTED (UTC)", "STATUS", "COMMAND"], &[Align::Left], &rows));
    }
    for n in notes {
        out.push_str(&n);
        out.push('\n');
    }
    out.push_str(&format!("\n{jobs} job(s), {running} running"));
    if failed > 0 {
        out.push_str(&format!("; {failed} pod(s) not read"));
    }
    out.push('\n');
    (out, failed)
}

/// The `pods jobs --kill` report: a line per pod, and how many failed (unreachable, or a
/// running job that couldn't be signalled). A pod without the job, or where it has already
/// ended, is only a note: one id names the run on every pod it started on, and the
/// selection may well be wider.
pub fn render_kill(pods: &[(String, Result<KillReply, String>)], id: &JobId) -> (Vec<String>, usize) {
    let pid = |info: &JobInfo| info.pid.map_or_else(|| "?".to_string(), |p| p.to_string());
    let mut failed = 0;
    let lines = pods
        .iter()
        .map(|(name, reply)| match reply {
            Ok(KillReply::Sent(info)) => format!("✓ {name}: SIGTERM sent to job {id} (process group {})", pid(info)),
            Ok(KillReply::Skipped(info)) => format!("– {name}: job {id} is not running ({})", state_label(info)),
            Ok(KillReply::Missing) => format!("– {name}: no job {id}"),
            Ok(KillReply::Refused(info)) => {
                failed += 1;
                format!("✗ {name}: pid {} doesn't lead its own process group — not signalled", pid(info))
            }
            Ok(KillReply::Failed(info)) => {
                failed += 1;
                format!("✗ {name}: kill failed (pid {})", pid(info))
            }
            Err(why) => {
                failed += 1;
                format!("✗ {name}: {why}")
            }
        })
        .collect();
    (lines, failed)
}

/// The header over a job's log in `pods logs`, then its command.
pub fn job_header(pod: &str, info: &JobInfo) -> Vec<String> {
    let started = info.started.as_deref().map(|s| format!(" · started {s}")).unwrap_or_default();
    vec![format!("── {pod} · {} · {}{started}", info.id, state_label(info)), format!("$ {}", info.cmd)]
}

/// The last `tail` lines of a log window for a one-off `pods logs` — an unfinished last
/// line included, as it stands — and whether older lines were cut off by the read cap
/// (so the operator knows there is more than is shown). A window starting mid-file
/// starts mid-line, so its first line is dropped.
pub fn snapshot_lines(chunk: &LogChunk, tail: usize) -> (Vec<String>, bool) {
    let (mut lines, rest) = split_lines(&chunk.bytes);
    if chunk.start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let mut shown: Vec<String> = lines.into_iter().map(display_line).collect();
    if !rest.is_empty() {
        shown.push(display_line(rest));
    }
    let cut = chunk.start > 0 && shown.len() < tail;
    shown.drain(..shown.len().saturating_sub(tail));
    (shown, cut)
}

/// One pod's position in a `pods logs --follow`: the next byte to read and an unfinished
/// line held back until its newline arrives (so a line split across two reads prints
/// once, whole).
#[derive(Debug, Clone, Default)]
pub struct Follow {
    offset: u64,
    partial: Vec<u8>,
}

impl Follow {
    /// Start following after a first read: its last `tail` complete lines (and whether the
    /// read cap cut older ones), with the unfinished last line held back.
    pub fn first(chunk: &LogChunk, tail: usize) -> (Follow, Vec<String>, bool) {
        let (mut lines, rest) = split_lines(&chunk.bytes);
        if chunk.start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        let cut = chunk.start > 0 && lines.len() < tail;
        let mut shown: Vec<String> = lines[lines.len().saturating_sub(tail)..].iter().map(|l| display_line(l)).collect();
        let mut f = Follow { offset: chunk.end(), partial: rest.to_vec() };
        shown.extend(f.bound_partial());
        (f, shown, cut)
    }

    /// Where the next read starts.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// A later read: its complete lines. If the pod didn't start where we asked (the log
    /// was truncated, or more than the cap arrived since the last read) a note says so
    /// first and the held line is dropped — it can't be completed any more.
    pub fn next(&mut self, chunk: &LogChunk) -> Vec<String> {
        let mut out = Vec::new();
        if chunk.start != self.offset {
            out.push(if chunk.start < self.offset {
                "… the log shrank (truncated?) — reading it again from here".to_string()
            } else {
                format!("… skipped {} bytes (more than {} KiB arrived between two reads)", chunk.start - self.offset, LOG_READ_CAP / 1024)
            });
            self.partial.clear();
        }
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&chunk.bytes);
        let (lines, rest) = split_lines(&data);
        out.extend(lines.into_iter().map(display_line));
        self.partial = rest.to_vec();
        out.extend(self.bound_partial());
        self.offset = chunk.end();
        out
    }

    /// The job is over: the held line, if any (a last line printed without a newline).
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.partial);
        (!rest.is_empty()).then(|| display_line(&rest))
    }

    /// An unfinished line past [`PARTIAL_MAX`], shown now rather than held forever.
    fn bound_partial(&mut self) -> Option<String> {
        (self.partial.len() > PARTIAL_MAX).then(|| display_line(&std::mem::take(&mut self.partial)))
    }
}

/// Canned pod replies in the scripts' exact format, for tests here and in the CLI (which
/// can't reach this crate's private base64).
#[cfg(any(test, feature = "test-util"))]
pub mod fixtures {
    use super::*;

    /// An `ARENA_JOB` status line (`code` only for `exit`).
    pub fn job_line(id: &str, state: &str, code: Option<i32>, pid: Option<u32>, cmd: &str) -> String {
        format!(
            "ARENA_JOB\t{id}\t2026-10-08T14:23:01Z\t{state}\t{}\t{}\t{cmd}",
            code.map_or("-".into(), |c| c.to_string()),
            pid.map_or("-".into(), |p| p.to_string())
        )
    }

    /// [`read_command`]'s reply for a job: its status line and log bytes `[start, size)`.
    pub fn read_reply(job_line: &str, start: u64, size: u64, bytes: &[u8]) -> String {
        format!("{job_line}\nARENA_LOG\t{start}\t{size}\n{}\nARENA_JOBS_END\n", base64::encode(bytes))
    }

    /// [`list_command`]'s reply.
    pub fn list_reply(job_lines: &[String]) -> String {
        let mut s: String = job_lines.iter().map(|l| format!("{l}\n")).collect();
        s.push_str("ARENA_JOBS_END\n");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn id(s: &str) -> JobId {
        JobId::parse(s).unwrap()
    }

    #[test]
    fn ids_sort_by_start_time_and_slug_the_command() {
        // 2026-10-08 14:23:01 UTC
        let t = 1_791_469_381;
        for (cmd, want) in [
            ("pytest -x tests/ -k 'part1'", "20261008-142301-pytest-x-tests-k-part1"),
            ("  /usr/bin/python3 run.py  ", "20261008-142301-usr-bin-python3-run-py"),
            ("echo \"$HOME\"; ls\nrm -rf x", "20261008-142301-echo-home-ls-rm-rf-x"),
            ("python -c 'print(1)' && nvidia-smi --query", "20261008-142301-python-c-print-1-nvidia"),
            ("pytest -x tests/ -k 'part1 and not $SLOW'", "20261008-142301-pytest-x-tests-k-part1"),
            ("supercalifragilisticexpialidocious", "20261008-142301-supercalifragilisticexpi"),
            ("ÜBER — ✓", "20261008-142301-ber"),
            ("'$@!'", "20261008-142301-job"),
            ("", "20261008-142301-job"),
        ] {
            let got = JobId::generate(t, cmd);
            assert_eq!(got.as_str(), want, "{cmd:?}");
            // Every generated id is one `parse` accepts (and so is safe in a remote path).
            assert_eq!(JobId::parse(got.as_str()), Ok(got.clone()), "{cmd:?}");
        }
        // The epoch, a leap day, and lexical order = time order.
        assert_eq!(JobId::generate(0, "x").as_str(), "19700101-000000-x");
        assert_eq!(JobId::generate(1_709_164_799, "x").as_str(), "20240228-235959-x");
        assert_eq!(JobId::generate(1_709_164_800, "x").as_str(), "20240229-000000-x");
        assert!(JobId::generate(t, "zzz") < JobId::generate(t + 1, "aaa"));
    }

    #[test]
    fn parse_accepts_only_the_generated_shape() {
        for ok in ["20261008-142301-pytest", "20261008-142301", "20261008-142301-a-b-9", "20261008-142301-"] {
            assert!(JobId::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "pytest",
            "2026108-142301-x",
            "20261008_142301",
            "20261008-14230-x",
            "20261008-142301x",
            "20261008-142301-../../etc",
            "20261008-142301-a b",
            "20261008-142301-A",
            "20261008-142301-$(reboot)",
            "20261008-142301-x'y",
            &format!("20261008-142301-{}", "a".repeat(60)),
        ] {
            let e = JobId::parse(bad).unwrap_err();
            assert!(e.contains("is not a job id"), "{bad}: {e}");
        }
    }

    #[test]
    fn logs_arguments_split_into_targets_and_one_job() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cases: &[(&[&str], Result<(&[&str], Option<&str>), &str>)] = &[
            (&[], Ok((&[], None))),
            (&["apple", "bloom..cloud"], Ok((&["apple", "bloom..cloud"], None))),
            (&["20261008-142301-pytest"], Ok((&[], Some("20261008-142301-pytest")))),
            (&["apple", "20261008-142301-pytest", "cloud"], Ok((&["apple", "cloud"], Some("20261008-142301-pytest")))),
            // provider ids (runpod / vast / hetzner) stay targets
            (&["r088kp345z5m8c", "12345678", "20261008"], Ok((&["r088kp345z5m8c", "12345678", "20261008"], None))),
            (&["20261008-142301-a", "20261008-142302-b"], Err("one job at a time")),
            (&["20261008-142301-Bad"], Err("is not a job id")),
        ];
        for (words, want) in cases {
            match (split_job_arg(&w(words)), want) {
                (Ok((t, j)), Ok((wt, wj))) => {
                    assert_eq!(t, w(wt), "{words:?}");
                    assert_eq!(j.as_ref().map(JobId::as_str), *wj, "{words:?}");
                }
                (Err(e), Err(frag)) => assert!(e.contains(frag), "{words:?}: {e}"),
                (got, _) => panic!("{words:?}: {got:?}"),
            }
        }
    }

    /// Every script travels as one `sh -c '…'` word: a real `sh` must get its body back
    /// out of the quoting byte for byte (where there is a `sh`).
    #[test]
    fn commands_are_one_quoted_sh_word() {
        let j = id("20261008-142301-x");
        let nasty = "echo 'it'\\''s' \"$HOME\" `id` $(whoami) \\n\nprintf '%s\\n' \"a\tb\" # done ; exit 7";
        let have_sh = std::process::Command::new("sh").arg("-c").arg("true").output().is_ok();
        for (cmd, body) in [
            (start_command(&j, nasty, Some("arena-env")), start_script(&j, nasty, Some("arena-env"))),
            (list_command(), list_script()),
            (read_command(Some(&j), Some(5)), read_script(Some(&j), Some(5))),
            (read_command(None, None), read_script(None, None)),
            (kill_command(&j), kill_script(&j)),
        ] {
            assert!(cmd.starts_with("sh -c '") && cmd.ends_with('\''), "{cmd}");
            assert!(body.contains("ARENA_JOB") && !body.contains(nasty), "the command only travels base64-encoded");
            if have_sh {
                // `printf %s <the quoted word>` hands back exactly what `sh -c` would run.
                let word = cmd.strip_prefix("sh -c ").unwrap();
                let out = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {word}")).output().unwrap();
                assert_eq!(String::from_utf8(out.stdout).unwrap(), body);
            }
        }
        // The command and wrapper are base64 inside the start script; the id is in the clear.
        let start = start_command(&j, nasty, None);
        assert!(start.contains(&base64::encode(nasty.as_bytes())));
        assert!(start.contains(&base64::encode(run_script(&j, nasty, None).as_bytes())));
        assert!(start.contains("ARENA_JOB_STARTED 20261008-142301-x $pid"));
        // The wrapper runs the command under the same wrap as `pods run`.
        let run = run_script(&j, "pytest -x", Some("arena-env"));
        assert!(run.contains(&crate::ssh::login_shell_wrap("pytest -x", Some("arena-env"))), "{run}");
        assert!(run.contains("trap : INT TERM") && run.contains("PYTHONUNBUFFERED=1"), "{run}");
        // Detached as a foreground `setsid -f` (an `&` list would start the job with SIGINT
        // and SIGQUIT ignored) — `&` only where `setsid` has no `-f`.
        let body = start_script(&j, "pytest", None);
        assert!(body.contains("\n  setsid -f nohup sh \"$d/run\" > \"$d/log\" 2>&1 < /dev/null\nelse"), "{body}");
        // A read with no job/offset leaves both empty (newest job, last cap bytes).
        let r = read_command(None, None);
        assert!(r.contains("id=\nif") && r.contains("from=; cap=1048576"), "{r}");
    }

    #[test]
    fn list_replies_parse_strictly_and_sort_newest_first() {
        let lines = vec![
            job_line("20261008-120000-nvidia-smi", "exit", Some(0), Some(77), "nvidia-smi"),
            job_line("20261008-142301-pytest", "running", None, Some(812), "pytest\ttests/ \u{1b}[31mred\u{1b}[0m"),
            job_line("20261007-090000-x", "lost", None, Some(5), "x"),
            job_line("20261008-130000-y", "starting", None, None, "y"),
        ];
        // Shell-startup noise before our lines is skipped.
        let reply = format!("Welcome back!\n{}", list_reply(&lines));
        let jobs = parse_list(&reply).unwrap();
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        assert_eq!(ids, ["20261008-142301-pytest", "20261008-130000-y", "20261008-120000-nvidia-smi", "20261007-090000-x"]);
        assert_eq!(
            jobs[0],
            JobInfo {
                id: id("20261008-142301-pytest"),
                started: Some("2026-10-08T14:23:01Z".into()),
                state: JobState::Running,
                pid: Some(812),
                cmd: "pytest\ttests/ red".into(),
            }
        );
        assert_eq!((jobs[2].state, jobs[3].state, jobs[1].pid), (JobState::Exited(0), JobState::Lost, None));
        assert_eq!(parse_list("ARENA_JOBS_END\n"), Ok(vec![]));

        // Fail closed: no end marker (cut off / something else answered), a bad field.
        for bad in [
            String::new(),
            "zsh: command not found: sh\n".to_string(),
            format!("{}\n", lines[0]),
            list_reply(&[job_line("20261008-1423", "running", None, Some(1), "x")]),
            list_reply(&[job_line("20261008-142301-x", "sleeping", None, Some(1), "x")]),
            list_reply(&[job_line("20261008-142301-x", "exit", None, Some(1), "x")]),
            list_reply(&[job_line("20261008-142301-x", "running", None, Some(812), "x").replace("\t812", "\tpid")]),
            list_reply(&["ARENA_JOB\t20261008-142301-x\tshort".to_string()]),
            "ARENA_JOB\t20261008-142301-x\t-\trunning\t-\tabc\tx\nARENA_JOBS_END\n".to_string(),
        ] {
            assert!(parse_list(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn read_replies_carry_an_exact_log_window() {
        let line = job_line("20261008-142301-pytest", "exit", Some(1), Some(812), "pytest");
        let bytes = b"collected 3 items\n\xff ok\n";
        let got = parse_read(&read_reply(&line, 10, 10 + bytes.len() as u64, bytes)).unwrap();
        let ReadReply::Job { info, chunk } = got else { panic!("{got:?}") };
        assert_eq!((info.state, chunk.start, chunk.end(), chunk.bytes.as_slice()), (JobState::Exited(1), 10, 33, &bytes[..]));
        // An empty log.
        let ReadReply::Job { chunk, .. } = parse_read(&read_reply(&line, 0, 0, b"")).unwrap() else { panic!() };
        assert_eq!(chunk, LogChunk::default());
        assert_eq!(parse_read("ARENA_JOBS_NONE\nARENA_JOBS_END\n"), Ok(ReadReply::NoJobs));
        assert_eq!(parse_read("noise\nARENA_JOB_MISSING\nARENA_JOBS_END\n"), Ok(ReadReply::Missing));

        for (bad, why) in [
            (read_reply(&line, 0, 10, b"x").replace("ARENA_JOBS_END\n", ""), "unexpected reply"),
            (format!("{line}\nARENA_JOBS_END\n"), "not its log"),
            (read_reply(&line, 5, 4, b""), "inconsistent"),
            (read_reply(&line, 0, 2, b"abc"), "inconsistent"),
            (read_reply(&line, 0, 3, b"abc").replace("YWJj", "YW!j"), "don't decode"),
            (read_reply(&line, 0, 3, b"abc").replace("ARENA_LOG\t0", "ARENA_LOG\tx"), "malformed log header"),
            ("ARENA_WHAT\nARENA_JOBS_END\n".to_string(), "malformed job line"),
            ("ARENA_JOBS_END\n".to_string(), "unexpected reply"),
        ] {
            let e = parse_read(&bad).unwrap_err();
            assert!(e.contains(why), "{bad:?}: {e}");
        }
    }

    #[test]
    fn start_and_kill_replies() {
        let j = id("20261008-142301-x");
        assert_eq!(parse_started("ARENA_JOB_STARTED 20261008-142301-x 4242\n", &j), Ok(4242));
        assert_eq!(parse_started("motd\nARENA_JOB_STARTED 20261008-142301-x 7\r\n", &j), Ok(7));
        for (out, want) in [
            ("ARENA_JOB_ERR setsid not found on the pod\n", "setsid not found on the pod"),
            // No pid within 5s: never a plain failure — the job may be running.
            (
                "ARENA_JOB_UNSURE /root/.arena/jobs/20261008-142301-x/log\n",
                "no pid from the job within 5s — it may have started anyway: check `arena pods jobs` before retrying \
                 (its log: /root/.arena/jobs/20261008-142301-x/log)",
            ),
            ("ARENA_JOB_STARTED 20261008-142301-y 7\n", "unexpected start reply"),
            ("ARENA_JOB_STARTED 20261008-142301-x seven\n", "bad pid"),
            ("", "no start confirmation from the pod: (no output)"),
        ] {
            let e = parse_started(out, &j).unwrap_err();
            assert!(e.contains(want), "{out:?}: {e}");
        }

        let line = job_line("20261008-142301-x", "running", None, Some(812), "x");
        let info = parse_job_line(&line).unwrap();
        for (verdict, want) in [
            ("ARENA_KILL_SENT", KillReply::Sent(info.clone())),
            ("ARENA_KILL_SKIPPED", KillReply::Skipped(info.clone())),
            ("ARENA_KILL_REFUSED", KillReply::Refused(info.clone())),
            ("ARENA_KILL_FAILED", KillReply::Failed(info.clone())),
        ] {
            assert_eq!(parse_kill(&format!("{line}\n{verdict}\nARENA_JOBS_END\n")), Ok(want), "{verdict}");
        }
        assert_eq!(parse_kill("ARENA_JOB_MISSING\nARENA_JOBS_END\n"), Ok(KillReply::Missing));
        for bad in [format!("{line}\nARENA_KILL_SENT\n"), format!("{line}\nARENA_KILL_MAYBE\nARENA_JOBS_END\n"), "ARENA_JOBS_END\n".into()] {
            assert!(parse_kill(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        for (raw, want) in [
            ("plain text", "plain text"),
            ("\u{1b}[1;31mFAILED\u{1b}[0m tests", "FAILED tests"),
            ("title\u{1b}]0;pwned\u{7}after", "titleafter"),
            ("clip\u{1b}]52;c;ZXZpbA==\u{1b}\\done", "clipdone"),
            ("bell\u{7} back\u{8}space\u{0}nul", "bell backspacenul"),
            ("tab\tkept", "tab\tkept"),
            ("c1\u{9b}31m", "c131m"),
            ("lone esc\u{1b}", "lone esc"),
            ("\u{1b}(Bcharset", "Bcharset"),
            ("unicode ✓ ünï", "unicode ✓ ünï"),
        ] {
            assert_eq!(sanitize(raw), want, "{raw:?}");
        }
        for (raw, want) in [
            (&b"line\r"[..], "line"),
            (b" 10%|#  |\r 50%|## |\r100%|###|", "100%|###|"),
            (b"bad \xff byte", "bad \u{fffd} byte"),
            (b"\x1b[32mok\x1b[0m", "ok"),
        ] {
            assert_eq!(display_line(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn state_labels() {
        let info = |state, pid| JobInfo { id: id("20261008-142301-x"), started: None, state, pid, cmd: "x".into() };
        for (state, pid, want) in [
            (JobState::Starting, None, "starting"),
            (JobState::Running, Some(812), "running (pid 812)"),
            (JobState::Exited(0), Some(812), "exit 0"),
            (JobState::Exited(2), None, "exit 2"),
            (JobState::Exited(143), None, "exit 143 (SIGTERM)"),
            (JobState::Exited(137), None, "exit 137 (SIGKILL)"),
            (JobState::Exited(128), None, "exit 128"),
            (JobState::Exited(255), None, "exit 255"),
            (JobState::Lost, Some(9), "lost (ended without an exit code)"),
        ] {
            assert_eq!(state_label(&info(state, pid)), want);
        }
        assert!(JobState::Lost.failed() && JobState::Exited(1).failed() && !JobState::Exited(0).failed());
        assert!(!JobState::Running.is_over() && !JobState::Starting.is_over() && JobState::Lost.is_over());
    }

    #[test]
    fn kill_report_fails_only_where_a_running_job_was_not_signalled() {
        let j = id("20261008-142301-pytest");
        let info = |state: &str, code| parse_job_line(&job_line(j.as_str(), state, code, Some(812), "pytest")).unwrap();
        let pods = vec![
            ("devtest-apple".to_string(), Ok(KillReply::Sent(info("running", None)))),
            ("devtest-bloom".to_string(), Ok(KillReply::Skipped(info("exit", Some(0))))),
            ("devtest-cloud".to_string(), Ok(KillReply::Missing)),
            ("devtest-dune".to_string(), Ok(KillReply::Refused(info("running", None)))),
            ("devtest-elm".to_string(), Ok(KillReply::Failed(info("running", None)))),
            ("devtest-fig".to_string(), Err("timed out after 30s".to_string())),
        ];
        let (lines, failed) = render_kill(&pods, &j);
        assert_eq!(failed, 3);
        assert_eq!(
            lines,
            [
                "✓ devtest-apple: SIGTERM sent to job 20261008-142301-pytest (process group 812)",
                "– devtest-bloom: job 20261008-142301-pytest is not running (exit 0)",
                "– devtest-cloud: no job 20261008-142301-pytest",
                "✗ devtest-dune: pid 812 doesn't lead its own process group — not signalled",
                "✗ devtest-elm: kill failed (pid 812)",
                "✗ devtest-fig: timed out after 30s",
            ]
        );
    }

    #[test]
    fn jobs_report_is_a_table_plus_a_line_per_empty_or_failed_pod() {
        let j = |line: String| parse_job_line(&line).unwrap();
        let apple = vec![
            j(job_line("20261008-142301-pytest", "running", None, Some(812), "pytest -x tests/")),
            j(job_line("20261008-120000-nvidia-smi", "exit", Some(0), Some(77), "nvidia-smi")),
        ];
        let cloud = vec![j(job_line("20261008-142301-pytest", "exit", Some(143), Some(90), "pytest -x tests/"))];
        let pods = vec![
            ("devtest-cloud".to_string(), Ok(cloud)),
            ("devtest-bloom".to_string(), Ok(vec![])),
            ("devtest-apple".to_string(), Ok(apple)),
            ("devtest-dune".to_string(), Err("timed out after 30s".to_string())),
        ];
        let (text, failed) = render_jobs(&pods, None);
        assert_eq!(failed, 1);
        assert_eq!(
            text,
            "POD            JOB                         STARTED (UTC)         STATUS              COMMAND\n\
             devtest-apple  20261008-142301-pytest      2026-10-08T14:23:01Z  running (pid 812)   pytest -x tests/\n\
             devtest-apple  20261008-120000-nvidia-smi  2026-10-08T14:23:01Z  exit 0              nvidia-smi\n\
             devtest-cloud  20261008-142301-pytest      2026-10-08T14:23:01Z  exit 143 (SIGTERM)  pytest -x tests/\n\
             devtest-bloom: no jobs\n\
             ✗ devtest-dune: timed out after 30s\n\
             \n3 job(s), 1 running; 1 pod(s) not read\n"
        );
        // One job across the fleet.
        let (text, _) = render_jobs(&pods[..3], Some(&id("20261008-120000-nvidia-smi")));
        assert_eq!(
            text,
            "POD            JOB                         STARTED (UTC)         STATUS  COMMAND\n\
             devtest-apple  20261008-120000-nvidia-smi  2026-10-08T14:23:01Z  exit 0  nvidia-smi\n\
             devtest-bloom: no job 20261008-120000-nvidia-smi\n\
             devtest-cloud: no job 20261008-120000-nvidia-smi\n\
             \n1 job(s), 0 running\n"
        );
    }

    #[test]
    fn snapshot_shows_the_last_lines_including_an_unfinished_one() {
        let chunk = |start: u64, bytes: &[u8]| LogChunk { start, size: start + bytes.len() as u64, bytes: bytes.to_vec() };
        let cases: &[(LogChunk, usize, &[&str], bool)] = &[
            (chunk(0, b"a\nb\nc\n"), 2, &["b", "c"], false),
            (chunk(0, b"a\nb\nc\n"), 10, &["a", "b", "c"], false),
            (chunk(0, b"a\nb\n50%\r80%"), 2, &["b", "80%"], false),
            (chunk(0, b""), 5, &[], false),
            (chunk(0, b"a\n"), 0, &[], false),
            // A capped window starts mid-line: that piece is dropped, and the cut is flagged
            // when it leaves fewer lines than asked for.
            (chunk(100, b"tail of x\ny\nz\n"), 5, &["y", "z"], true),
            (chunk(100, b"tail of x\ny\nz\n"), 2, &["y", "z"], false),
            (chunk(100, b"one huge line"), 3, &["one huge line"], true),
        ];
        for (c, tail, want, cut) in cases {
            assert_eq!(snapshot_lines(c, *tail), (want.iter().map(|s| s.to_string()).collect(), *cut), "{c:?} tail {tail}");
        }
    }

    #[test]
    fn follow_prints_each_line_once_whole() {
        let chunk = |start: u64, bytes: &[u8]| LogChunk { start, size: start + bytes.len() as u64, bytes: bytes.to_vec() };
        // First read: last 2 complete lines; "par" is held for the next read to finish.
        let (mut f, shown, cut) = Follow::first(&chunk(0, b"a\nb\nc\npar"), 2);
        assert_eq!((shown, cut, f.offset()), (vec!["b".to_string(), "c".to_string()], false, 9));
        // The held line completes; a new partial is held.
        assert_eq!(f.next(&chunk(9, b"tial\nd\ne")), ["partial", "d"]);
        assert_eq!(f.offset(), 17);
        // Nothing new: nothing printed.
        assert!(f.next(&chunk(17, b"")).is_empty());
        // The job ended with an unfinished last line: it is flushed once.
        assert_eq!(f.finish().as_deref(), Some("e"));
        assert_eq!(f.finish(), None);

        // Skipped ahead (a flood bigger than the cap): said, and the stale held piece dropped.
        let (mut f, _, _) = Follow::first(&chunk(0, b"x\nhel"), 5);
        let got = f.next(&chunk(5 + 2_000_000, b"lo\nz\n"));
        assert_eq!(got, ["… skipped 2000000 bytes (more than 1024 KiB arrived between two reads)", "lo", "z"]);
        // Truncated under us: said, then read afresh.
        let got = f.next(&chunk(0, b"new\n"));
        assert_eq!(got, ["… the log shrank (truncated?) — reading it again from here", "new"]);
        assert_eq!(f.offset(), 4);

        // A first read that starts mid-file drops its first (partial) line.
        let (_, shown, cut) = Follow::first(&chunk(50, b"ial\nq\n"), 5);
        assert_eq!((shown, cut), (vec!["q".to_string()], true));

        // A line that never ends is shown once it passes the bound, not held forever.
        let (mut f, shown, _) = Follow::first(&chunk(0, b""), 5);
        assert!(shown.is_empty());
        let big = vec![b'x'; PARTIAL_MAX + 1];
        let got = f.next(&chunk(0, &big));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].len(), PARTIAL_MAX + 1);
        assert_eq!(f.finish(), None);
    }

    /// A pod stand-in for the real-shell tests: `$HOME` in a temp dir, a stub `zsh` (the
    /// login-shell wrap runs `zsh -c`; this one hands the script to bash, so `source
    /// ~/.zshrc` / `conda` just fail quietly), and each remote command run the way sshd
    /// runs it — handed to a shell's `-c`. No ssh, no network: exactly the bytes a pod runs.
    #[cfg(target_os = "linux")]
    struct LocalPod {
        dir: std::path::PathBuf,
        home: std::path::PathBuf,
        bin: std::path::PathBuf,
        path: String,
    }

    #[cfg(target_os = "linux")]
    impl LocalPod {
        /// `None` (the test skips) where a tool the scripts need is missing.
        fn new(tag: &str) -> Option<LocalPod> {
            use std::process::Command;
            let have = |t: &str| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success());
            if !["setsid", "nohup", "base64", "bash", "head", "tail", "find"].iter().all(|t| have(t)) {
                eprintln!("setsid/nohup/base64/bash/find missing — skipping");
                return None;
            }
            let dir = std::env::temp_dir().join(format!("arena-jobs-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let (home, bin) = (dir.join("home"), dir.join("bin"));
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
            let pod = LocalPod { dir, home, bin, path };
            pod.stub("zsh", "[ \"$1\" = -c ] && shift\nexec bash -c \"$1\"");
            Some(pod)
        }

        /// An executable `name` on the pod's PATH, ahead of the real one.
        fn stub(&self, name: &str, body: &str) {
            use std::os::unix::fs::PermissionsExt;
            let f = self.bin.join(name);
            std::fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// What sshd does with a remote command: hand it to the login shell's `-c`.
        fn run(&self, remote_cmd: &str) -> String {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote_cmd)
                .env("HOME", &self.home)
                .env("PATH", &self.path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn job_dir(&self, id: &JobId) -> std::path::PathBuf {
            self.home.join(JOBS_DIR).join(id.as_str())
        }

        /// A job directory as `START` leaves it, with `pid` recorded or not — and no
        /// wrapper actually running.
        fn fake_job(&self, id: &JobId, pid: Option<&str>) -> std::path::PathBuf {
            let d = self.job_dir(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cmd"), "pytest").unwrap();
            std::fs::write(d.join("started_at"), "2026-10-08T14:23:01Z\n").unwrap();
            if let Some(pid) = pid {
                std::fs::write(d.join("pid"), format!("{pid}\n")).unwrap();
            }
            d
        }

        fn wait_until(&self, what: &str, ok: &dyn Fn() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ok() {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for LocalPod {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The `SigIgn` mask from a `/proc/<pid>/status` text.
    #[cfg(target_os = "linux")]
    fn sig_ign(status: &str) -> u64 {
        let hex = status.lines().find_map(|l| l.strip_prefix("SigIgn:")).expect("a SigIgn line");
        u64::from_str_radix(hex.trim(), 16).expect("SigIgn is hex")
    }

    /// The scripts for real, against a [`LocalPod`]: start a job, list it, read its log from
    /// an offset, see it finish with its exit code; start one that sleeps, kill it, see 143;
    /// the job's command gets SIGINT/SIGQUIT as the operator's shell had them; a recycled pid
    /// never passes for a running job (nor gets signalled).
    #[cfg(target_os = "linux")]
    #[test]
    fn scripts_run_for_real_against_a_local_shell() {
        let Some(pod) = LocalPod::new("real") else { return };
        let run = |c: &str| pod.run(c);

        // A command full of quoting traps; it prints two lines (one to stderr), then exits 7.
        let cmd = "x='it'\"'\"'s'; echo \"out $x \\$HOME\"; printf 'err\\t%s\\n' \"$((6*7))\" >&2\nexit 7";
        let j = JobId::generate(1_791_469_381, cmd);
        let pid = parse_started(&run(&start_command(&j, cmd, Some("arena-env"))), &j).unwrap();
        assert!(pid > 1);
        let jobdir = pod.job_dir(&j);
        assert_eq!(std::fs::read_to_string(jobdir.join("cmd")).unwrap(), cmd, "the command is stored as typed");
        pod.wait_until("the job to exit", &|| jobdir.join("exit").exists());
        let jobs = parse_list(&run(&list_command())).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].id.clone(), jobs[0].state, jobs[0].pid), (j.clone(), JobState::Exited(7), Some(pid)));
        assert!(jobs[0].started.as_deref().is_some_and(|s| s.len() == 20 && s.ends_with('Z')), "{:?}", jobs[0].started);
        assert_eq!(jobs[0].cmd, sanitize(&cmd.replace(['\t', '\n'], " ")));
        let ReadReply::Job { info, chunk } = parse_read(&run(&read_command(None, None))).unwrap() else { panic!() };
        assert_eq!(info.id, j);
        let (lines, _) = snapshot_lines(&chunk, 10);
        assert_eq!(lines, ["out it's $HOME", "err\t42"]);
        // From an offset: exactly the rest.
        let ReadReply::Job { chunk: rest, .. } = parse_read(&run(&read_command(Some(&j), Some(4)))).unwrap() else { panic!() };
        assert_eq!((rest.start, rest.bytes.as_slice()), (4, &chunk.bytes[4..]));
        // An id clash is refused, not overwritten.
        let e = parse_started(&run(&start_command(&j, "true", None)), &j).unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        // Unknown job / killing a finished one.
        let other = id("20261008-142302-nope");
        assert_eq!(parse_read(&run(&read_command(Some(&other), None))), Ok(ReadReply::Missing));
        assert_eq!(parse_kill(&run(&kill_command(&other))), Ok(KillReply::Missing));
        assert!(matches!(parse_kill(&run(&kill_command(&j))), Ok(KillReply::Skipped(_))));

        // A long job: running (its pid checked against its cmdline), killed by group → 143.
        let k = JobId::generate(1_791_469_382, "sleep");
        let pid = parse_started(&run(&start_command(&k, "echo begin; sleep 30; echo never", None)), &k).unwrap();
        let ReadReply::Job { info, .. } = parse_read(&run(&read_command(None, None))).unwrap() else { panic!() };
        assert_eq!((info.id.clone(), info.state, info.pid), (k.clone(), JobState::Running, Some(pid)), "newest = the sleeper");
        assert!(matches!(parse_kill(&run(&kill_command(&k))), Ok(KillReply::Sent(_))));
        let kdir = pod.job_dir(&k);
        pod.wait_until("the killed job's exit code", &|| kdir.join("exit").exists());
        let jobs = parse_list(&run(&list_command())).unwrap();
        assert_eq!(jobs.iter().map(|j| (j.id.clone(), j.state)).collect::<Vec<_>>(), [(k.clone(), JobState::Exited(143)), (j, JobState::Exited(7))]);
        let ReadReply::Job { chunk, .. } = parse_read(&run(&read_command(Some(&k), None))).unwrap() else { panic!() };
        assert!(snapshot_lines(&chunk, 10).0.first().is_some_and(|l| l == "begin"), "{chunk:?}");
        // The wrapper's pid no longer counts as the job once it has gone.
        std::fs::remove_file(kdir.join("exit")).unwrap();
        let ReadReply::Job { info, .. } = parse_read(&run(&read_command(Some(&k), None))).unwrap() else { panic!() };
        assert_eq!(info.state, JobState::Lost);

        // The command's signal dispositions: SIGHUP ignored (nohup), SIGINT/SIGQUIT exactly
        // as this test's own — a job started as an `&` list would have them ignored, so
        // python would never raise KeyboardInterrupt.
        let s = JobId::generate(1_791_469_383, "sigign");
        parse_started(&run(&start_command(&s, "grep '^SigIgn:' /proc/self/status", Some("arena-env"))), &s).unwrap();
        pod.wait_until("the signal job", &|| pod.job_dir(&s).join("exit").exists());
        let job_ign = sig_ign(&std::fs::read_to_string(pod.job_dir(&s).join("log")).unwrap());
        let own_ign = sig_ign(&std::fs::read_to_string("/proc/self/status").unwrap());
        let (hup, int_quit) = (1 << (1 - 1), (1 << (2 - 1)) | (1 << (3 - 1)));
        assert_eq!(job_ign & hup, hup, "nohup: SIGHUP ignored ({job_ign:#x})");
        assert_eq!(job_ign & int_quit, own_ign & int_quit, "SIGINT/SIGQUIT as the caller had them ({job_ign:#x} vs {own_ign:#x})");

        // A recycled pid: alive, but not this job's wrapper → lost, and never signalled.
        let mut stranger = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let r = id("20261008-142304-recycled");
        pod.fake_job(&r, Some(&stranger.id().to_string()));
        let jobs = parse_list(&run(&list_command())).unwrap();
        assert_eq!(jobs.iter().find(|x| x.id == r).map(|x| x.state), Some(JobState::Lost));
        assert!(matches!(parse_kill(&run(&kill_command(&r))), Ok(KillReply::Skipped(ref i)) if i.state == JobState::Lost));
        assert!(stranger.try_wait().unwrap().is_none(), "the unrelated process was not signalled");
        let _ = stranger.kill();
        let _ = stranger.wait();
    }

    /// The window between `judge`'s first look for `exit` and its liveness check, made
    /// deterministic: a stand-in `ours` plays the wrapper finishing right then. A job that
    /// ended in that window reads as exited (not `lost`, which `logs -f` would end on as a
    /// failure), and one `kill` caught running reads as not running (not "refused").
    #[cfg(target_os = "linux")]
    #[test]
    fn a_job_ending_mid_read_is_exited_never_lost_or_refused() {
        let Some(pod) = LocalPod::new("race") else { return };
        let j = id("20261008-142301-pytest");
        // No process has this pid (above any pid_max), so the real `ours` says "gone".
        let d = pod.fake_job(&j, Some("999999999"));
        // The real script, with `ours` replaced right after the prelude defines it.
        let with = |ours: &str, script: String| sh_c(&script.replacen(PRELUDE, &format!("{PRELUDE}{ours}\n"), 1));

        // `ours` writes the exit code (the wrapper finishing), then the pid is gone.
        let finishing = r#"ours() { echo 0 > "$J/$2/exit"; return 1; }"#;
        let jobs = parse_list(&pod.run(&with(finishing, list_script()))).unwrap();
        assert_eq!(jobs[0].state, JobState::Exited(0));
        let ReadReply::Job { info, .. } = parse_read(&pod.run(&with(finishing, read_script(Some(&j), None)))).unwrap() else {
            panic!()
        };
        assert_eq!(info.state, JobState::Exited(0));

        // `kill`: running at the first look (and finishing right after), so its process
        // group is gone by the time it's read — skipped, with the job's final state.
        std::fs::remove_file(d.join("exit")).unwrap();
        let ends_now = r#"ours() { [ -f "$J/$2/exit" ] && return 1; echo 0 > "$J/$2/exit"; }"#;
        match parse_kill(&pod.run(&with(ends_now, kill_script(&j)))) {
            Ok(KillReply::Skipped(info)) => assert_eq!(info.state, JobState::Exited(0)),
            other => panic!("{other:?}"),
        }

        // Without an exit code it is still lost — the second look adds nothing else.
        std::fs::remove_file(d.join("exit")).unwrap();
        assert_eq!(parse_list(&pod.run(&list_command())).unwrap()[0].state, JobState::Lost);
    }

    /// A job that never records its pid: `starting` while it is fresh, `lost` (over, so
    /// `logs -f` ends) once its directory is over a minute old.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_job_without_a_pid_is_starting_then_lost() {
        let Some(pod) = LocalPod::new("nopid") else { return };
        let j = id("20261008-142301-pytest");
        let d = pod.fake_job(&j, None);
        assert_eq!(parse_list(&pod.run(&list_command())).unwrap()[0].state, JobState::Starting);
        pod.run(&format!("touch -t 202001010000 '{}'", d.display()));
        assert_eq!(parse_list(&pod.run(&list_command())).unwrap()[0].state, JobState::Lost);
        assert!(matches!(parse_kill(&pod.run(&kill_command(&j))), Ok(KillReply::Skipped(ref i)) if i.state == JobState::Lost));
    }

    /// The start's two ways of not getting a pid. A wrapper that can't record it doesn't run
    /// the command (and says so: exit 125) — a clean "did not start". One that hasn't even
    /// begun within 5s may still run it, so it is reported as "may have started", never as
    /// a plain failure an operator would retry.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_start_without_a_pid_never_runs_untracked_and_is_never_called_clean() {
        let Some(pod) = LocalPod::new("start") else { return };
        let real_mv = pod.run("command -v mv").trim().to_string();
        assert!(real_mv.starts_with('/'), "{real_mv}");
        // `mv` refuses to install the pid file (as a full disk would refuse to write it).
        pod.stub("mv", &format!("for a; do last=$a; done\ncase \"$last\" in */pid) exit 1 ;; esac\nexec {real_mv} \"$@\""));
        let j = id("20261008-142301-touch");
        let e = parse_started(&pod.run(&start_command(&j, "touch \"$HOME/ran\"", None)), &j).unwrap_err();
        assert!(e.contains("could not record its pid, so it did not run the command"), "{e}");
        assert!(!pod.home.join("ran").exists(), "the command must not run untracked");
        assert_eq!(parse_list(&pod.run(&list_command())).unwrap()[0].state, JobState::Exited(125));
        let log = std::fs::read_to_string(pod.job_dir(&j).join("log")).unwrap();
        assert!(log.contains("could not record this job's pid"), "{log}");

        // A `setsid` that never starts anything: no pid and no exit code after 5s.
        std::fs::remove_file(pod.bin.join("mv")).unwrap();
        pod.stub("setsid", "exit 0");
        let k = id("20261008-142302-slow");
        let e = parse_started(&pod.run(&start_command(&k, "true", None)), &k).unwrap_err();
        assert!(e.contains("may have started anyway: check `arena pods jobs` before retrying"), "{e}");
    }
}
