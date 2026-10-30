//! Runs: the CLI commands the dashboard starts — `pods up` from the add-pod form, `pods start`,
//! `rename`, `jobs` and `logs` from the action menu, and the form's `offers` reads — as the
//! `arena` binary beside its own (`arena tui` launched it from there; canonicalised, so a
//! symlinked `arena-tui` finds the real one), **never one from `PATH`**: on a box with two
//! installs that could be another version, or another account's.
//!
//! Why the CLI as a child, not the same code in-process: the form gets exactly what `arena pods
//! up` does — placement with its fallbacks across GPU types and tiers, near the proxy, the
//! endpoint → proxy → setup → check → keys pipelines, `CREATE_EXTRA_JSON`, the reap policy, the
//! remembered prices — and every later change to `up` with it, with nothing copied here (the
//! form's old raw creates had fallen behind). The dashboard has asked in its own confirm, so the
//! child gets `--yes`; it gets the dashboard's `--config` and the form's `--provider`.
//!
//! A run writes its output — stdout and stderr together, as at a terminal — to a file of its own
//! under the state directory (`<state>/<prefix>/runs/`, the newest [`KEEP_LOGS`] kept), which the
//! dashboard reads as it grows ([`supervise`]) into the runs panel (`p`). Not a pipe: a run that
//! makes pods must be able to outlive the dashboard — once the reader of a pipe is gone, the next
//! line `up` prints kills it — and it runs in a process group of its own, so neither the
//! dashboard's end nor the terminal's hangup signals it. **Quitting while one is going asks
//! first** ([`Mode::Quit`]), and leaving it running is the default: stopping `up` half way leaves
//! pods created and billing but not set up, whereas letting it go finishes what was confirmed
//! (as `nohup arena pods up …` would) — its output stays in its file, which the quit says. A
//! hangup or a TERM ends the dashboard at once and leaves it too. `s` in the panel (or at the
//! quit) sends it Ctrl+C instead: `up`'s own — no new attempts, nothing terminated.
//! Read-only runs (`jobs`, `logs`, `offers`) end with the dashboard.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tokio::sync::Notify;

use crate::offers;
use crate::{centered_rect, scroll_by, Mode, Shared, Ui};

/// How many runs' output files are kept (the oldest go when a run starts) — besides the ones
/// still being written ([`prune`]). Each is what one command printed: `up`/`start`/`rename`'s
/// own progress lines, `jobs`' table, `logs --tail 100` (read from at most the last 1 MiB of
/// each pod's log) — small.
pub(crate) const KEEP_LOGS: usize = 30;



/// How many of a run's lines the panel keeps (its file has them all).
const KEEP_LINES: usize = 2_000;

/// The longest line the panel keeps, in bytes: a longer one is cut there (its file has it
/// whole). With [`KEEP_LINES`] it bounds a run's share of memory (~2 MB) whatever it prints.
const LINE_MAX: usize = 1_024;

/// The most of a run's file one look reads: a run that prints faster is read over several
/// looks, never all at once.
const READ_MAX: usize = 256 * 1024;

/// The most pods one run tracks the state of (a cohort is ~60): names past it are output only.
const MAX_PODS: usize = 500;

/// How many runs the dashboard remembers.
const KEEP_RUNS: usize = 20;

/// How often a run's file is read for new lines.
const TAIL_EVERY: Duration = Duration::from_millis(150);

/// How to run the CLI: the `arena` beside the dashboard's own executable, its config, and where
/// runs' output goes.
#[derive(Debug, Clone)]
pub(crate) struct Cli {
    pub exe: PathBuf,
    pub config: String,
    pub dir: PathBuf,
}

impl Cli {
    /// The `arena` beside `own` (this executable; canonicalised) — or why there's none — with
    /// the dashboard's `config` and runs' output under `state` (`<state dir>/runs`).
    pub(crate) fn locate(own: std::io::Result<PathBuf>, config: &str, state: Result<PathBuf, String>) -> Result<Cli, String> {
        let own = own.and_then(|p| p.canonicalize()).map_err(|e| format!("can't tell where arena-tui is ({e})"))?;
        let exe = own.parent().map(|d| d.join("arena")).ok_or_else(|| format!("{} has no directory", own.display()))?;
        if !exe.is_file() {
            return Err(format!(
                "no `arena` beside {} — the dashboard runs the CLI installed with it, never one from PATH",
                own.display()
            ));
        }
        let dir = state.map_err(|e| format!("no state directory for runs' output: {e}"))?.join("runs");
        Ok(Cli { exe, config: config.to_string(), dir })
    }

    /// `arena --config <config> --provider <provider> <args…>`, stdin from nowhere (a prompt
    /// refuses rather than waits).
    fn command(&self, provider: &str, args: &[String]) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.exe);
        c.arg("--config").arg(&self.config).arg("--provider").arg(provider).args(args).stdin(Stdio::null());
        c
    }
}

/// What a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Up,
    Start,
    Rename,
    Jobs,
    Logs,
}

impl Kind {
    fn word(&self) -> &'static str {
        match self {
            Kind::Up => "up",
            Kind::Start => "start",
            Kind::Rename => "rename",
            Kind::Jobs => "jobs",
            Kind::Logs => "logs",
        }
    }

    /// It changes pods: left running when the dashboard ends (asked first on a quit), never
    /// killed with it. The rest only read.
    pub(crate) fn lasting(&self) -> bool {
        matches!(self, Kind::Up | Kind::Start | Kind::Rename)
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum End {
    Code(i32),
    Signal(i32),
    /// It never started (the spawn failed, the output file couldn't be made).
    NotStarted(String),
}

impl End {
    fn ok(&self) -> bool {
        *self == End::Code(0)
    }

    fn label(&self) -> String {
        match self {
            End::Code(0) => "done".into(),
            End::Code(c) => format!("exit {c}"),
            End::Signal(s) => format!("killed (signal {s})"),
            End::NotStarted(why) => format!("didn't start: {why}"),
        }
    }
}

/// Where one pod of a run is, from its `[name] …` lines.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PodState {
    Created,
    /// A stage line (`setup ✓`, `proxy: port 9500 → …`, `check: pass`).
    Doing(String),
    /// `READY after 3m10s — …`
    Ready(String),
    /// `FAILED setup: …`
    Failed(String),
    /// `STOPPED at endpoint (Ctrl+C)`
    Stopped(String),
}

/// The CLI's bracketed tags that aren't a pod's name (`[proxy] after up: …`, `[keys] …`, a
/// lifecycle verb's `[terminated] NAME …`): never read as a pod (live: `[proxy]` showed as one).
const NOT_PODS: &[&str] = &[
    "big", "dry-run", "failed", "keys", "proxy", "reimaged", "rejected", "renamed", "restarted", "ssh-config", "started", "stop",
    "stopped", "terminated", "unlocked", "waiting",
];

/// A `pods up`/`start` line about one pod — `[created] NAME id=…`, `[NAME] READY after …`,
/// `[NAME] FAILED stage: why`, `[NAME] STOPPED at …`, any other `[NAME] stage …` — as the pod and
/// where it is. `None`: not about one pod. Pure.
pub(crate) fn pod_line(line: &str) -> Option<(String, PodState)> {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("[created] ") {
        let name = rest.split_whitespace().next()?;
        return Some((name.to_string(), PodState::Created));
    }
    let rest = line.strip_prefix('[')?;
    let (name, what) = rest.split_once("] ")?;
    if name.is_empty() || name.contains(' ') || NOT_PODS.contains(&name) {
        return None;
    }
    let what = what.trim();
    let state = if let Some(r) = what.strip_prefix("READY") {
        PodState::Ready(r.trim().to_string())
    } else if what.starts_with("FAILED") {
        PodState::Failed(what.to_string())
    } else if what.starts_with("STOPPED") {
        PodState::Stopped(what.to_string())
    } else {
        PodState::Doing(what.chars().take(80).collect())
    };
    Some((name.to_string(), state))
}

/// `up`'s line between two rounds while capacity is short: `round 2: 1 short of target 1
/// (capacity); retrying in 60s (Ctrl+C to stop)…` (one GPU option), `round 1: 2 name(s) not
/// placed (no capacity); retrying in 60s (Ctrl+C to stop)…` (several). Pure.
pub(crate) fn retrying_line(line: &str) -> bool {
    line.starts_with("round ") && line.contains("; retrying in ") && line.contains("(Ctrl+C to stop)")
}

/// `up`'s line when its retries are over: interrupted (`interrupted — stopping retries with 0 of
/// 1`), or the window spent (`retry window (30m) has no room for another round — …`). Pure.
fn retries_over(line: &str) -> bool {
    line.starts_with("interrupted — stopping retries") || line.starts_with("retry window (")
}

/// When an `up` with these arguments stops retrying, `started` (unix seconds) plus its
/// `--retry-mins`; `None` without one (or 0). Pure.
pub(crate) fn retry_until(args: &[String], started: u64) -> Option<u64> {
    let at = args.iter().position(|a| a == "--retry-mins")?;
    let mins: u64 = args.get(at + 1)?.parse().ok()?;
    (mins > 0).then_some(started + mins * 60)
}

/// `14:35 UTC` — a time today, as the dashboard says its times (UTC, the CLI's).
pub(crate) fn clock(unix: u64) -> String {
    let t = arena_core::snapshot::rfc3339(unix);
    format!("{} UTC", t.get(11..16).unwrap_or(&t))
}

/// One run, as the panel shows it.
#[derive(Debug)]
pub(crate) struct Run {
    pub id: u64,
    pub kind: Kind,
    pub title: String,
    /// The provider it was started with (`--provider`): a created pod's placeholder row is on it.
    pub provider: String,
    /// What runs, for the panel: `arena --provider hetzner pods up …`.
    pub command: String,
    pub log: PathBuf,
    pub pid: Option<u32>,
    pub started: Instant,
    pub lines: VecDeque<String>,
    /// Lines no longer kept (the file has them).
    pub dropped: usize,
    /// Each pod's last state, in the order they were first named — for a run that changes pods
    /// only ([`Kind::lasting`]), at most [`MAX_PODS`].
    pub pods: Vec<(String, PodState)>,
    /// Where each of `pods` is, by name.
    pub index: HashMap<String, usize>,
    pub end: Option<(Instant, End)>,
    /// An `up --retry-mins N` run: when its window ends (unix seconds) — from its arguments.
    pub retry_until: Option<u64>,
    /// It's waiting for capacity: its last retry line (`round 2: 1 short of target 1 (capacity);
    /// retrying in 60s (Ctrl+C to stop)…`) — until a pod is made or the retries end.
    pub waiting: Option<String>,
}

impl Run {
    /// Take one line of output; the pod it created, if it says so.
    pub(crate) fn take(&mut self, line: String) -> Option<String> {
        let mut created = None;
        // Only a command that changes pods reports their progress: `jobs`' and `logs`' lines are
        // the pods' own output, whatever they look like (review ai-review codex round 3 #1: a
        // log's `[created] x` made a pending row).
        if self.kind == Kind::Up {
            if retrying_line(&line) {
                self.waiting = Some(line.trim().to_string());
            } else if line.starts_with("[created] ") || retries_over(&line) {
                self.waiting = None;
            }
        }
        if let Some((name, state)) = self.kind.lasting().then(|| pod_line(&line)).flatten() {
            if state == PodState::Created && self.kind == Kind::Up {
                created = Some(name.clone());
            }
            match self.index.get(&name) {
                Some(&i) => {
                    let s = &mut self.pods[i].1;
                    // A finished pod's verdict stands against later chatter about it (the summary).
                    let done = matches!(s, PodState::Ready(_) | PodState::Failed(_) | PodState::Stopped(_));
                    if !(done && matches!(state, PodState::Doing(_))) {
                        *s = state;
                    }
                }
                None if self.pods.len() < MAX_PODS => {
                    self.index.insert(name.clone(), self.pods.len());
                    self.pods.push((name, state));
                }
                None => {}
            }
        }
        self.lines.push_back(line);
        while self.lines.len() > KEEP_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        created
    }

    pub(crate) fn running(&self) -> bool {
        self.end.is_none()
    }

    /// `2 ready, 1 failed` — the pods' outcomes, for the notice and the panel.
    pub(crate) fn tally(&self) -> String {
        let n = |f: fn(&PodState) -> bool| self.pods.iter().filter(|(_, s)| f(s)).count();
        let ready = n(|s| matches!(s, PodState::Ready(_)));
        let failed = n(|s| matches!(s, PodState::Failed(_)));
        let stopped = n(|s| matches!(s, PodState::Stopped(_)));
        let other = self.pods.len() - ready - failed - stopped;
        let mut parts = Vec::new();
        for (k, w) in [(ready, "ready"), (failed, "failed"), (stopped, "stopped"), (other, if self.running() { "under way" } else { "unfinished" })] {
            if k > 0 {
                parts.push(format!("{k} {w}"));
            }
        }
        parts.join(", ")
    }

    /// The footer's line when it ends: `✓ pods up: 1 ready`, `✗ pods up: exit 1 — 1 failed`.
    fn notice(&self) -> (String, bool) {
        let Some((_, end)) = &self.end else { return (String::new(), false) };
        let tally = self.tally();
        let what = format!("pods {}", self.kind.word());
        let text = match (end.ok(), tally.is_empty()) {
            (true, true) => format!("✓ {what} done"),
            (true, false) => format!("✓ {what}: {tally}"),
            (false, true) => format!("✗ {what}: {}", end.label()),
            (false, false) => format!("✗ {what}: {} — {tally}", end.label()),
        };
        (format!("{text} — [p] shows it"), !end.ok())
    }
}

/// Start `arena <args…>` on `provider` as a run (`title` in the panel): its output file made,
/// the child spawned in the background ([`supervise`]). The run's id — or why it can't start.
pub(crate) fn start(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, kind: Kind, provider: &str, args: Vec<String>, title: String) -> Result<u64, String> {
    let cli = ui.cli.as_ref().map_err(Clone::clone)?;
    if arena_core::remote::ending() {
        return Err("the dashboard is ending".into());
    }
    std::fs::create_dir_all(&cli.dir).map_err(|e| format!("making {}: {e}", cli.dir.display()))?;
    prune(&cli.dir, KEEP_LOGS.saturating_sub(1));
    let id = {
        let mut s = shared.lock().unwrap();
        s.next_run += 1;
        s.next_run
    };
    let log = cli.dir.join(format!("{}-{}-{id}.log", arena_core::snapshot::unix_now(), kind.word()));
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&log).map_err(|e| format!("making {}: {e}", log.display()))?
    };
    // A shared lock on the file, which the child's stdout and stderr share (one open file
    // description): held while anything of the run still has it open — the dashboard gone or
    // not — so [`prune`] can tell a file still being written.
    let _ = file.lock_shared();
    let mut cmd = cli.command(provider, &args);
    let err = file.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(file).stderr(err).process_group(0).kill_on_drop(!kind.lasting());
    // Spawned here, not in the background task: the run is listed with its pid, so a stop
    // reaches it at once, and a quit that leaves it running leaves a process that exists (it
    // was spawned in the task: a quit straight after `y` could end the dashboard before it was).
    let child = match spawn_retrying_busy(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log);
            return Err(format!("couldn't run {}: {e}", cli.exe.display()));
        }
    };
    drop(cmd);
    let quote = |a: &String| if a.contains(' ') || a.contains(',') { format!("'{a}'") } else { a.clone() };
    let command = format!("arena --provider {provider} {}", args.iter().map(quote).collect::<Vec<_>>().join(" "));
    {
        let mut s = shared.lock().unwrap();
        s.runs.push(Run {
            id,
            kind,
            title,
            provider: provider.to_string(),
            command,
            log: log.clone(),
            pid: child.id(),
            started: Instant::now(),
            lines: VecDeque::new(),
            dropped: 0,
            pods: Vec::new(),
            index: HashMap::new(),
            end: None,
            retry_until: (kind == Kind::Up).then(|| retry_until(&args, arena_core::snapshot::unix_now())).flatten(),
            waiting: None,
        });
        // Forget the oldest finished runs (their files stay).
        while s.runs.len() > KEEP_RUNS {
            match s.runs.iter().position(|r| !r.running()) {
                Some(i) => drop(s.runs.remove(i)),
                None => break,
            }
        }
    }
    tokio::spawn(supervise(shared.clone(), ui.nudge.clone(), id, child, log));
    Ok(id)
}

/// Remove all but the newest `keep` `*.log` files in `dir` (named by start time, so by name) —
/// never one still being written: a run's child (and what it starts) holds a shared `flock` on
/// its file for as long as it has it open ([`start`]), this dashboard's or an earlier one's,
/// so a file whose exclusive lock can't be had is kept. Unlinked, a run still writing would
/// lose its output, and the path the quit printed would be gone.
fn prune(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut logs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    logs.sort();
    let n = logs.len().saturating_sub(keep);
    for p in logs.into_iter().take(n) {
        let Ok(f) = std::fs::File::open(&p) else { continue };
        if f.try_lock().is_ok() {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// `cmd.spawn()` — a fork and an exec, no waiting on anything — again a few times while exec
/// says the file is busy (ETXTBSY: a test's stub just written, still open in a child another
/// thread forked — see the CLI's live_smoke).
fn spawn_retrying_busy(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 20 => {
                tries += 1;
                // Only ever a test's stub (an installed `arena` isn't being written): a brief
                // block, never on a real dashboard.
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Run `cmd` (run `id`), reading its output file `log` as it grows into the run — a created
/// pod shows at once as a pending row, and a refresh is nudged when a pod is made or finishes —
/// until it has exited and every line is read; then its end, and the footer says so.
async fn supervise(shared: Arc<Mutex<Shared>>, nudge: Arc<Notify>, id: u64, mut child: tokio::process::Child, log: PathBuf) {
    let mut reader = std::fs::File::open(&log).ok();
    let mut split = Lines::default();
    let mut status = None;
    loop {
        let (lines, read) = read_lines(reader.as_mut(), &mut split);
        if !lines.is_empty() {
            take_lines(&shared, &nudge, id, lines);
        }
        // Once it has exited, read on until the file has nothing more — counted in bytes, not
        // lines: a read in the middle of a long line completes none (review ai-review codex
        // round 2 #2: the verdicts after one were lost).
        if status.is_some() {
            if read > 0 {
                continue;
            }
            break;
        }
        if read == READ_MAX {
            // More is waiting: on with it, letting the other tasks in.
            tokio::task::yield_now().await;
            continue;
        }
        tokio::select! {
            s = child.wait() => status = Some(s),
            _ = tokio::time::sleep(TAIL_EVERY) => {}
        }
    }
    if let Some(last) = split.rest() {
        take_lines(&shared, &nudge, id, vec![last]);
    }
    let end = match status {
        Some(Ok(st)) => {
            use std::os::unix::process::ExitStatusExt;
            st.code().map_or_else(|| End::Signal(st.signal().unwrap_or(0)), End::Code)
        }
        Some(Err(e)) => End::NotStarted(format!("waiting for it: {e}")),
        None => End::NotStarted("lost".into()),
    };
    finish(&shared, id, end);
}

/// A run's output as lines, fed as it's read: a line ends at `\n`, `\r` or `\r\n` (a progress
/// bar redrawing itself is lines, not one that never ends), and only a line's first
/// [`LINE_MAX`] bytes are kept (then `…`; its file has it whole) — what waits for its end never
/// holds more, however long it runs.
#[derive(Debug, Default)]
pub(crate) struct Lines {
    partial: Vec<u8>,
    /// The line went past [`LINE_MAX`]: the rest of it is dropped.
    long: bool,
    /// The last byte was a `\r`: a `\n` right after it ends nothing more.
    after_cr: bool,
}

impl Lines {
    /// The lines `bytes` complete. Pure.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, b == b'\r');
            match b {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => out.push(self.take()),
                _ if self.partial.len() >= LINE_MAX => self.long = true,
                _ => self.partial.push(b),
            }
        }
        out
    }

    /// What's left after the run ended: its last line, if it had no end.
    pub(crate) fn rest(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> String {
        let mut line = cut(&String::from_utf8_lossy(&self.partial));
        if std::mem::take(&mut self.long) && !line.ends_with('…') {
            line.push('…');
        }
        self.partial.clear();
        line
    }
}

/// The lines read from `file` since the last call — at most [`READ_MAX`] bytes of it a call —
/// and how many bytes were read (0: nothing more there now).
fn read_lines(file: Option<&mut std::fs::File>, lines: &mut Lines) -> (Vec<String>, usize) {
    use std::io::Read;
    let Some(f) = file else { return (Vec::new(), 0) };
    let mut buf = vec![0u8; READ_MAX];
    match f.read(&mut buf) {
        Ok(n) if n > 0 => (lines.feed(&buf[..n]), n),
        _ => (Vec::new(), 0),
    }
}

/// `line`, cut at [`LINE_MAX`] bytes (on a character boundary) with `…` when longer — a lossy
/// decoding can be longer than the bytes it read.
fn cut(line: &str) -> String {
    if line.len() <= LINE_MAX {
        return line.to_string();
    }
    let mut end = LINE_MAX;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// Lines into run `id` ([`apply`]); a refresh nudged when they made or finished a pod.
fn take_lines(shared: &Mutex<Shared>, nudge: &Notify, id: u64, lines: Vec<String>) {
    let mut s = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if apply(&mut s, id, lines) {
        nudge.notify_one();
    }
}

/// Lines into run `id`: each pod a line says was created shows at once as a `pending` row (the
/// refresh drops it when the provider lists the real one). `true`: a pod was made or finished —
/// worth a refresh.
pub(crate) fn apply(s: &mut Shared, id: u64, lines: Vec<String>) -> bool {
    let Some(r) = s.runs.iter_mut().find(|r| r.id == id) else { return false };
    let provider = r.provider.clone();
    let mut created = Vec::new();
    let mut refresh = false;
    for line in lines {
        refresh |= r.kind.lasting() && matches!(pod_line(&line), Some((_, PodState::Ready(_) | PodState::Failed(_))));
        created.extend(r.take(line));
    }
    for name in created {
        refresh = true;
        if !s.pods.iter().any(|p| p.name == name) && !s.pending.iter().any(|p| p.name == name) {
            s.pending.push(arena_core::Pod { id: format!("pending-{name}"), name, provider: provider.clone(), status: "pending".into(), ..Default::default() });
        }
    }
    refresh
}

fn finish(shared: &Mutex<Shared>, id: u64, end: End) {
    let mut s = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(r) = s.runs.iter_mut().find(|r| r.id == id) else { return };
    r.end = Some((Instant::now(), end));
    let (text, bad) = r.notice();
    s.notify(text, bad);
}

/// Run `arena offers …` for `q` in the background; its answer lands in `Shared::offers_landed`
/// (the loop hands it to the form's [`offers::State`]). No CLI to run (a demo, no `arena` beside
/// the dashboard): the answer is why, at once.
pub(crate) fn fetch_offers(shared: &Arc<Mutex<Shared>>, cli: &Result<Cli, String>, q: offers::Query, state: &mut offers::State, now: Instant) {
    let cli = match cli {
        Ok(c) => c,
        Err(why) => return state.landed(q, Err(why.clone()), now),
    };
    let (provider, args) = offers::args(&q);
    let cmd = cli.command(&provider, &args);
    let shared = shared.clone();
    tokio::spawn(async move {
        let out = capture(cmd, offers::TIMEOUT).await;
        shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).offers_landed.push((q, out));
    });
}

/// Run a read-only `cmd` to its end (killed after `timeout`): its stdout when that's JSON, else
/// why not — its last line on stderr (`Error: ` dropped), or its exit.
pub(crate) async fn capture(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = spawn_retrying_busy(&mut cmd).map_err(|e| format!("couldn't run arena: {e}"))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => return Err(format!("no answer within {}s", timeout.as_secs())),
        Ok(Err(e)) => return Err(e.to_string()),
        Ok(Ok(out)) => out,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if stdout.trim_start().starts_with('{') {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| l.strip_prefix("Error: ").unwrap_or(l).to_string());
    Err(why.unwrap_or_else(|| format!("arena offers: {}", out.status)))
}

/// What asking a run to stop came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stop {
    /// Ctrl+C went to its process group.
    Sent,
    /// It has ended (or there's no such run).
    NotRunning,
    /// `kill` refused or couldn't run: why.
    Failed(String),
}

/// Send run `id` Ctrl+C — SIGINT to its process group, as a terminal's Ctrl+C would (`up`: no new
/// attempts, nothing terminated). A run is listed with its pid ([`start`] spawns it first).
fn interrupt(shared: &Mutex<Shared>, id: u64) -> Stop {
    let pid = {
        let s = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pid) = s.runs.iter().find(|r| r.id == id && r.running()).and_then(|r| r.pid) else { return Stop::NotRunning };
        pid
    };
    match sigint_group(pid) {
        Ok(()) => Stop::Sent,
        Err(e) => Stop::Failed(e),
    }
}

/// SIGINT to process group `pid` (`kill -INT -- -pid`).
fn sigint_group(pid: u32) -> Result<(), String> {
    let out = std::process::Command::new("kill")
        .args(["-INT", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't run kill: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if why.is_empty() { format!("kill: {}", out.status) } else { why })
}

/// The runs still going that change pods — what a quit asks about.
pub(crate) fn lasting_running(s: &Shared) -> Vec<(u64, String, Option<u32>, PathBuf)> {
    s.runs.iter().filter(|r| r.running() && r.kind.lasting()).map(|r| (r.id, r.command.clone(), r.pid, r.log.clone())).collect()
}

/// `q` (or Ctrl+C): quit now, or — with a run that changes pods still going — the question
/// ([`Mode::Quit`]). `true`: quit now.
pub(crate) fn quit_or_ask(ui: &mut Ui, shared: &Mutex<Shared>) -> bool {
    if lasting_running(&shared.lock().unwrap()).is_empty() {
        return true;
    }
    ui.mode = Mode::Quit;
    false
}

/// What a key in the runs panel or the quit question leads to.
pub(crate) enum Flow {
    NotOurs,
    Handled,
    /// Quit, saying these lines once the terminal is back.
    Quit(Vec<String>),
}

/// The lines said after a quit that left `left` running (`stops`: each one's [`interrupt`],
/// when the quit sent Ctrl+C) — what really happened to each, never "sent" for one that wasn't.
fn left_running(left: &[(u64, String, Option<u32>, PathBuf)], stops: Option<&[Stop]>) -> Vec<String> {
    left.iter()
        .enumerate()
        .map(|(i, (_, cmd, pid, log))| {
            let pid = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            let log = log.display();
            match stops.map(|s| s.get(i).cloned().unwrap_or(Stop::NotRunning)) {
                None => format!("arena-tui: left running: `{cmd}`{pid} — its output: {log}"),
                Some(Stop::Sent) => format!("arena-tui: sent Ctrl+C to `{cmd}`{pid}: it stops where it is, terminates nothing — its output: {log}"),
                Some(Stop::NotRunning) => format!("arena-tui: `{cmd}` had already ended — its output: {log}"),
                Some(Stop::Failed(why)) => format!("arena-tui: ✗ couldn't send Ctrl+C to `{cmd}`{pid} ({why}): it's still running — its output: {log}"),
            }
        })
        .collect()
}

/// The runs panel's keys (`Mode::Runs`), the quit question's (`Mode::Quit`), and `p` on the
/// list or detail view, which opens the newest run.
pub(crate) fn key(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, code: KeyCode) -> Flow {
    match ui.mode.clone() {
        Mode::List | Mode::Detail if code == KeyCode::Char('p') => {
            let newest = shared.lock().unwrap().runs.last().map(|r| r.id);
            match newest {
                Some(id) => ui.mode = Mode::Runs { id, scroll: None, stop: false },
                None => shared.lock().unwrap().notify("no runs yet — [n] makes pods, [a] has start/rename/jobs/logs", false),
            }
            Flow::Handled
        }
        Mode::Runs { id, scroll, stop } => {
            let ids: Vec<u64> = shared.lock().unwrap().runs.iter().map(|r| r.id).collect();
            let at = ids.iter().position(|&i| i == id).unwrap_or(0);
            let max = ui.result_max.get();
            let mut next = Mode::Runs { id, scroll, stop: false };
            match code {
                KeyCode::Char('y') if stop => {
                    let (text, bad) = match interrupt(shared, id) {
                        Stop::Sent => ("sent Ctrl+C to the run: it stops where it is, terminates nothing".to_string(), false),
                        Stop::NotRunning => ("✗ the run isn't running".to_string(), true),
                        Stop::Failed(why) => (format!("✗ couldn't send Ctrl+C to the run: {why}"), true),
                    };
                    shared.lock().unwrap().notify(text, bad);
                }
                KeyCode::Esc | KeyCode::Char('q') => next = Mode::List,
                KeyCode::Left | KeyCode::Char('h') if at > 0 => next = Mode::Runs { id: ids[at - 1], scroll: None, stop: false },
                KeyCode::Right | KeyCode::Char('l') if at + 1 < ids.len() => next = Mode::Runs { id: ids[at + 1], scroll: None, stop: false },
                KeyCode::Up | KeyCode::Char('k') => next = Mode::Runs { id, scroll: Some(scroll_by(scroll.unwrap_or(max), max, -1)), stop: false },
                KeyCode::Down | KeyCode::Char('j') => next = Mode::Runs { id, scroll: follow(scroll_by(scroll.unwrap_or(max), max, 1), max), stop: false },
                KeyCode::PageUp => next = Mode::Runs { id, scroll: Some(scroll_by(scroll.unwrap_or(max), max, -10)), stop: false },
                KeyCode::PageDown => next = Mode::Runs { id, scroll: follow(scroll_by(scroll.unwrap_or(max), max, 10), max), stop: false },
                KeyCode::End | KeyCode::Char('G') => next = Mode::Runs { id, scroll: None, stop: false },
                KeyCode::Home | KeyCode::Char('g') => next = Mode::Runs { id, scroll: Some(0), stop: false },
                KeyCode::Char('s') => {
                    let can = shared.lock().unwrap().runs.iter().any(|r| r.id == id && r.running() && r.kind.lasting());
                    next = Mode::Runs { id, scroll, stop: can };
                }
                _ => {}
            }
            ui.mode = next;
            Flow::Handled
        }
        Mode::Quit => {
            let left = lasting_running(&shared.lock().unwrap());
            match code {
                KeyCode::Esc => {
                    ui.mode = Mode::List;
                    Flow::Handled
                }
                KeyCode::Char('s') => {
                    let stops: Vec<Stop> = left.iter().map(|(id, ..)| interrupt(shared, *id)).collect();
                    Flow::Quit(left_running(&left, Some(&stops)))
                }
                KeyCode::Char('k' | 'q') | KeyCode::Enter => Flow::Quit(left_running(&left, None)),
                _ => Flow::Handled,
            }
        }
        _ => Flow::NotOurs,
    }
}

/// A scroll at the end follows the output again.
fn follow(scroll: u16, max: u16) -> Option<u16> {
    (scroll < max).then_some(scroll)
}

/// `1m12s`, `45s`.
fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// The footer's lead while runs are going: `running: pods up 1m12s (1 under way) · `.
pub(crate) fn footer(s: &Shared) -> Option<String> {
    let going: Vec<String> = s
        .runs
        .iter()
        .filter(|r| r.running())
        .map(|r| {
            let tally = r.tally();
            let mut notes: Vec<String> = (!tally.is_empty()).then_some(tally).into_iter().collect();
            if let (Some(_), Some(until)) = (&r.waiting, r.retry_until) {
                notes.push(format!("waiting for capacity until {}", clock(until)));
            }
            let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(", ")) };
            format!("pods {} {}{notes}", r.kind.word(), elapsed(r.started.elapsed()))
        })
        .collect();
    (!going.is_empty()).then(|| format!("running: {} · [p] progress · ", going.join(", ")))
}

/// The runs panel: run `id`'s title and state, its command and output file, each pod's state,
/// then its output — following the end unless scrolled (`scroll`).
pub(crate) fn render(f: &mut Frame, s: &Shared, id: u64, scroll: Option<u16>, stop: bool, max: &std::cell::Cell<u16>) {
    let area = centered_rect(90, 85, f.area());
    f.render_widget(Clear, area);
    let Some(run) = s.runs.iter().find(|r| r.id == id) else {
        f.render_widget(Paragraph::new("this run is gone").block(Block::default().borders(Borders::ALL)), area);
        return;
    };
    let at = s.runs.iter().position(|r| r.id == id).unwrap_or(0) + 1;
    let dim = Style::default().fg(Color::DarkGray);
    let (state, color) = match &run.end {
        None if run.waiting.is_some() => (format!("running {} · waiting for capacity", elapsed(run.started.elapsed())), Color::Yellow),
        None => (format!("running {}", elapsed(run.started.elapsed())), Color::Cyan),
        Some((t, e)) => (format!("{} after {}", e.label(), elapsed(t.duration_since(run.started))), if e.ok() { Color::Green } else { Color::Red }),
    };
    let mut head: Vec<Line> = vec![
        Line::styled(format!("$ {}", run.command), Style::default().fg(Color::Cyan)),
        Line::styled(format!("output: {}", run.log.display()), dim),
    ];
    // A retrying `up`: until when, and how to stop it sooner; while it waits, its last round.
    if let (true, Some(until)) = (run.running(), run.retry_until) {
        let left = until.saturating_sub(arena_core::snapshot::unix_now());
        head.push(Line::styled(
            format!("retry: no capacity → tries again until {} (in {}) · [s] stops it, keeping what was made", clock(until), elapsed(Duration::from_secs(left))),
            Style::default().fg(Color::Yellow),
        ));
        if let Some(w) = &run.waiting {
            head.push(Line::styled(format!("  waiting for capacity — {w}"), Style::default().fg(Color::Yellow)));
        }
    }
    let inner = area.height.saturating_sub(2) as usize;
    // The pods get at most a third of the panel (review ai-review codex round 3 #2: a cohort's
    // statuses pushed the output and the stop question off it): the failed, stopped and
    // unfinished first, then the ready, and a line for the rest.
    let pod_room = (inner / 3).min(inner.saturating_sub(6));
    let mut shown: Vec<&(String, PodState)> = run.pods.iter().collect();
    let more = shown.len().saturating_sub(pod_room);
    if more > 0 {
        shown.sort_by_key(|(_, st)| match st {
            PodState::Failed(_) => 0,
            PodState::Stopped(_) => 1,
            PodState::Doing(_) | PodState::Created => 2,
            PodState::Ready(_) => 3,
        });
        shown.truncate(pod_room.saturating_sub(1));
    }
    let pods_at = head.len();
    for (name, st) in shown {
        let (text, style) = match st {
            PodState::Created => ("created".to_string(), Style::default()),
            PodState::Doing(w) => (format!("… {w}"), Style::default()),
            PodState::Ready(w) => (format!("✓ READY {w}"), Style::default().fg(Color::Green)),
            PodState::Failed(w) => (format!("✗ {w}"), Style::default().fg(Color::Red)),
            PodState::Stopped(w) => (format!("⊘ {w}"), Style::default().fg(Color::Yellow)),
        };
        head.push(Line::from(vec![Span::styled(format!("  {name:<20} "), Style::default().add_modifier(Modifier::BOLD)), Span::styled(text, style)]));
    }
    if more > 0 && pod_room > 0 {
        let hidden = run.pods.len() - (head.len() - pods_at);
        head.push(Line::styled(format!("  … and {hidden} more · in all: {}", run.tally()), dim));
    }
    let keys = if stop {
        Line::styled("send Ctrl+C to it (no new attempts, nothing terminated)?  [y] yes  [any other key] no", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        let stop = if run.running() && run.kind.lasting() { "  [s] stop" } else { "" };
        Line::styled(format!("[esc] hide  [←→] run {at} of {}  [↑↓ PgUp PgDn] scroll  [end] follow{stop}", s.runs.len()), dim)
    };
    let room = inner.saturating_sub(head.len() + 2).max(1);
    // Long lines wrap (an error's point is often at its end); the scroll is in screen rows.
    let rows = wrap_rows(run.lines.iter().map(String::as_str), area.width.saturating_sub(2) as usize);
    let total = rows.len();
    let max_scroll = total.saturating_sub(room).min(u16::MAX as usize) as u16;
    max.set(max_scroll);
    let from = scroll.map_or(max_scroll, |s| s.min(max_scroll)) as usize;
    let mut lines = head;
    lines.push(Line::styled("─".repeat(area.width.saturating_sub(2) as usize), dim));
    if total == 0 {
        lines.push(Line::styled(if run.running() { "(no output yet)" } else { "(no output)" }, dim));
    }
    lines.extend(rows.into_iter().skip(from).take(room).map(Line::raw));
    let used = lines.len();
    lines.extend(std::iter::repeat_n(Line::raw(""), inner.saturating_sub(used + 1)));
    lines.push(keys);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title(format!(" {} — {state} ", run.title)),
        ),
        area,
    );
}

/// `lines` as screen rows `width` characters wide: each line cut into as many rows as it
/// needs (an empty line is one row). Pure.
fn wrap_rows<'a>(lines: impl Iterator<Item = &'a str>, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push(String::new());
        }
        rows.extend(chars.chunks(width).map(|c| c.iter().collect::<String>()));
    }
    rows
}

/// The quit question: which runs that change pods are still going, and what each key does.
pub(crate) fn render_quit(f: &mut Frame, s: &Shared) {
    let left = lasting_running(s);
    let mut lines = vec![Line::styled(format!("{} run(s) that change pods still going:", left.len()), Style::default().add_modifier(Modifier::BOLD)), Line::from("")];
    for (_, cmd, pid, _) in &left {
        lines.push(Line::from(format!("  {cmd}{}", pid.map(|p| format!("  (pid {p})")).unwrap_or_default())));
    }
    lines.extend([
        Line::from(""),
        Line::from("[k] quit, and leave it running (default) — it finishes what was confirmed; its output stays in its file"),
        Line::from("[s] quit, and send it Ctrl+C — it stops where it is (pods made so far are left as they are, billing)"),
        Line::from("[esc] stay"),
    ]);
    let area = centered_rect(80, 40, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" quit? ")),
        area,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `pods up`'s per-pod lines, as the panel reads them; anything else isn't about a pod.
    #[test]
    fn up_lines_say_where_each_pod_is() {
        let cases: [(&str, Option<(&str, PodState)>); 12] = [
            ("[created] devtest-alpha id=12345 on cx33 (€0.0160/h)", Some(("devtest-alpha", PodState::Created))),
            ("[devtest-alpha] proxy: port 9500 → 1.2.3.4:22", Some(("devtest-alpha", PodState::Doing("proxy: port 9500 → 1.2.3.4:22".into())))),
            ("[devtest-alpha] setup ✓ (3m02s)", Some(("devtest-alpha", PodState::Doing("setup ✓ (3m02s)".into())))),
            ("[devtest-alpha] READY after 4m10s — proxy :9500", Some(("devtest-alpha", PodState::Ready("after 4m10s — proxy :9500".into())))),
            ("[devtest-bravo] FAILED setup: timed out", Some(("devtest-bravo", PodState::Failed("FAILED setup: timed out".into())))),
            ("[devtest-bravo] STOPPED at endpoint (Ctrl+C) — left as it is", Some(("devtest-bravo", PodState::Stopped("STOPPED at endpoint (Ctrl+C) — left as it is".into())))),
            ("[dry-run] would create devtest-alpha", None),
            // Live: `up`'s closing proxy sync read as a pod named "proxy".
            ("[proxy] after up: +0 ~0 -0 =0 (unchanged)", None),
            ("[keys] devtest-alpha: no key in ./keys/", None),
            ("Will create 1 pod(s) on hetzner", None),
            ("[two words] x", None),
            ("", None),
        ];
        for (line, want) in cases {
            assert_eq!(pod_line(line), want.map(|(n, s)| (n.to_string(), s)), "{line:?}");
        }
    }

    pub(crate) fn run(id: u64, kind: Kind) -> Run {
        Run {
            id,
            kind,
            title: "t".into(),
            provider: "hetzner".into(),
            command: "arena --provider hetzner pods up --add 1 --yes".into(),
            log: PathBuf::from("/tmp/x.log"),
            pid: None,
            started: Instant::now(),
            lines: VecDeque::new(),
            dropped: 0,
            pods: Vec::new(),
            index: HashMap::new(),
            end: None,
            retry_until: None,
            waiting: None,
        }
    }

    /// A retrying `up` (`--retry-mins`): its window's end read off its arguments; its round
    /// lines — one GPU option's and placement's — say it's waiting for capacity, until a pod is
    /// made or its retries end (Ctrl+C, the window spent); the footer says until when.
    #[test]
    fn a_retrying_up_says_it_waits_for_capacity_and_until_when() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(retry_until(&args(&["pods", "up", "--add", "1", "--retry-mins", "30", "--yes"]), 1_000), Some(1_000 + 1_800));
        assert_eq!(retry_until(&args(&["pods", "up", "--add", "1", "--yes"]), 1_000), None);
        assert_eq!(retry_until(&args(&["pods", "up", "--retry-mins", "0"]), 1_000), None);
        assert_eq!(clock(1_791_460_800 + 35 * 60), "12:35 UTC");
        for (line, waiting) in [
            ("round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…", true),
            ("round 1: 2 name(s) not placed (no capacity); retrying in 60s (Ctrl+C to stop)…", true),
            ("[devtest-echo] endpoint poll failed (x); retrying in 5s", false),
            ("round 3: nothing created — vast didn't answer", false),
        ] {
            assert_eq!(retrying_line(line), waiting, "{line}");
        }
        let mut r = run(1, Kind::Up);
        r.retry_until = Some(arena_core::snapshot::unix_now() + 1_800);
        r.take("[stop] no capacity for this GPU right now (created 0/1).".into());
        r.take("round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…".into());
        assert!(r.waiting.is_some());
        let shared = Shared { runs: vec![r], ..Default::default() };
        let f = footer(&shared).unwrap();
        assert!(f.starts_with("running: pods up 0s (waiting for capacity until ") && f.ends_with(" UTC) · [p] progress · "), "{f}");
        let mut r = shared.runs.into_iter().next().unwrap();
        r.take("[created] devtest-echo id=9 on 1×RTX A4000 SECURE ($0.25/h)".into());
        assert_eq!(r.waiting, None, "made: not waiting any more");
        r.take("round 2: 1 short of target 2 (capacity); retrying in 60s (Ctrl+C to stop)…".into());
        r.take("interrupted — stopping retries with 1 of 2".into());
        assert_eq!(r.waiting, None, "stopped");
        // Only `up` retries: a start's lines are its own.
        let mut start = run(2, Kind::Start);
        start.take("round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…".into());
        assert_eq!(start.waiting, None);
    }

    /// A created pod shows at once as a pending row on the run's provider (once), and its
    /// creation and its verdict ask for a refresh; a verdict stands against later chatter; the
    /// tally and the closing notice count the outcomes.
    #[test]
    fn a_runs_lines_make_pending_rows_and_a_tally() {
        let mut s = Shared::default();
        s.runs.push(run(1, Kind::Up));
        let l = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(!apply(&mut s, 1, l(&["Will create 2 pod(s)…"])));
        assert!(apply(&mut s, 1, l(&["[created] devtest-alpha id=1 on cx33", "[created] devtest-bravo id=2 on cx33", "[created] devtest-alpha id=1"])));
        assert_eq!(s.pending.iter().map(|p| (p.name.as_str(), p.provider.as_str(), p.status.as_str())).collect::<Vec<_>>(), [("devtest-alpha", "hetzner", "pending"), ("devtest-bravo", "hetzner", "pending")]);
        assert!(!apply(&mut s, 1, l(&["[devtest-alpha] setup ✓ (3m)"])));
        assert!(apply(&mut s, 1, l(&["[devtest-alpha] READY after 4m — ok", "[devtest-bravo] FAILED setup: timed out"])));
        apply(&mut s, 1, l(&["[devtest-alpha] fleet SSH map updated"]));
        let r = &s.runs[0];
        assert_eq!(r.pods[0].1, PodState::Ready("after 4m — ok".into()), "the verdict stands");
        assert_eq!(r.tally(), "1 ready, 1 failed");
        assert_eq!(r.lines.len(), 8);
        finish(&Mutex::new(std::mem::take(&mut s)), 1, End::Code(1));
        assert!(!apply(&mut Shared::default(), 9, l(&["[created] x id=1"])), "no such run");
    }

    /// The runner never takes `arena` from PATH: only the one beside the dashboard's own
    /// executable (canonicalised — a symlinked arena-tui finds the real one's sibling).
    #[test]
    fn the_cli_is_the_one_beside_the_dashboard() {
        let dir = std::env::temp_dir().join(format!("arena-tui-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join("links")).unwrap();
        std::fs::write(dir.join("real/arena-tui"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("real/arena-tui"), dir.join("links/arena-tui")).unwrap();
        let state = || Ok(dir.join("state"));
        let missing = Cli::locate(Ok(dir.join("links/arena-tui")), "/c.env", state()).unwrap_err();
        assert!(missing.contains("no `arena` beside") && missing.contains("never one from PATH"), "{missing}");
        std::fs::write(dir.join("real/arena"), "").unwrap();
        let cli = Cli::locate(Ok(dir.join("links/arena-tui")), "/c.env", state()).unwrap();
        assert_eq!(cli.exe, dir.join("real/arena").canonicalize().unwrap());
        assert_eq!((cli.config.as_str(), cli.dir.clone()), ("/c.env", dir.join("state/runs")));
        assert!(Cli::locate(Ok(dir.join("links/arena-tui")), "/c.env", Err("no HOME".into())).unwrap_err().contains("no HOME"));
        assert!(Cli::locate(Err(std::io::Error::other("x")), "/c.env", state()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cohort-sized run keeps its output, its tally and its keys (the stop question too) on
    /// screen: the pods get a third of the panel, failures first, the rest summed up (review
    /// ai-review codex round 3 #2). And only a run that changes pods reads its lines as pod
    /// progress, at most MAX_PODS of them; only `up`'s `[created]` makes a pending row (#1).
    #[test]
    fn a_cohort_sized_run_keeps_its_output_and_keys_on_screen() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut s = Shared::default();
        let mut r = run(1, Kind::Up);
        for i in 0..30 {
            r.take(format!("[created] devtest-p{i:02} id={i}"));
        }
        for i in 0..25 {
            r.take(format!("[devtest-p{i:02}] READY after 3m — CPU VM"));
        }
        r.take("[devtest-p27] FAILED setup: boom".into());
        r.take("the last line of output".into());
        s.runs.push(r);
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let max = std::cell::Cell::new(0);
        t.draw(|f| render(f, &s, 1, None, true, &max)).unwrap();
        let buf = t.backend().buffer();
        let screen: String = (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect();
        assert!(screen.contains("send Ctrl+C to it"), "{screen}");
        assert!(screen.contains("the last line of output"), "{screen}");
        assert!(screen.contains("devtest-p27") && screen.contains("FAILED setup: boom"), "{screen}");
        assert!(screen.contains("more · in all: 25 ready, 1 failed, 4 under way"), "{screen}");
        assert!(!screen.contains("devtest-p24 "), "a ready one past the room is summed up: {screen}");

        // jobs/logs: their lines are the pods' output, never progress or pending rows.
        let mut s = Shared::default();
        s.runs.push(run(2, Kind::Logs));
        assert!(!apply(&mut s, 2, vec!["[created] devtest-zz id=1".into(), "[devtest-zz] READY after 1s".into()]));
        assert!(s.runs[0].pods.is_empty() && s.pending.is_empty());
        // `start`: progress, but no pending rows; and no more than MAX_PODS names.
        let mut s = Shared::default();
        s.runs.push(run(3, Kind::Start));
        let lines: Vec<String> = (0..MAX_PODS + 50).map(|i| format!("[step-{i}] x")).collect();
        apply(&mut s, 3, lines);
        apply(&mut s, 3, vec!["[created] devtest-q id=1".into()]);
        assert_eq!((s.runs[0].pods.len(), s.pending.len()), (MAX_PODS, 0));
    }

    /// The panel wraps a long line into rows (the live 403's "shared core limit" was past the
    /// edge); an empty line is a row.
    #[test]
    fn long_output_lines_wrap_into_rows() {
        let rows = wrap_rows(["abcdefg", "", "hi", "ééé"].into_iter(), 3);
        assert_eq!(rows, ["abc", "def", "g", "", "hi", "ééé"]);
        assert_eq!(wrap_rows(["ab"].into_iter(), 0), ["a", "b"], "a zero width is one column");
    }

    /// Only the newest `keep` output files stay — but never one a run is still writing: its
    /// child holds a shared lock on it (review ai-review codex #4, round 2 #5: a long `up`'s file
    /// went once enough jobs/logs runs came after it; by modification time alone, an earlier
    /// dashboard's quiet run could lose its file).
    #[test]
    fn old_run_files_are_pruned_but_never_one_being_written() {
        let dir = std::env::temp_dir().join(format!("arena-tui-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for t in 100..106 {
            std::fs::write(dir.join(format!("{t}-up-1.log")), "").unwrap();
        }
        std::fs::write(dir.join("keep.txt"), "").unwrap();
        // A run still writing 101 (its child's descriptor, locked as `start` locks it).
        let writing = std::fs::OpenOptions::new().append(true).open(dir.join("101-up-1.log")).unwrap();
        writing.lock_shared().unwrap();
        prune(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["101-up-1.log", "104-up-1.log", "105-up-1.log", "keep.txt"], "101's run is going");
        drop(writing);
        prune(&dir, 2);
        assert!(!dir.join("101-up-1.log").exists(), "its run ended: it goes too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run's output as the panel keeps it: lines end at `\n`, `\r` or `\r\n`, across reads;
    /// a line is kept to its first LINE_MAX bytes then `…` — what waits for a line's end never
    /// grows past that (review ai-review codex #1: a log printed without newlines grew it
    /// without bound); the last line without an end is the rest.
    #[test]
    fn output_splits_into_bounded_lines() {
        let mut l = Lines::default();
        assert_eq!(l.feed(b"one\ntwo\r\nthr"), ["one", "two"]);
        assert_eq!(l.feed(b"ee\r"), ["three"]);
        assert_eq!(l.feed(b"\n10%\r20%\r"), ["10%", "20%"], "the \\n after a \\r ends nothing; \\r alone ends a line");
        let long = vec![b'x'; LINE_MAX * 50];
        assert!(l.feed(&long).is_empty());
        assert!(l.partial.len() <= LINE_MAX, "bounded while the line goes on");
        let got = l.feed(b"tail\nnext");
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].len(), got[0].ends_with("x…")), (LINE_MAX + '…'.len_utf8(), true));
        assert_eq!(l.rest().as_deref(), Some("next"));
        assert_eq!(l.rest(), None);
        assert_eq!(cut(&"é".repeat(LINE_MAX)).chars().filter(|c| *c == 'é').count(), LINE_MAX / 2, "cut on a character boundary");
    }
}
