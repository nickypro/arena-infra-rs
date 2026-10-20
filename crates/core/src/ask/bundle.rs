//! One pod's state for `pods ask`: one bounded SSH call, parsed tolerantly, rendered as a few KB
//! of text with every field masked ([`super::redact`]) and the whole capped ([`BUNDLE_CAP`]).
//!
//! **One call, existing probes first.** [`probe_command`] chains, in one remote command: the
//! dashboard's metrics probe (`crate::metrics`: nvidia-smi, the GPU processes, CPU and memory,
//! the root disk, the repo's branch, origin, ahead/behind, recent commits, the setup flags), the
//! `pods jobs` listing and the newest job's log tail (`crate::jobs`, the same scripts as `pods
//! jobs`/`pods logs`), and a small extra probe (`probe.sh`: clock and uptime, load, disks,
//! logins, tmux sessions, the busiest processes, the repo's working tree and any operation in
//! progress — the backup's own `busy_op`). Each part is fenced by a marker line, so one that
//! fails or is cut short leaves the others readable. All of it only reads.
//!
//! **Unknown, never guessed.** Every fact is optional: a section that's missing, cut off or
//! garbled renders as `unknown`, never as "none", and nothing here panics on any input (a test
//! feeds every prefix of a full reply). A pod that wasn't reached gets a bundle from its listing
//! alone, saying why.
//!
//! **What is never in a bundle:** a file's contents, shell history, the environment, `~/.arena`'s
//! other files, or where a login comes from. Process arguments, job commands and log lines,
//! commit subjects and file names are in it — masked: the backup's key patterns, secret-named
//! values, any 32+ character token ([`super::redact`]).

use std::time::Duration;

use super::redact;
use crate::fleet;
use crate::jobs::{self, JobInfo, JobState, ReadReply};
use crate::metrics::{self, PodMetrics, ProbeOpts};
use crate::pod::Pod;

/// The budget for one pod's call: the extra probe's slowest steps (`du` of the home, `git
/// status`) are bounded at 10–20 s each on the pod.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Most bytes one pod's bundle takes in a prompt.
pub const BUNDLE_CAP: usize = 6 * 1024;

/// The least a pod's bundle is cut to when many pods share a prompt ([`super::pod_cap`]).
pub const MIN_CAP: usize = 1536;

/// Lines of the newest job's log in a bundle, read from at most [`LOG_BYTES`].
pub const LOG_LINES: usize = 40;
pub const LOG_BYTES: u64 = 16 * 1024;

const FILES_SHOWN: usize = 20;
const PROCS_SHOWN: usize = 15;
const JOBS_SHOWN: usize = 8;
const ARGS_MAX: usize = 160;
const LINE_MAX: usize = 200;
const NAME_MAX: usize = 120;

const BEGIN: &str = "@@ARENA_ASK_BEGIN@@";
const JOBS: &str = "@@ARENA_ASK_JOBS@@";
const LOG: &str = "@@ARENA_ASK_LOG@@";
const EXTRA: &str = "@@ARENA_ASK_EXTRA@@";
const END: &str = "@@ARENA_ASK_END@@";

/// The extra probe, embedded (the backup's `busy_op` rendered in: [`probe_script`]).
pub const PROBE_SCRIPT: &str = include_str!("probe.sh");

/// [`PROBE_SCRIPT`] with the backup's `busy_op` function in place of its marker — one
/// definition of "an operation in progress" for the backup, the branch switches and this — and
/// the dashboard probe's read-only git ([`metrics::RO_GIT_SH`]) in place of its own.
pub fn probe_script() -> String {
    PROBE_SCRIPT.replacen("# @ARENA_BUSY_OP_SH@", crate::backup::BUSY_OP_SH, 1).replacen("# @ARENA_RO_GIT_SH@", metrics::RO_GIT_SH, 1)
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The one remote command for a pod's bundle (see the module doc). `GIT_OPTIONAL_LOCKS=0` first:
/// the metrics probe's and the extra probe's git is `ro_git` ([`metrics::RO_GIT_SH`]: no locks,
/// nothing of the repo's own run), and this keeps any other git in the call from refreshing
/// (writing) the index or taking its lock under a participant's git too (the extra probe, under
/// `env -i`, sets its own). The extra probe goes base64 (no quoting of its own) under `env -i`
/// with a fixed `PATH` — nothing from the pod's environment but `HOME` — as POSIX `sh`, like the
/// reaper's probe; `repo` is quoted, never spliced into the script.
pub fn probe_command(opts: &ProbeOpts, repo: &str) -> String {
    let b64 = crate::base64::encode(probe_script().as_bytes());
    format!(
        "export GIT_OPTIONAL_LOCKS=0; echo '{BEGIN}'; {}; echo '{JOBS}'; {}; echo '{LOG}'; {}; echo '{EXTRA}'; \
         env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=\"$HOME\" \
         sh -c \"$(printf %s {b64} | base64 -d)\" arena-ask-probe {}; echo '{END}'",
        metrics::probe_command(opts),
        jobs::list_command(),
        jobs::read_tail_command(LOG_BYTES),
        sq(repo.trim_end_matches('/')),
    )
}

/// A disk: KB used and in all, and its mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Df {
    pub used_kb: u64,
    pub total_kb: u64,
    pub mount: String,
}

/// One tmux session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tmux {
    pub name: String,
    pub windows: Option<u32>,
    pub attached: Option<u32>,
    /// Last activity, pod clock (unix seconds).
    pub activity: Option<u64>,
}

/// One process, as `ps` listed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub user: String,
    /// CPU % averaged over the process's life (ps's `pcpu`), memory %.
    pub cpu: f64,
    pub mem: f64,
    /// Seconds since it started.
    pub age: u64,
    pub args: String,
}

/// The repo's working tree, as the extra probe saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repo {
    /// Its real path (through a symlink: on the `/workspace` volume?).
    pub real: Option<String>,
    pub branch: Option<String>,
    /// An operation in progress: the backup's `busy_op` line (one of
    /// [`crate::backup::BUSY_WORDS`], then ` unmerged` while conflicts are unresolved), else
    /// `rebase` or `bisect`; `Some("")` = none, `None` = unknown.
    pub op: Option<String>,
    pub modified: Option<u32>,
    pub untracked: Option<u32>,
    /// `git status --porcelain`-style lines (`XY path`, from `ro_changes`), the first [`FILES_SHOWN`]; empty when the
    /// status couldn't be read (then `modified`/`untracked` are `None` too).
    pub files: Vec<String>,
    /// The last commit: committer time (unix) and subject.
    pub last: Option<(u64, String)>,
    pub upstream: Option<String>,
    pub stashes: Option<u32>,
}

/// Where the repo stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Git {
    /// No repo path to look at.
    NotSet,
    /// The path isn't there.
    Missing,
    /// No `git` on the pod.
    NoGit,
    /// The path isn't a git checkout.
    NotRepo,
    Repo(Repo),
}

/// The extra probe's facts. `None` everywhere means unknown (its line was missing or odd);
/// an `Err` says why a list couldn't be read at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extra {
    pub now: Option<String>,
    pub uptime: Option<u64>,
    pub pid1: Option<String>,
    pub pid1_age: Option<u64>,
    pub load: Option<String>,
    pub cpus: Option<u32>,
    pub nvidia_smi: Option<bool>,
    pub df_root: Option<Df>,
    pub df_home: Option<Df>,
    /// `Some(None)`: there is no `/workspace`.
    pub df_workspace: Option<Option<Df>>,
    pub home: Option<String>,
    pub home_kb: Option<u64>,
    pub who: Option<Result<Vec<String>, String>>,
    pub tmux: Option<Result<Vec<Tmux>, String>>,
    pub procs: Option<Result<Vec<Proc>, String>>,
    pub git: Option<Git>,
    /// The probe ran to its last line.
    pub done: bool,
}

/// A pod's reply, section by section; a section that's absent is `None` (unknown).
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub metrics: Option<PodMetrics>,
    pub jobs: Option<Result<Vec<JobInfo>, String>>,
    pub log: Option<Result<ReadReply, String>>,
    pub extra: Option<Extra>,
    /// The whole command ran to its end marker.
    pub complete: bool,
}

/// Read a reply to [`probe_command`]. `None` when it isn't one at all (no begin marker: the call
/// never got to run it). Never fails otherwise: each section is read on its own.
pub fn parse_reply(stdout: &str) -> Option<Reply> {
    let mut bufs: [Option<String>; 4] = Default::default();
    let (mut cur, mut begun, mut complete) = (None, false, false);
    for line in stdout.split('\n') {
        let l = line.trim_end_matches('\r');
        let at = match l {
            BEGIN => Some(0),
            JOBS => Some(1),
            LOG => Some(2),
            EXTRA => Some(3),
            END => {
                complete = begun;
                cur = None;
                continue;
            }
            _ => None,
        };
        if let Some(i) = at {
            begun |= i == 0;
            if begun {
                cur = Some(i);
                bufs[i].get_or_insert_with(String::new);
            }
            continue;
        }
        if let Some(buf) = cur.and_then(|i| bufs[i].as_mut()) {
            buf.push_str(l);
            buf.push('\n');
        }
    }
    if !begun {
        return None;
    }
    let [m, j, lg, x] = bufs;
    Some(Reply {
        metrics: m.map(|s| metrics::parse_probe_output(&s)),
        jobs: j.map(|s| jobs::parse_list(&s)),
        log: lg.map(|s| jobs::parse_read(&s)),
        extra: x.map(|s| parse_extra(&s)),
        complete,
    })
}

fn number<T: std::str::FromStr>(s: &str) -> Option<T> {
    let s = s.trim();
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

fn df(v: &str) -> Option<Df> {
    let mut it = v.split_whitespace();
    let (used_kb, total_kb) = (number(it.next()?)?, number(it.next()?)?);
    let mount = it.collect::<Vec<_>>().join(" ");
    Some(Df { used_kb, total_kb, mount })
}

/// The next whitespace-separated word of `s`, and the rest.
fn word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..end], &s[end..]))
}

fn parse_proc(line: &str) -> Option<Proc> {
    let (pid, r) = word(line)?;
    let (user, r) = word(r)?;
    let (cpu, r) = word(r)?;
    let (mem, r) = word(r)?;
    let (age, r) = word(r)?;
    let float = |s: &str| s.parse::<f64>().ok().filter(|f| f.is_finite() && *f >= 0.0);
    Some(Proc { pid: number(pid)?, user: user.to_string(), cpu: float(cpu)?, mem: float(mem)?, age: number(age)?, args: r.trim().to_string() })
}

fn parse_tmux(line: &str) -> Tmux {
    let mut f = line.split('\t');
    Tmux {
        name: f.next().unwrap_or("").to_string(),
        windows: f.next().and_then(number),
        attached: f.next().and_then(number),
        activity: f.next().and_then(number),
    }
}

/// The extra probe's lines (see `probe.sh`). Unknown lines are skipped; a key that appears
/// twice keeps its first value.
pub fn parse_extra(text: &str) -> Extra {
    let mut kv: std::collections::BTreeMap<&str, &str> = Default::default();
    let (mut ps, mut who, mut tmux, mut files) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut bad_ps = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ARENA_ASK ") {
            if let Some((k, v)) = rest.split_once('=') {
                kv.entry(k).or_insert(v);
            }
        } else if let Some(rest) = line.strip_prefix("ARENA_ASK_PS ") {
            match parse_proc(rest) {
                Some(p) if !ps.iter().any(|q: &Proc| q.pid == p.pid) => ps.push(p),
                Some(_) => {}
                None => bad_ps += 1,
            }
        } else if let Some(rest) = line.strip_prefix("ARENA_ASK_WHO ") {
            who.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("ARENA_ASK_TMUX ") {
            tmux.push(parse_tmux(rest));
        } else if let Some(rest) = line.strip_prefix("ARENA_ASK_FILE ") {
            files.push(rest.to_string());
        }
    }
    let get = |k: &str| kv.get(k).map(|v| v.trim()).filter(|v| !v.is_empty() && *v != "-");
    let list = |state: Option<&str>, items: Vec<String>| match state {
        Some("ok") => Some(Ok(items)),
        Some("absent") => Some(Err("not installed".to_string())),
        Some(_) => Some(Err("couldn't be read".to_string())),
        None => None,
    };
    let git = match get("git") {
        None => None,
        Some("none") => Some(Git::NotSet),
        Some("absent") => Some(Git::Missing),
        Some("nogit") => Some(Git::NoGit),
        Some("notrepo") => Some(Git::NotRepo),
        Some("ok") => {
            let status_ok = get("status") == Some("ok");
            let last = get("last").and_then(|v| {
                let (t, subject) = v.split_once(' ').unwrap_or((v, ""));
                Some((number(t)?, subject.to_string()))
            });
            Some(Git::Repo(Repo {
                real: get("real").map(String::from),
                branch: get("branch").map(String::from),
                op: match kv.get("op").map(|v| v.trim()) {
                    Some("-") => Some(String::new()),
                    Some(v) if !v.is_empty() => Some(v.to_string()),
                    _ => None,
                },
                modified: status_ok.then(|| get("modified").and_then(number)).flatten(),
                untracked: status_ok.then(|| get("untracked").and_then(number)).flatten(),
                files: if status_ok { files.into_iter().take(FILES_SHOWN).collect() } else { Vec::new() },
                last,
                upstream: get("upstream").map(String::from),
                stashes: get("stashes").and_then(number),
            }))
        }
        Some(_) => None,
    };
    Extra {
        now: get("now").map(String::from),
        uptime: get("uptime").and_then(number),
        pid1: get("pid1").map(String::from),
        pid1_age: get("pid1_age").and_then(number),
        load: get("load").map(String::from),
        cpus: get("cpus").and_then(number),
        nvidia_smi: get("nvsmi").map(|v| v == "yes"),
        df_root: get("df_root").and_then(df),
        df_home: get("df_home").and_then(df),
        df_workspace: match get("df_workspace") {
            Some("none") => Some(None),
            Some(v) => df(v).map(Some),
            None => None,
        },
        home: get("home").map(String::from),
        home_kb: get("home_kb").and_then(number),
        who: list(get("who"), who),
        tmux: match get("tmux") {
            Some("ok") => Some(Ok(tmux.into_iter().take(20).collect())),
            Some("absent") => Some(Err("not installed".to_string())),
            Some(_) => Some(Err("couldn't be read".to_string())),
            None => None,
        },
        procs: match get("ps") {
            // Lines that don't read as processes, and none that do: garbled, not "none".
            Some("ok") if ps.is_empty() && bad_ps > 0 => Some(Err("couldn't be read".to_string())),
            Some("ok") => Some(Ok(ps)),
            Some(_) => Some(Err("ps unavailable".to_string())),
            None => None,
        },
        git,
        done: get("done") == Some("1"),
    }
}

/// What the one call to a pod gave.
#[derive(Debug, Clone)]
pub enum Look {
    /// It answered — fully or in part ([`Reply`] says which parts).
    Probed(Box<Reply>),
    /// It was tried and gave nothing usable: unreachable, timed out, failed.
    Failed(String),
    /// Not tried: the pod is stopped or has no SSH endpoint.
    NotTried(String),
}

/// What this machine knows about a pod besides the pod itself (`super::local_context`).
#[derive(Debug, Clone, Default)]
pub struct Local {
    /// The last `pods test --deep` verdict, rendered (`None`: never checked here).
    pub health: Option<String>,
    /// The local file backups (`pods pull`) of it, rendered (`None`: not looked for).
    pub backups: Option<String>,
    /// The branch today's backups go to, for a cohort pod when the start date is known.
    pub expected_branch: Option<String>,
    /// The backup's record of its last copy of the pod while an operation was in progress on it
    /// (`busy-copies.json`), rendered, masked — or that the record can't be read. `None`: none
    /// recorded.
    pub busy_copy: Option<String>,
    /// `BUSY_AUTO_RESOLVE_HOURS` in words: whether, and after how long, `pods backup` finishes an
    /// operation left in progress (`None`: unknown).
    pub auto_finish: Option<String>,
}

/// `3h12m`, `2d4h`, `45s`.
pub fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{}m", s / 3_600, s % 3_600 / 60),
        s => format!("{}d{}h", s / 86_400, s % 86_400 / 3_600),
    }
}

fn gib(kb: u64) -> String {
    format!("{:.1} GiB", kb as f64 / 1_048_576.0)
}

fn df_text(d: &Df) -> String {
    let pct = (d.used_kb * 100).checked_div(d.total_kb).unwrap_or(0);
    format!("{} of {} used ({pct}%)", gib(d.used_kb), gib(d.total_kb))
}

pub(super) const UNKNOWN: &str = "unknown";

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "no",
        None => UNKNOWN,
    }
}

/// A run of lines in a bundle: fixed `head` lines, then `items` a cap can trim (oldest or
/// last first), saying how many it dropped.
struct Block {
    head: Vec<String>,
    items: Vec<String>,
    /// 0: never trimmed; otherwise lower trims first.
    order: u8,
    /// Keep at least this many items.
    min: usize,
    /// Trim from the front (a log: its oldest lines).
    front: bool,
    dropped: usize,
}

impl Block {
    fn fixed(lines: Vec<String>) -> Block {
        Block { head: lines, items: Vec::new(), order: 0, min: 0, front: false, dropped: 0 }
    }

    fn list(head: String, items: Vec<String>, order: u8, min: usize, front: bool) -> Block {
        Block { head: vec![head], items, order, min, front, dropped: 0 }
    }

    fn render(&self, out: &mut String) {
        for l in &self.head {
            out.push_str(l);
            out.push('\n');
        }
        let note = |n: usize| format!("    (… {n} more not shown)\n");
        if self.front && self.dropped > 0 {
            out.push_str(&note(self.dropped));
        }
        for i in &self.items {
            out.push_str("    ");
            out.push_str(i);
            out.push('\n');
        }
        if !self.front && self.dropped > 0 {
            out.push_str(&note(self.dropped));
        }
    }
}

fn render_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    blocks.iter().for_each(|b| b.render(&mut out));
    out
}

/// The blocks rendered within `cap` bytes: items trimmed from the cheapest block first
/// (the log's oldest lines, then processes, files, jobs, commits), each down to its minimum;
/// still over, the text is cut at `cap` with a note. The head of a bundle — what the pod is,
/// whether it was reached, its GPU, disks and repo state — is never what goes.
fn fit(mut blocks: Vec<Block>, cap: usize) -> String {
    loop {
        let text = render_blocks(&blocks);
        if text.len() <= cap {
            return text;
        }
        let Some(b) = blocks.iter_mut().filter(|b| b.order > 0 && b.items.len() > b.min).min_by_key(|b| b.order) else {
            const CUT: &str = "[… bundle cut to its size cap]\n";
            let mut end = cap.saturating_sub(CUT.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let end = text[..end].rfind('\n').map_or(end, |i| i + 1);
            return format!("{}{CUT}", &text[..end]);
        };
        if b.front {
            b.items.remove(0);
        } else {
            b.items.pop();
        }
        b.dropped += 1;
    }
}

fn listing_line(pod: &Pod, now: u64) -> String {
    let n = |s: &str| redact::name(s, NAME_MAX);
    let mut parts = vec![format!("provider {}", n(&pod.provider)), format!("id {}", n(&pod.id)), format!("status {}", n(&pod.status))];
    if let Some(d) = pod.status_detail.as_deref().filter(|d| !d.trim().is_empty()) {
        parts.push(format!("({})", redact::text(d, NAME_MAX)));
    }
    if crate::lock::is_locked(pod) {
        parts.push("locked".into());
    }
    parts.push(format!("gpu {}", n(&fleet::gpu_label(pod))));
    parts.push(match pod.cost_per_hr {
        Some(c) if crate::status::bills_hourly(&pod.provider, &pod.status) => {
            format!("{}/h", fleet::fmt_money(fleet::currency_symbol(&pod.provider), c))
        }
        Some(c) => format!("{}/h listed, not billing in this state", fleet::fmt_money(fleet::currency_symbol(&pod.provider), c)),
        None => "price unknown".into(),
    });
    parts.push(format!("ssh {}", n(&fleet::endpoint_label(pod))));
    if let Some(c) = pod.created_at {
        parts.push(format!("created {} ago", age(now.saturating_sub(c))));
    }
    let maint = fleet::maintenance_label(pod.maintenance.as_ref());
    if maint != "-" {
        parts.push(format!("host {}", redact::text(&maint, NAME_MAX)));
    }
    format!("listing: {}", parts.join(" · "))
}

fn local_lines(local: &Local) -> Vec<String> {
    let mut v = vec![format!("last deep check (`pods test --deep`): {}", local.health.as_deref().unwrap_or("never run from here"))];
    if let Some(b) = &local.backups {
        v.push(format!("local file backups (`pods pull`): {b}"));
    }
    if let Some(b) = &local.busy_copy {
        v.push(format!("backup's copies while BUSY (`busy-copies.json`): {b}"));
    }
    if let Some(b) = &local.expected_branch {
        v.push(format!("today's backup branch would be: {}", redact::name(b, NAME_MAX)));
    }
    v
}

fn system_line(m: Option<&PodMetrics>, x: Option<&Extra>) -> String {
    let mut parts = Vec::new();
    match x {
        Some(x) => {
            parts.push(format!("pod clock {}", x.now.as_deref().map_or(UNKNOWN.to_string(), |s| redact::name(s, 40))));
            parts.push(format!("machine up {}", x.uptime.map_or(UNKNOWN.into(), age)));
            let pid1 = x.pid1.as_deref().map(|p| redact::name(p, 40)).unwrap_or_else(|| "pid 1".into());
            parts.push(format!("{pid1} (pid 1) up {}", x.pid1_age.map_or(UNKNOWN.into(), age)));
            parts.push(format!("load {} on {} CPUs", x.load.as_deref().map_or(UNKNOWN.into(), |l| redact::name(l, 40)), x.cpus.map_or(UNKNOWN.into(), |c| c.to_string())));
        }
        None => parts.push("clock, uptime and load unknown".into()),
    }
    let m = m.cloned().unwrap_or_default();
    parts.push(format!("cpu {}", m.cpu_pct.map_or(UNKNOWN.into(), |c| format!("{c}%"))));
    parts.push(format!(
        "memory {}",
        m.host_mem_summary().map_or(UNKNOWN.into(), |(u, t)| format!("{} of {} used", gib(u64::from(u) * 1024), gib(u64::from(t) * 1024)))
    ));
    format!("system: {}", parts.join(" · "))
}

fn gpu_lines(m: Option<&PodMetrics>, x: Option<&Extra>) -> Vec<String> {
    let Some(m) = m else { return vec![format!("gpu: {UNKNOWN} (no reply)")] };
    if m.gpus.is_empty() {
        return vec![match x.and_then(|x| x.nvidia_smi) {
            Some(false) => "gpu: none (no nvidia-smi on the machine)".to_string(),
            Some(true) => "gpu: nvidia-smi is there but listed no GPU (failed?)".to_string(),
            None => format!("gpu: {UNKNOWN} (nvidia-smi gave nothing)"),
        }];
    }
    let mut v = vec![format!("gpu: {}", redact::name(&m.gpu_summary().unwrap_or_default(), NAME_MAX))];
    for (i, g) in m.gpus.iter().enumerate() {
        let opt = |o: Option<u32>, unit: &str| o.map_or(UNKNOWN.to_string(), |n| format!("{n}{unit}"));
        v.push(format!(
            "    #{i}: util {} · memory {}/{} · {}",
            opt(g.util_pct, "%"),
            opt(g.mem_used_mb, " MiB"),
            opt(g.mem_total_mb, " MiB"),
            opt(g.temp_c, "°C")
        ));
    }
    if m.gpu_procs.is_empty() {
        v.push("    processes on the GPUs: none listed".into());
    } else {
        let procs: Vec<String> = m
            .gpu_procs
            .iter()
            .take(8)
            .map(|p| format!("{} {} ({})", p.pid, redact::text(&p.name, 60), p.mem_mb.map_or(UNKNOWN.into(), |mb| format!("{mb} MiB"))))
            .collect();
        v.push(format!("    processes on the GPUs: {}", procs.join("; ")));
    }
    v
}

fn disk_line(m: Option<&PodMetrics>, x: Option<&Extra>) -> String {
    let mut parts = Vec::new();
    match x.and_then(|x| x.df_root.as_ref()) {
        Some(d) => parts.push(format!("/ {}", df_text(d))),
        None => parts.push(format!(
            "/ {}",
            m.and_then(PodMetrics::disk_summary).map_or(UNKNOWN.into(), |(u, t)| df_text(&Df {
                used_kb: u64::from(u) * 1024,
                total_kb: u64::from(t) * 1024,
                mount: "/".into()
            }))
        )),
    }
    if let Some(x) = x {
        match &x.df_workspace {
            Some(Some(d)) => parts.push(format!("/workspace {}", df_text(d))),
            Some(None) => parts.push("no /workspace".into()),
            None => parts.push(format!("/workspace {UNKNOWN}")),
        }
        let home = x.home.as_deref().map_or("~".to_string(), |h| redact::name(h, 60));
        let on = match (&x.df_home, &x.df_root) {
            (Some(h), Some(r)) if h.mount == r.mount => "on /".to_string(),
            (Some(h), _) => format!("on {} ({})", redact::name(&h.mount, 60), df_text(h)),
            (None, _) => format!("filesystem {UNKNOWN}"),
        };
        parts.push(format!("home {home} {on}, holds {}", x.home_kb.map_or(UNKNOWN.into(), gib)));
    }
    format!("disk: {}", parts.join(" · "))
}

fn setup_line(m: Option<&PodMetrics>) -> Option<String> {
    let m = m?;
    let mut line = format!(
        "setup: ~/.name {} · deploy key {} · API keys in the shell rc {} · HF token {} · Claude Code token {}",
        yes_no(m.has_name),
        yes_no(m.has_key),
        yes_no(m.has_api_key),
        yes_no(m.has_hf_token),
        yes_no(m.has_cc_token)
    );
    if let Some(p) = &m.progress {
        line.push_str(&format!(" · progress (PROGRESS_CMD): {}", redact::text(p, NAME_MAX)));
    }
    Some(line)
}

fn sessions_line(x: Option<&Extra>, now: Option<u64>) -> String {
    let Some(x) = x else { return format!("sessions: {UNKNOWN}") };
    let who = match &x.who {
        Some(Ok(v)) if v.is_empty() => "no logins (who)".to_string(),
        Some(Ok(v)) => format!(
            "{} login(s) (who): {}",
            v.len(),
            v.iter().take(6).map(|w| redact::name(w, 60)).collect::<Vec<_>>().join("; ")
        ),
        Some(Err(why)) => format!("logins (who): {why}"),
        None => format!("logins: {UNKNOWN}"),
    };
    let tmux = match &x.tmux {
        Some(Ok(v)) if v.is_empty() => "no tmux sessions".to_string(),
        Some(Ok(v)) => {
            let s: Vec<String> = v
                .iter()
                .take(8)
                .map(|t| {
                    let mut d = format!("{} ({} windows", redact::name(&t.name, 40), t.windows.map_or(UNKNOWN.into(), |w| w.to_string()));
                    if t.attached.is_some_and(|a| a > 0) {
                        d.push_str(", attached");
                    }
                    if let (Some(a), Some(now)) = (t.activity, now) {
                        d.push_str(&format!(", active {} ago", age(now.saturating_sub(a))));
                    }
                    d.push(')');
                    d
                })
                .collect();
            format!("{} tmux session(s): {}", v.len(), s.join("; "))
        }
        Some(Err(why)) => format!("tmux: {why}"),
        None => format!("tmux: {UNKNOWN}"),
    };
    format!("sessions: {who} · {tmux}")
}

/// The pod clock as unix seconds, from the extra probe's `now`.
fn pod_now(x: Option<&Extra>) -> Option<u64> {
    let s = x?.now.as_deref()?;
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<u64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let days = crate::schedule::days_from_civil(y, u32::try_from(mo).ok()?, u32::try_from(da).ok()?);
    u64::try_from(days).ok().map(|d| d * 86_400 + h * 3_600 + mi * 60 + se)
}

/// What an operation in progress (`op`: the probe's `busy_op` line) means for the pod's backup:
/// BUSY — nothing committed or pushed, its files copied to this machine instead — how the
/// participant ends it (the backup's own words), and whether `pods backup` would finish it once
/// left alone (`BUSY_AUTO_RESOLVE_HOURS`: a merge, a single cherry-pick or a stash pop's
/// conflicts, never the rest). `None` for no operation, or one the backup doesn't stop for
/// (`bisect`). Pure.
fn busy_line(op: &str, branch: &str, local: &Local) -> Option<String> {
    let mut words = op.split_whitespace();
    let what = words.next()?;
    let unmerged = words.next() == Some("unmerged");
    if !crate::backup::BUSY_WORDS.contains(&what) {
        return None;
    }
    let noun = crate::backup::busy_noun(what);
    let finish = if crate::resolve::finishes(what) {
        format!("auto-finish by `pods backup` (BUSY_AUTO_RESOLVE_HOURS): {}", local.auto_finish.as_deref().map_or(UNKNOWN.to_string(), |w| redact::name(w, LINE_MAX)))
    } else {
        format!("`pods backup` never finishes {noun} on its own")
    };
    let recorded = if local.busy_copy.is_none() { " · no copy of it recorded here yet" } else { "" };
    Some(format!(
        "    backup: BUSY — {}, so the git backup commits and pushes nothing (the pod's files are copied to this machine \
         instead) · the participant ends it: {} · {finish}{recorded}",
        crate::backup::busy_situation(what, branch),
        crate::backup::busy_advice(what, unmerged)
    ))
}

fn repo_blocks(repo_path: &str, m: Option<&PodMetrics>, x: Option<&Extra>, local: &Local, now: u64) -> Vec<Block> {
    let path = redact::name(repo_path, NAME_MAX);
    let origin = m.and_then(|m| m.origin.as_deref()).map_or(UNKNOWN.to_string(), |o| redact::text(o, NAME_MAX));
    let sync = match m.map(|m| (m.ahead, m.behind)) {
        Some((Some(a), Some(b))) => format!("{a} commit(s) not pushed, {b} behind its upstream"),
        Some(_) => "ahead/behind unknown (no upstream?)".into(),
        None => format!("ahead/behind {UNKNOWN}"),
    };
    let repo = match x.and_then(|x| x.git.as_ref()) {
        None => {
            // The extra probe didn't say; the metrics probe's branch is the best there is.
            let branch = m.and_then(|m| m.branch.as_deref()).map_or(UNKNOWN.to_string(), |b| redact::name(b, NAME_MAX));
            return vec![Block::fixed(vec![format!("repo {path}: branch {branch} · origin {origin} · {sync} · working tree {UNKNOWN}")])];
        }
        Some(Git::NotSet) => return vec![Block::fixed(vec!["repo: no repo path configured".into()])],
        Some(Git::Missing) => return vec![Block::fixed(vec![format!("repo {path}: not there")])],
        Some(Git::NoGit) => return vec![Block::fixed(vec![format!("repo {path}: git isn't installed")])],
        Some(Git::NotRepo) => return vec![Block::fixed(vec![format!("repo {path}: not a git checkout")])],
        Some(Git::Repo(r)) => r,
    };
    let mut head = Vec::new();
    let real = repo.real.as_deref().filter(|r| *r != repo_path).map(|r| format!(" (really {})", redact::name(r, NAME_MAX))).unwrap_or_default();
    let branch = repo.branch.as_deref().map_or(UNKNOWN.to_string(), |b| redact::name(b, NAME_MAX));
    let upstream = repo.upstream.as_deref().map_or("none".to_string(), |u| redact::name(u, NAME_MAX));
    head.push(format!("repo {path}{real}: branch {branch} · upstream {upstream} · origin {origin} · {sync}"));
    let op = match repo.op.as_deref() {
        Some("") => "none".to_string(),
        Some(op) => redact::name(op, 40),
        None => UNKNOWN.to_string(),
    };
    let count = |n: Option<u32>| n.map_or(UNKNOWN.to_string(), |n| n.to_string());
    head.push(format!(
        "    working tree: {} modified/staged, {} untracked · operation in progress: {op} · stashes {}",
        count(repo.modified),
        count(repo.untracked),
        count(repo.stashes)
    ));
    head.extend(repo.op.as_deref().and_then(|op| busy_line(op, &branch, local)));
    head.push(match &repo.last {
        Some((t, subject)) => format!("    last commit {} ago: {}", age(now.saturating_sub(*t)), redact::text(subject, NAME_MAX)),
        None => format!("    last commit: {UNKNOWN}"),
    });
    let mut blocks = vec![Block::fixed(head)];
    let recent: Vec<String> = m.map(|m| m.recent_commits.iter().map(|c| redact::text(c, NAME_MAX)).collect()).unwrap_or_default();
    if !recent.is_empty() {
        blocks.push(Block::list("    recent commits (newest first):".into(), recent, 5, 1, false));
    }
    if !repo.files.is_empty() {
        let files: Vec<String> = repo.files.iter().map(|f| status_line(f)).collect();
        blocks.push(Block::list(
            format!("    changed files (git status XY codes, `.` = unchanged; a file only touched can show as M; the first {FILES_SHOWN}):"),
            files,
            3,
            5,
            false,
        ));
    }
    blocks
}

/// A `git status --porcelain` line (`XY path`) for the prompt: the two status letters (a space
/// shown as `.`, as one-line text would lose it), then the name.
fn status_line(line: &str) -> String {
    match (line.get(..2), line.get(2..3), line.get(3..)) {
        (Some(xy), Some(" "), Some(path)) if xy.bytes().all(|b| b.is_ascii_graphic() || b == b' ') => {
            format!("{} {}", xy.replace(' ', "."), redact::name(path, NAME_MAX))
        }
        _ => redact::name(line, NAME_MAX),
    }
}

fn procs_block(x: Option<&Extra>) -> Block {
    match x.and_then(|x| x.procs.as_ref()) {
        Some(Ok(ps)) if !ps.is_empty() => {
            let items = ps
                .iter()
                .take(PROCS_SHOWN)
                .map(|p| {
                    format!("{} {} cpu {:.1}% mem {:.1}% up {}: {}", p.pid, redact::name(&p.user, 20), p.cpu, p.mem, age(p.age), redact::text(&p.args, ARGS_MAX))
                })
                .collect();
            Block::list(
                "processes (busiest by CPU — ps's %cpu is the average since each started — then by memory; this probe's own ssh session included):".into(),
                items,
                2,
                5,
                false,
            )
        }
        Some(Ok(_)) => Block::fixed(vec!["processes: none listed".into()]),
        Some(Err(why)) => Block::fixed(vec![format!("processes: {why}")]),
        None => Block::fixed(vec![format!("processes: {UNKNOWN}")]),
    }
}

fn job_text(j: &JobInfo) -> String {
    let started = j.started.as_deref().map(|s| format!(", started {}", redact::name(s, 30))).unwrap_or_default();
    format!("{} — {}{started}: {}", j.id, jobs::state_label(j), redact::text(&j.cmd, ARGS_MAX))
}

fn jobs_blocks(r: &Reply) -> Vec<Block> {
    let mut v = Vec::new();
    match &r.jobs {
        Some(Ok(list)) if list.is_empty() => v.push(Block::fixed(vec!["jobs (`pods run --background`): none".into()])),
        Some(Ok(list)) => v.push(Block::list(
            format!("jobs (`pods run --background`, newest first, {} in all):", list.len()),
            list.iter().take(JOBS_SHOWN).map(job_text).collect(),
            4,
            2,
            false,
        )),
        Some(Err(e)) => v.push(Block::fixed(vec![format!("jobs: couldn't be read ({})", redact::text(e, NAME_MAX))])),
        None => v.push(Block::fixed(vec![format!("jobs: {UNKNOWN}")])),
    }
    match &r.log {
        Some(Ok(ReadReply::Job { info, chunk })) => {
            // The whole window read, masked before it's cut to the last LOG_LINES (a key block
            // across the cut must be seen whole to be masked at all).
            let (window, _) = jobs::snapshot_lines(chunk, usize::MAX);
            let (lines, hidden) = redact::log_tail(&window, chunk.start > 0, LOG_LINES, LINE_MAX);
            let state = if info.state == JobState::Running { "still running".to_string() } else { jobs::state_label(info) };
            let more = if hidden > 0 || chunk.start > 0 { ", older lines exist" } else { "" };
            if lines.is_empty() {
                v.push(Block::fixed(vec![format!("newest job's log ({}, {state}): empty", info.id)]));
            } else {
                v.push(Block::list(format!("newest job's log ({}, {state}), last {} lines{more}:", info.id, lines.len()), lines, 1, 8, true));
            }
        }
        // No jobs (or no such job): the jobs line already says so.
        Some(Ok(_)) => {}
        Some(Err(e)) => v.push(Block::fixed(vec![format!("newest job's log: couldn't be read ({})", redact::text(e, NAME_MAX))])),
        None => v.push(Block::fixed(vec![format!("newest job's log: {UNKNOWN}")])),
    }
    v
}

/// One pod's bundle: what it is, what this machine knows of it, and what the call found — at
/// most `cap` bytes, every field masked. `repo_path` is the configured checkout; `now` this
/// machine's clock (unix seconds, for the listing's ages).
pub fn render(pod: &Pod, look: &Look, local: &Local, repo_path: &str, now: u64, cap: usize) -> String {
    let name = if pod.name.trim().is_empty() { "(no name)".to_string() } else { redact::name(&pod.name, NAME_MAX) };
    let mut head = vec![format!("## {name}"), listing_line(pod, now)];
    head.extend(local_lines(local));
    let reply = match look {
        Look::NotTried(why) => {
            head.push(format!("reached: no — not tried: {}", redact::text(why, LINE_MAX)));
            return fit(vec![Block::fixed(head)], cap);
        }
        Look::Failed(why) => {
            head.push(format!("reached: no — unreachable: {} (everything below the listing is unknown)", redact::text(why, LINE_MAX)));
            return fit(vec![Block::fixed(head)], cap);
        }
        Look::Probed(r) => r,
    };
    let m = reply.metrics.as_ref();
    let x = reply.extra.as_ref();
    let complete = if reply.complete && x.is_some_and(|x| x.done) { "" } else { " — the reply was cut short or partly garbled: what's missing is unknown" };
    head.push(format!("reached: yes{complete}"));
    head.push(system_line(m, x));
    head.extend(gpu_lines(m, x));
    head.push(disk_line(m, x));
    head.extend(setup_line(m));
    head.push(sessions_line(x, pod_now(x)));
    let mut blocks = vec![Block::fixed(head)];
    // Ages of commits run on the pod's clock when there is one (a skewed control box
    // shouldn't make a fresh commit look old).
    let clock = pod_now(x).unwrap_or(now);
    blocks.extend(repo_blocks(repo_path, m, x, local, clock));
    blocks.push(procs_block(x));
    blocks.extend(jobs_blocks(reply));
    fit(blocks, cap)
}

/// Canned replies in the probe's exact format, for tests here and in the CLI/TUI.
#[cfg(any(test, feature = "test-util"))]
pub mod fixtures {
    use super::*;

    /// A healthy busy GPU pod's whole reply: 1×A4000 at 97 %, a training run, a dirty tree with a
    /// merge in progress, a tmux session, one running job and its log. `extra_args` goes into the
    /// training process's arguments, `log_extra` into the job's log, `job_cmd` is its command —
    /// where tests plant keys.
    pub fn busy_reply(extra_args: &str, log_extra: &str, job_cmd: &str) -> String {
        let metrics = format!(
            "NVIDIA RTX A4000, 97, 14000, 16376, 71\n{}\nbranch=autocommit-devtest-w1d2-alpha\norigin=git@github.com:nickypro/arena-sandbox-materials.git\n\
             committed=1792403200\ndirty=3\nsync=0\t2\nclog=a1b2c3d  2 hours ago  arena backup devtest-alpha\nname=1\napikey=1\nhf=1\ncc=0\nkey=1\n\
             disk=12582912 104857600\ncpu=37\nhostmem=8388608 16777216\n{}\n4242, 13000, /opt/conda/envs/arena-env/bin/python\n",
            metrics::SENTINEL,
            metrics::PROC_SENTINEL
        );
        let job = jobs::fixtures::job_line("20261019-101500-pytest", "running", None, Some(777), job_cmd);
        let log: String = ["collecting ...", "test_a PASSED", log_extra, "test_b PASSED"].iter().filter(|l| !l.is_empty()).map(|l| format!("{l}\n")).collect();
        let extra = format!(
            "ARENA_ASK now=2026-10-19T12:00:00Z\nARENA_ASK uptime=1036800\nARENA_ASK load=1.02 0.98 0.90\nARENA_ASK cpus=16\nARENA_ASK pid1=sleep\n\
             ARENA_ASK pid1_age=11400\nARENA_ASK nvsmi=yes\nARENA_ASK df_root=12582912 104857600 /\nARENA_ASK home=/root\n\
             ARENA_ASK df_home=12582912 104857600 /\nARENA_ASK df_workspace=5242880 52428800 /workspace\nARENA_ASK home_kb=3355443\n\
             ARENA_ASK who=ok\nARENA_ASK_WHO root pts/0 2026-10-19 10:00\nARENA_ASK tmux=ok\nARENA_ASK_TMUX lab\t2\t1\t1792410900\n\
             ARENA_ASK ps=ok\nARENA_ASK_PS  4242 root      98.5  6.1   7200 python train.py --epochs 3 {extra_args}\n\
             ARENA_ASK_PS   777 root       0.0  0.0   3600 sh /root/.arena/jobs/20261019-101500-pytest/run\n\
             ARENA_ASK_PS  4242 root      98.5  6.1   7200 python train.py (a duplicate pid, from the by-memory list)\n\
             ARENA_ASK git=ok\nARENA_ASK real=/workspace/ARENA_materials\nARENA_ASK branch=autocommit-devtest-w1d2-alpha\nARENA_ASK op=merge unmerged\n\
             ARENA_ASK status=ok\nARENA_ASK modified=2\nARENA_ASK untracked=1\nARENA_ASK_FILE UU chapter1/solutions.py\n\
             ARENA_ASK_FILE  M chapter1/part4_interp_on_algorithmic_model.ipynb\nARENA_ASK_FILE ?? notes.txt\n\
             ARENA_ASK last=1792403200 Merge branch main\nARENA_ASK upstream=origin/autocommit-devtest-w1d2-alpha\nARENA_ASK stashes=1\nARENA_ASK done=1\n"
        );
        format!(
            "Welcome to the pod!\n{BEGIN}\n{metrics}{JOBS}\n{}{LOG}\n{}{EXTRA}\n{extra}{END}\n",
            jobs::fixtures::list_reply(std::slice::from_ref(&job)),
            jobs::fixtures::read_reply(&job, 0, log.len() as u64, log.as_bytes())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::busy_reply;
    use super::*;
    use crate::ask::redact::tests::{body, one_key_per_family, ALNUM, URLSAFE};

    const NOW: u64 = 1_792_411_200; // 2026-10-19T12:00:00Z, the fixture's pod clock

    fn pod() -> Pod {
        Pod {
            id: "rp1".into(),
            name: "devtest-alpha".into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: Some("NVIDIA RTX A4000".into()),
            gpu_count: Some(1),
            cost_per_hr: Some(0.17),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(22001),
            created_at: Some(NOW - 11_520),
            ..Default::default()
        }
    }

    fn local() -> Local {
        Local {
            health: Some("pass 12m ago".into()),
            backups: Some("snapshots w1d2, w1d1 (newest first); all-files mirror: yes".into()),
            expected_branch: Some("autocommit-devtest-w1d2-alpha".into()),
            busy_copy: Some("last whole copy while BUSY with a merge: taken 40m ago, snapshot w1d2/devtest-alpha, still on this machine".into()),
            auto_finish: Some("after 3h with nothing changed (default)".into()),
        }
    }

    fn bundle(stdout: &str, cap: usize) -> String {
        let look = parse_reply(stdout).map_or_else(|| Look::Failed("no reply".into()), |r| Look::Probed(Box::new(r)));
        render(&pod(), &look, &local(), "/root/ARENA_materials", NOW, cap)
    }

    #[test]
    fn a_full_reply_reads_as_the_pods_state() {
        let r = parse_reply(&busy_reply("", "", "pytest -x chapter1")).unwrap();
        assert!(r.complete);
        let x = r.extra.as_ref().unwrap();
        assert!(x.done);
        assert_eq!(x.procs.as_ref().unwrap().as_ref().unwrap().len(), 2, "one per pid");
        let Some(Git::Repo(repo)) = &x.git else { panic!("{:?}", x.git) };
        assert_eq!((repo.modified, repo.untracked, repo.op.as_deref()), (Some(2), Some(1), Some("merge unmerged")));
        let text = render(&pod(), &Look::Probed(Box::new(r)), &local(), "/root/ARENA_materials", NOW, BUNDLE_CAP);
        for want in [
            "## devtest-alpha",
            "listing: provider runpod · id rp1 · status RUNNING · gpu 1×RTX A4000 · $0.17/h · ssh 10.0.0.1:22001 · created 3h12m ago",
            "last deep check (`pods test --deep`): pass 12m ago",
            "today's backup branch would be: autocommit-devtest-w1d2-alpha",
            "reached: yes\n",
            "pod clock 2026-10-19T12:00:00Z · machine up 12d0h · sleep (pid 1) up 3h10m · load 1.02 0.98 0.90 on 16 CPUs · cpu 37%",
            "gpu: 1×RTX A4000",
            "#0: util 97% · memory 14000 MiB/16376 MiB · 71°C",
            "processes on the GPUs: 4242 /opt/conda/envs/arena-env/bin/python (13000 MiB)",
            "disk: / 12.0 GiB of 100.0 GiB used (12%) · /workspace 5.0 GiB of 50.0 GiB used (10%) · home /root on /, holds 3.2 GiB",
            "setup: ~/.name yes · deploy key yes · API keys in the shell rc yes · HF token yes · Claude Code token no",
            "sessions: 1 login(s) (who): root pts/0 2026-10-19 10:00 · 1 tmux session(s): lab (2 windows, attached, active 5m ago)",
            "repo /root/ARENA_materials (really /workspace/ARENA_materials): branch autocommit-devtest-w1d2-alpha · upstream origin/autocommit-devtest-w1d2-alpha",
            "2 commit(s) not pushed, 0 behind its upstream",
            "working tree: 2 modified/staged, 1 untracked · operation in progress: merge unmerged · stashes 1",
            "\n    backup: BUSY — a merge is in progress on autocommit-devtest-w1d2-alpha, so the git backup commits and pushes \
             nothing (the pod's files are copied to this machine instead) · the participant ends it: resolve the conflicts and \
             `git add` them, then `git commit` — or abort it (`git merge --abort`) · auto-finish by `pods backup` \
             (BUSY_AUTO_RESOLVE_HOURS): after 3h with nothing changed (default)\n",
            "backup's copies while BUSY (`busy-copies.json`): last whole copy while BUSY with a merge: taken 40m ago, snapshot \
             w1d2/devtest-alpha, still on this machine\n",
            "last commit 2h13m ago: Merge branch main",
            "    UU chapter1/solutions.py",
            "    .M chapter1/part4_interp_on_algorithmic_model.ipynb",
            "    ?? notes.txt",
            "4242 root cpu 98.5% mem 6.1% up 2h0m: python train.py --epochs 3",
            "20261019-101500-pytest — running (pid 777), started 2026-10-08T14:23:01Z: pytest -x chapter1",
            "newest job's log (20261019-101500-pytest, still running), last 3 lines:",
            "    test_b PASSED",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains("Welcome"), "shell chatter before the begin marker is dropped");
        assert!(!text.contains("unknown"), "{text}");
        assert!(text.len() <= BUNDLE_CAP);
    }

    /// Every prefix of a full reply (a cut-off call), and garbage: never a panic, never over the
    /// cap, and what's missing says `unknown` — never "none".
    #[test]
    fn a_cut_or_garbled_reply_is_unknown_never_a_panic() {
        let full = busy_reply("", "", "pytest");
        let mut at = 0;
        while at <= full.len() {
            let cut = &full[..at];
            let text = bundle(cut, BUNDLE_CAP);
            assert!(text.len() <= BUNDLE_CAP, "{at}");
            assert!(text.starts_with("## devtest-alpha\nlisting: "), "{at}: {text}");
            at += 7;
            while !full.is_char_boundary(at) && at < full.len() {
                at += 1;
            }
        }
        // Just the begin marker: reached, but nothing known.
        let text = bundle(&format!("{BEGIN}\n"), BUNDLE_CAP);
        for want in ["reached: yes — the reply was cut short", "clock, uptime and load unknown", "gpu: unknown", "sessions: unknown", "processes: unknown", "jobs: unknown", "working tree unknown"] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains("none"), "unknown is never none:\n{text}");
        // Garbage inside every section: each reads as unknown or unreadable.
        let junk = format!("{BEGIN}\n\u{0}\u{1b}[2J\n{JOBS}\nARENA_JOB\tnot-an-id\n{LOG}\nARENA_LOG x y\n{EXTRA}\nARENA_ASK git=ok\nARENA_ASK_PS x y z\nARENA_ASK ps=ok\nARENA_ASK uptime=12abc\n{END}\n");
        let text = bundle(&junk, BUNDLE_CAP);
        assert!(text.contains("jobs: couldn't be read") && text.contains("newest job's log: couldn't be read"), "{text}");
        assert!(text.contains("machine up unknown") && text.contains("working tree: unknown modified/staged"), "{text}");
        assert!(text.contains("processes: couldn't be read"), "a garbled ps line isn't \"none\": {text}");
        assert!(!text.contains('\u{1b}') && !text.contains('\u{0}'), "{text:?}");
        // Not our reply at all.
        assert!(parse_reply("bash: sh: command not found\n").is_none());
        assert!(bundle("", BUNDLE_CAP).contains("reached: no — unreachable: no reply"));
    }

    /// The keys of every [`crate::backup::SECRET_PATTERNS`] family, and a 40-character token,
    /// in a process's arguments, the job's command and its log, a tmux session, a commit subject
    /// and a file name: none reaches the bundle.
    #[test]
    fn keys_in_args_and_logs_never_reach_the_bundle() {
        let keys = one_key_per_family();
        let token = body(URLSAFE, 40, 9);
        let all: Vec<String> = keys.iter().map(|(k, _)| k.clone()).chain([token.clone()]).collect();
        let args = all.iter().enumerate().map(|(i, k)| format!("--opt{i} {k}")).collect::<Vec<_>>().join(" ");
        let log = all.iter().map(|k| format!("Using key {k} for the run")).collect::<Vec<_>>().join("\n");
        let cmd = format!("python eval.py --key={} --hf {}", keys[0].0, keys[5].0);
        let mut reply = busy_reply(&args, &log, &cmd);
        reply = reply.replace("ARENA_ASK_TMUX lab\t", &format!("ARENA_ASK_TMUX lab-{}\t", keys[6].0));
        reply = reply.replace("ARENA_ASK last=1792403200 Merge branch main", &format!("ARENA_ASK last=1792403200 add key {}", keys[1].0));
        reply = reply.replace("ARENA_ASK_FILE ?? notes.txt", &format!("ARENA_ASK_FILE ?? {}.txt", keys[11].0));
        // Room for every planted line: the cap is the size test's business.
        let text = bundle(&reply, 64 * 1024);
        for k in &all {
            assert!(!text.contains(k.as_str()), "a key reached the bundle: {}…\n{text}", &k[..12]);
            // Not even its last 16 characters.
            assert!(!text.contains(&k[k.len() - 16..]), "{}…", &k[..12]);
        }
        for (_, repl) in &keys {
            assert!(text.contains(repl), "{repl} in\n{text}");
        }
        assert!(text.contains(redact::MASK), "the plain token is masked");
        // The rest of those lines survives.
        assert!(text.contains("python train.py --epochs 3 --opt0 REDACTED-OPENROUTER-KEY"), "{text}");
        assert!(text.contains("Using key REDACTED-AWS-ACCESS-KEY-ID for the run"), "{text}");
    }

    /// The bundle for real at both cuts: the remote read's last bytes begin inside a key (its
    /// END in the window, or not), and a whole key block straddles where the 40-line cut falls —
    /// no 12-character piece of any key line reaches the bundle.
    #[test]
    fn a_log_tail_starting_inside_a_key_never_shows_it() {
        use crate::ask::redact::tests::key_body;
        let job = jobs::fixtures::job_line("20261019-101500-pytest", "running", None, Some(777), "pytest");
        let with_log = |start: u64, log: &str| {
            let full = busy_reply("", "", "pytest");
            let (head, rest) = full.split_once(&format!("{LOG}\n")).unwrap();
            let (_, tail) = rest.split_once(&format!("{EXTRA}\n")).unwrap();
            let read = jobs::fixtures::read_reply(&job, start, start + log.len() as u64, log.as_bytes());
            format!("{head}{LOG}\n{read}{EXTRA}\n{tail}")
        };
        let key = key_body(40, 7);
        let lines = |v: &[String]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
        let progress: String = (0..30).map(|i| format!("step {i} loss 0.{i}\n")).collect();
        let cases = [
            // The 16 KiB read began mid-line inside the key; its END is in the window.
            (9000, format!("{}{}-----END OPENSSH PRIVATE KEY-----\n{progress}", &key[4][10..], lines(&key[5..]))),
            // The same, the key still being printed (no END yet), then a line.
            (9000, format!("{}{}still printing\n", &key[4][10..], lines(&key[5..30]))),
            // The whole log: a block, then 30 lines — the 40-line cut lands in its body.
            (0, format!("start\n-----BEGIN OPENSSH PRIVATE KEY-----\n{}-----END OPENSSH PRIVATE KEY-----\n{progress}", lines(&key))),
        ];
        for (n, (start, log)) in cases.iter().enumerate() {
            let text = bundle(&with_log(*start, log), 64 * 1024);
            assert!(text.contains("newest job's log (20261019-101500-pytest, still running), last "), "{n}: {text}");
            for l in &key {
                for piece in l.as_bytes().chunks(12).filter(|c| c.len() == 12) {
                    let piece = std::str::from_utf8(piece).unwrap();
                    assert!(!text.contains(piece), "case {n}: {piece} of a key line reached the bundle:\n{text}");
                }
            }
            assert_eq!(text.contains(redact::PEM_MASK), n != 1, "{n}: {text}");
        }
    }

    /// A pod with a flood of everything still fits the cap: the log's oldest lines go first, then
    /// processes, files, jobs; the head — listing, reach, GPU, disk, repo — stays whole.
    #[test]
    fn a_bundle_never_exceeds_its_cap() {
        let long_args = format!("--x {}", "word ".repeat(80));
        let log: String = (0..2000).map(|i| format!("step {i} loss {}", "0.123 ".repeat(30))).collect::<Vec<_>>().join("\n");
        let mut reply = busy_reply(&long_args, &log, "pytest");
        let extra_procs: String = (0..30).map(|i| format!("ARENA_ASK_PS {} root 1.0 1.0 60 {}\n", 5000 + i, "python worker.py ".repeat(20))).collect();
        reply = reply.replace("ARENA_ASK git=ok\n", &format!("{extra_procs}ARENA_ASK git=ok\n"));
        let files: String = (0..20).map(|i| format!("ARENA_ASK_FILE ?? data/run-{i}-{}.json\n", body(ALNUM, 60, i))).collect();
        reply = reply.replace("ARENA_ASK last=", &format!("{files}ARENA_ASK last="));
        for cap in [BUNDLE_CAP, 3000, MIN_CAP] {
            let text = bundle(&reply, cap);
            assert!(text.len() <= cap, "{cap}: {} bytes", text.len());
            assert!(text.starts_with("## devtest-alpha\nlisting: "), "{cap}");
            assert!(text.contains("reached: yes") && text.contains("gpu: 1×RTX A4000"), "{cap}:\n{text}");
        }
        let text = bundle(&reply, BUNDLE_CAP);
        assert!(text.contains("more not shown"), "{text}");
        assert!(text.contains("repo /root/ARENA_materials") && text.contains("disk: /"), "{text}");
        // A tiny cap cuts the text itself, saying so.
        let tiny = bundle(&reply, 700);
        assert!(tiny.len() <= 700 && tiny.ends_with("[… bundle cut to its size cap]\n"), "{tiny}");
    }

    /// What an operation in progress means for the backup, per `busy_op` word: BUSY, how the
    /// participant ends it, and whether `pods backup` finishes it on its own (a merge, a single
    /// cherry-pick, a stash pop's conflicts — never `git am`, a rebase, a revert or a sequence);
    /// no line for no operation or one the backup doesn't stop for (bisect); "no copy recorded"
    /// only when the record says so, the setting `unknown` when it isn't known.
    #[test]
    fn an_operation_in_progress_says_the_backup_is_busy_and_how_it_ends() {
        let l = local();
        let line = |op: &str| busy_line(op, "b", &l);
        for (op, situation, ends, finish) in [
            ("merge", "a merge is in progress on b", "finish it (`git commit`) or abort it (`git merge --abort`)", true),
            ("cherry-pick unmerged", "a cherry-pick is in progress on b", "resolve the conflicts and `git add` them, then `git cherry-pick --continue`", true),
            ("conflicts", "b has unresolved conflicts", "resolve them and `git add` them, or undo them (`git reset --merge`)", true),
            ("revert", "a revert is in progress on b", "`git revert --abort`", false),
            ("am", "a `git am` is in progress on b", "`git am --continue`", false),
            ("rebase unmerged", "a rebase is in progress on b", "`git rebase --continue`", false),
            ("sequence", "a cherry-pick or revert sequence is in progress on b", "`git revert --continue` for a revert", false),
        ] {
            let got = line(op).unwrap_or_else(|| panic!("{op}: no line"));
            assert!(got.starts_with(&format!("    backup: BUSY — {situation}, so the git backup commits and pushes nothing")), "{op}: {got}");
            assert!(got.contains(ends), "{op}: {got}");
            assert_eq!(got.contains("auto-finish by `pods backup` (BUSY_AUTO_RESOLVE_HOURS): after 3h"), finish, "{op}: {got}");
            assert_eq!(got.contains("`pods backup` never finishes"), !finish, "{op}: {got}");
            assert!(!got.contains("no copy of it recorded"), "{op}: the record has one: {got}");
        }
        for op in ["", "bisect", "garbage words"] {
            assert_eq!(line(op), None, "{op:?}");
        }
        let bare = busy_line("merge unmerged", "b", &Local::default()).unwrap();
        assert!(bare.ends_with("· auto-finish by `pods backup` (BUSY_AUTO_RESOLVE_HOURS): unknown · no copy of it recorded here yet"), "{bare}");
    }

    #[test]
    fn an_unreachable_or_stopped_pod_is_its_listing_and_why() {
        let mut p = pod();
        let text = render(&p, &Look::Failed("ssh: connect to host 10.0.0.1 port 22001: Connection refused".into()), &Local::default(), "/root/r", NOW, BUNDLE_CAP);
        assert!(text.contains("listing: provider runpod · id rp1 · status RUNNING"), "{text}");
        assert!(text.contains("last deep check (`pods test --deep`): never run from here"), "{text}");
        assert!(text.contains("reached: no — unreachable: ssh: connect to host 10.0.0.1 port 22001: Connection refused (everything below the listing is unknown)"), "{text}");
        assert!(!text.contains("gpu:") && !text.contains("processes"), "{text}");
        p.status = "EXITED".into();
        p.ssh_ip = None;
        let text = render(&p, &Look::NotTried("stopped (EXITED) — no SSH while it's stopped".into()), &Local::default(), "/root/r", NOW, BUNDLE_CAP);
        assert!(text.contains("$0.17/h listed, not billing in this state") && text.contains("ssh -"), "{text}");
        assert!(text.contains("reached: no — not tried: stopped (EXITED)"), "{text}");
    }

    #[test]
    fn the_command_reuses_the_probes_and_quotes_the_repo() {
        let opts = ProbeOpts { repo_path: Some("/root/ARENA_materials".into()), ..Default::default() };
        let c = probe_command(&opts, "/root/it's/");
        assert!(c.contains(&metrics::probe_command(&opts)), "the dashboard's probe, as is");
        assert!(c.contains(&jobs::list_command()) && c.contains(&jobs::read_tail_command(LOG_BYTES)), "the jobs reads, as is");
        assert!(c.ends_with(&format!(" arena-ask-probe '/root/it'\\''s'; echo '{END}'")), "{}", &c[c.len() - 80..]);
        assert!(probe_script().contains("busy_op() {") && !probe_script().contains("@ARENA_BUSY_OP_SH@"));
        assert!(probe_script().contains(metrics::RO_GIT_SH) && !probe_script().contains("@ARENA_RO_GIT_SH@"));
        // No `git status` anywhere it runs (comments aside): it refreshes, filters, asks fsmonitor.
        let code = |text: &str| text.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
        assert!(!code(&probe_script()).contains("git status") && !code(&c).contains("git status"));
    }

    /// Strictly read-only, for real: a repo whose config runs a helper at every turn — an
    /// fsmonitor hook (any index read), a clean filter on every file (made racily clean, so
    /// even plumbing compares contents), a submodule with its own clean filter and a touched
    /// file, a signed HEAD with `log.showSignature` and a `gpg.program` — and each helper writes
    /// a marker. Neither the dashboard's probe nor `pods ask`'s whole command runs any of them,
    /// and the index is untouched; plain git, on the same repo, runs every one (the fixture is
    /// armed).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probes_run_nothing_the_repository_configures() {
        use std::process::{Command, Stdio};
        let have = |t: &str| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success());
        if !["git", "ps", "base64", "timeout", "df", "du", "touch"].iter().all(|t| have(t)) {
            eprintln!("git/ps/base64/timeout/df/du/touch missing — skipping");
            return;
        }
        let t = crate::ask::ai::tests::Tmp::new("helpers");
        let (home, marks) = (t.0.join("home"), t.0.join("marks"));
        let (repo, sub) = (home.join("ARENA_materials"), home.join("ARENA_materials/sub"));
        for d in [&marks, &sub] {
            std::fs::create_dir_all(d).unwrap();
        }
        let helper = |name: &str, then: &str| {
            let p = t.0.join(name);
            crate::remote::write_test_script(&p, format!("#!/bin/sh\necho \"$*\" >> {}/{name}\n{then}\n", marks.display())).unwrap();
            p.display().to_string()
        };
        let (fsmonitor, clean, sub_clean, gpg) =
            (helper("fsmonitor", "exit 1"), helper("clean-filter", "cat"), helper("submodule-clean-filter", "cat"), helper("gpg", "cat >/dev/null; exit 1"));
        let git_in = |dir: &std::path::Path, args: &[&str], input: Option<&str>| {
            let mut c = Command::new("git");
            c.args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main", "-c", "advice.addEmbeddedRepo=false"])
                .args(args)
                .current_dir(dir)
                .env("HOME", &home)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = c.spawn().unwrap();
            if let Some(i) = input {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
            }
            let o = child.wait_with_output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let git = |args: &[&str]| git_in(&repo, args, None);
        // The repo and its submodule, committed before any helper is configured.
        git_in(&sub, &["init", "-q"], None);
        std::fs::write(sub.join("s.txt"), "s\n").unwrap();
        std::fs::write(sub.join(".gitattributes"), "* filter=subevil\n").unwrap();
        git_in(&sub, &["add", "."], None);
        git_in(&sub, &["commit", "-q", "-m", "s"], None);
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        std::fs::write(repo.join(".gitattributes"), "* filter=evil\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        // A signed HEAD: a gpgsig header is all `log --show-signature` needs to call gpg.program.
        let signed = format!(
            "tree {}\nparent {}\nauthor t <t@example.com> 1700000000 +0000\ncommitter t <t@example.com> 1700000000 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\nsigned\n",
            git(&["rev-parse", "HEAD^{tree}"]),
            git(&["rev-parse", "HEAD"])
        );
        let commit = git_in(&repo, &["hash-object", "-w", "-t", "commit", "--stdin"], Some(&signed));
        git(&["update-ref", "refs/heads/main", &commit]);
        // The helpers.
        git_in(&sub, &["config", "filter.subevil.clean", &format!("{sub_clean} %f")], None);
        for (k, v) in [
            ("core.fsmonitor", fsmonitor.as_str()),
            ("filter.evil.clean", &format!("{clean} %f")),
            ("filter.evil.required", "true"),
            ("log.showSignature", "true"),
            ("gpg.program", gpg.as_str()),
        ] {
            git(&["config", k, v]);
        }
        // Work: a.txt modified, notes.txt new; the submodule's file touched (stat-dirty, which
        // its own `git status` would hash through its filter); every index entry racily clean.
        std::fs::write(repo.join("a.txt"), "a2\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(sub.join("s.txt"), "s\n").unwrap();
        let touched = Command::new("touch").args(["-d", "@978307200"]).arg(repo.join(".git/index")).status().unwrap();
        assert!(touched.success());
        let index = std::fs::read(repo.join(".git/index")).unwrap();
        let fired = || {
            let mut v: Vec<String> = std::fs::read_dir(&marks).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let clear = || std::fs::read_dir(&marks).unwrap().flatten().for_each(|e| std::fs::remove_file(e.path()).unwrap());
        clear();

        let opts = ProbeOpts { repo_path: Some(repo.display().to_string()), key_remote: None, progress_cmd: None };
        let sh = |cmd: String| {
            let o = Command::new("sh").arg("-c").arg(cmd).current_dir(&home).env("HOME", &home).output().unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let dashboard = metrics::parse_probe_output(&sh(metrics::probe_command(&opts)));
        assert_eq!(fired(), Vec::<String>::new(), "the dashboard's probe ran the repo's helpers");
        assert_eq!((dashboard.dirty_files, dashboard.recent_commits.first().map(|c| c.ends_with("signed"))), (Some(2), Some(true)), "{dashboard:?}");
        let stdout = sh(probe_command(&opts, &repo.display().to_string()));
        assert_eq!(fired(), Vec::<String>::new(), "`pods ask`'s probe ran the repo's helpers");
        let r = parse_reply(&stdout).unwrap();
        let Some(Git::Repo(g)) = r.extra.as_ref().and_then(|x| x.git.as_ref()) else { panic!("{stdout}") };
        assert_eq!((g.modified, g.untracked, g.files.clone()), (Some(1), Some(1), vec![" M a.txt".to_string(), "?? notes.txt".into()]), "{stdout}");
        assert_eq!(g.last.as_ref().map(|(_, s)| s.as_str()), Some("signed"));
        assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index, "the index was written");

        // Armed: plain git runs every helper here.
        git(&["status", "--porcelain"]);
        assert_eq!(fired(), ["clean-filter", "fsmonitor", "submodule-clean-filter"]);
        clear();
        git(&["-c", "core.fsmonitor=", "diff-files"]);
        assert_eq!(fired(), ["clean-filter", "submodule-clean-filter"], "plumbing hashes racily-clean files through their filter");
        clear();
        git(&["-c", "core.fsmonitor=", "log", "-1", "--format=%ct"]);
        assert_eq!(fired(), ["gpg"]);
        clear();
        git(&["ls-files", "-u"]);
        assert_eq!(fired(), ["fsmonitor"], "busy_op's index read asks fsmonitor");
    }

    /// The command for real, under `sh` against a temp home with a real git checkout (a merge
    /// in progress, a modified and an untracked file, a tracked file touched but unchanged): every
    /// section reads, the repo's state is right, the probe's own processes aren't listed — and
    /// the index isn't written (`GIT_OPTIONAL_LOCKS=0`).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_runs_for_real_against_a_local_shell() {
        use std::process::Command;
        let have = |t: &str| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success());
        if !["git", "ps", "base64", "timeout", "df", "du"].iter().all(|t| have(t)) {
            eprintln!("git/ps/base64/timeout/df/du missing — skipping");
            return;
        }
        let t = crate::ask::ai::tests::Tmp::new("probe");
        let home = t.0.join("home");
        let repo = home.join("ARENA_materials");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(&repo)
                .env("HOME", &home)
                .output()
                .unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["checkout", "-q", "-b", "autocommit-devtest-w1d2-alpha"]);
        std::fs::write(repo.join("a.py"), "x = 1\n").unwrap();
        std::fs::write(repo.join("b.py"), "y = 1\n").unwrap();
        git(&["add", "a.py", "b.py"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(repo.join("a.py"), "x = 2\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "n").unwrap();
        let head = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
        // b.py: same content, a new mtime — what makes a plain `git status` rewrite the index.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(repo.join("b.py"), "y = 1\n").unwrap();
        let index = std::fs::read(repo.join(".git/index")).unwrap();

        let opts = ProbeOpts { repo_path: Some(repo.display().to_string()), key_remote: Some("/nonexistent/key".into()), progress_cmd: None };
        let out = Command::new("sh")
            .arg("-c")
            .arg(probe_command(&opts, &repo.display().to_string()))
            .env("HOME", &home)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let r = parse_reply(&stdout).unwrap_or_else(|| panic!("no reply: {stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
        assert!(r.complete, "{stdout}");
        let x = r.extra.as_ref().unwrap();
        assert!(x.done, "{stdout}");
        let Some(Git::Repo(g)) = &x.git else { panic!("{:?}\n{stdout}", x.git) };
        assert_eq!(g.branch.as_deref(), Some("autocommit-devtest-w1d2-alpha"));
        // b.py, only touched, counts as modified: the plumbing doesn't refresh (or read) it.
        assert_eq!((g.modified, g.untracked, g.op.as_deref(), g.stashes), (Some(2), Some(1), Some("merge"), Some(0)), "{stdout}");
        assert_eq!(g.files, [" M a.py", " M b.py", "?? notes.txt"]);
        assert_eq!(g.last.as_ref().map(|(_, s)| s.as_str()), Some("first"));
        assert_eq!(g.upstream, None);
        assert!(x.df_root.is_some() && x.home_kb.is_some() && x.uptime.is_some() && x.nvidia_smi.is_some(), "{x:?}");
        assert_eq!(x.home.as_deref(), Some(home.display().to_string().as_str()));
        let ps = x.procs.as_ref().unwrap().as_ref().unwrap();
        assert!(!ps.is_empty() && ps.iter().all(|p| !p.args.contains("arena-ask-probe") && !p.args.contains("ps -eo")), "{ps:?}");
        let m = r.metrics.as_ref().unwrap();
        assert_eq!((m.branch.as_deref(), m.dirty_files, m.has_name, m.has_key), (Some("autocommit-devtest-w1d2-alpha"), Some(3), Some(false), Some(false)));
        assert!(matches!(&r.jobs, Some(Ok(j)) if j.is_empty()), "{:?}", r.jobs);
        assert!(matches!(&r.log, Some(Ok(ReadReply::NoJobs))), "{:?}", r.log);
        assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index, "the probe wrote the index");
        let text = render(&pod(), &Look::Probed(Box::new(r)), &Local::default(), &repo.display().to_string(), NOW, BUNDLE_CAP);
        assert!(text.contains("reached: yes\n") && text.contains("operation in progress: merge"), "{text}");
        // The backup's own `busy_op`, for real: BUSY, and how this merge (nothing unmerged) ends.
        assert!(
            text.contains("backup: BUSY — a merge is in progress on autocommit-devtest-w1d2-alpha, so the git backup commits")
                && text.contains("the participant ends it: finish it (`git commit`) or abort it (`git merge --abort`)"),
            "{text}"
        );
    }

    #[test]
    fn ages_read_compactly() {
        assert_eq!([age(5), age(600), age(11_520), age(1_036_800)], ["5s", "10m", "3h12m", "12d0h"]);
        let x = Extra { now: Some("2026-10-19T12:00:00Z".into()), ..Default::default() };
        assert_eq!(pod_now(Some(&x)), Some(NOW));
        assert_eq!(pod_now(Some(&Extra { now: Some("garbage".into()), ..Default::default() })), None);
    }
}
