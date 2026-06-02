//! `arena-tui` — the interactive dashboard.
//!
//! It lists pods from the configured provider and, for each pod with an SSH endpoint,
//! fetches GPU stats (`nvidia-smi`) and an optional operator-defined progress signal
//! (`PROGRESS_CMD`) over SSH. Fetching runs in a **background task** so the UI never
//! blocks: the loop redraws and handles keys continuously while fresh data lands as
//! soon as each sweep finishes. Move the cursor with `↑/↓`/`j/k`, open a per-pod detail
//! pane (per-GPU breakdown + util/temp sparklines) with `Enter`, act on the selected
//! pod with `a` (restart / stop / terminate / backup / setup), `f` cycles the refresh
//! cadence, `r` refreshes now.
//!
//! Safety against live prod is built into the *interaction*, not bolted on: a mutating
//! action always pops a confirmation modal. Lifecycle actions (restart/stop/terminate)
//! make you type the pod's exact name back before they apply; backup/setup show the
//! precise command(s) that will run and take a single `y`. There is no way to mutate a
//! pod without going through that modal — the dashboard's reads stay reads.
//!
//! Provider is chosen by `ARENA_PROVIDER` (default `runpod`); config path by
//! `ARENA_CONFIG`; initial cadence by `ARENA_REFRESH_SECS` (default 5).

mod prefs;
mod state;

use std::collections::HashMap;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use arena_core::metrics::{self, PodMetrics, ProbeOpts};
use arena_core::provider::Provider;
use arena_core::ssh::{self, SshTarget};
use arena_core::naming;
use arena_core::{Config, Pod, PodSpec};

use prefs::Prefs;
use state::{display_name, short_branch, summarize, Action, Confirm, FleetSummary, History};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";
/// Shorter than backup/setup's 10s: a down pod shouldn't stall a whole metrics sweep.
const METRICS_CONNECT_TIMEOUT: u32 = 4;
/// The cadences `f` cycles through (seconds).
const REFRESH_STEPS: &[u64] = &[2, 5, 10, 20, 60];

/// What the UI is currently showing. `List`/`Detail` are the normal views; the rest
/// are modal (the background fetch keeps running, but a modal can't be acted on by a
/// stray refresh — it only reads shared data).
#[derive(Debug, Clone)]
enum Mode {
    List,
    Detail,
    /// Action chooser for the selected pod.
    Menu,
    /// A pending action awaiting confirmation.
    Confirm(Confirm),
    /// Fleet action chooser (safe ops only: restart / backup / setup).
    FleetMenu,
    /// A pending fleet action; requires typing `ALL` to confirm.
    FleetConfirm { action: Action, typed: String },
    /// Add-pod form: `free` machine names available, `count` pods to make, plus the
    /// chosen GPU type (index into `gpu_types`) and GPUs-per-pod.
    NewPod {
        free: Vec<String>,
        count: usize,
        gpu_types: Vec<String>,
        gpu_idx: usize,
        gpu_count: u32,
    },
    /// The outcome of the last action; any key dismisses (then we nudge a refresh).
    Result(String),
}

/// Live fleet data, written by the background fetch task and read by the UI thread.
/// Guarded by a std `Mutex` held only for brief, synchronous critical sections — never
/// across an `.await`, so the fetcher and the UI never deadlock.
#[derive(Default)]
struct Shared {
    pods: Vec<Pod>,
    metrics: HashMap<String, PodMetrics>,
    history: HashMap<String, History>,
    summary: FleetSummary,
    status: String,
    last_refresh: Option<Instant>,
    /// True while a sweep is in flight (drives the footer's ⟳ indicator).
    refreshing: bool,
}

/// UI-thread-only state: what the operator is looking at / interacting with. Kept
/// separate from `Shared` so key handling never contends with the fetcher.
struct Ui {
    provider_name: String,
    config_path: String,
    cfg: Config,
    /// `MACHINE_NAME_PREFIX`, for shortening names/branches.
    prefix: String,
    /// Show short pod names (`apple`) instead of full (`arena8-apple`). Persisted.
    short_names: bool,
    mode: Mode,
    /// Cursor into `Shared::pods` (clamped to a valid row each frame).
    selected: usize,
}

impl Ui {
    /// The pod's name as currently displayed (short or full).
    fn shown_name(&self, full: &str) -> String {
        display_name(full, &self.prefix, self.short_names)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path =
        std::env::var("ARENA_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string());
    let cfg = Config::load(&PathBuf::from(&config_path))
        .with_context(|| format!("loading config {config_path}"))?;
    let provider_name = std::env::var("ARENA_PROVIDER").unwrap_or_else(|_| "runpod".to_string());
    let provider: Arc<dyn Provider> =
        Arc::from(arena_core::provider::build(&provider_name, &cfg)?);
    let progress_cmd = cfg.get("PROGRESS_CMD").map(String::from);
    let initial_secs = std::env::var("ARENA_REFRESH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let shared = Arc::new(Mutex::new(Shared {
        status: "loading…".into(),
        ..Default::default()
    }));
    let interval = Arc::new(AtomicU64::new(initial_secs));
    let nudge = Arc::new(Notify::new());

    // Background fetcher: keeps `shared` fresh without blocking the UI.
    tokio::spawn(fetch_loop(
        provider.clone(),
        cfg.clone(),
        progress_cmd,
        shared.clone(),
        interval.clone(),
        nudge.clone(),
    ));

    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let ui = Ui {
        provider_name,
        config_path,
        prefix,
        short_names: Prefs::load().short_names,
        cfg,
        mode: Mode::List,
        selected: 0,
    };

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, &shared, &provider, &interval, &nudge, ui).await;
    restore_terminal(&mut terminal)?;
    res
}

/// The background refresh loop: list pods, fetch their metrics concurrently, publish to
/// `shared`, then wait for the cadence to elapse *or* a manual nudge (`r`/`f`/action),
/// whichever comes first.
async fn fetch_loop(
    provider: Arc<dyn Provider>,
    cfg: Config,
    progress_cmd: Option<String>,
    shared: Arc<Mutex<Shared>>,
    interval: Arc<AtomicU64>,
    nudge: Arc<Notify>,
) {
    // The per-pod probe gathers GPU stats + branch + setup health in one SSH call.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0"))
    });
    let opts = ProbeOpts {
        progress_cmd,
        repo_path: Some(repo_path),
        key_remote: Some(cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string()),
    };

    loop {
        shared.lock().unwrap().refreshing = true;

        match provider.list_pods().await {
            Err(e) => {
                // Keep the last known pods on screen; just report the error.
                let mut s = shared.lock().unwrap();
                s.status = format!("list error: {e}");
                s.refreshing = false;
            }
            Ok(mut pods) => {
                pods.sort_by(|a, b| a.name.cmp(&b.name));
                let metrics = fetch_metrics(&pods, &cfg, &opts).await;
                let summary = summarize(&pods, &metrics);
                let status = status_line(&summary);

                let mut s = shared.lock().unwrap();
                for pod in &pods {
                    let m = metrics.get(&pod.name);
                    s.history
                        .entry(pod.name.clone())
                        .or_default()
                        .push(m.and_then(|m| m.mean_util()), m.and_then(|m| m.max_temp()));
                }
                s.summary = summary;
                s.status = status;
                s.metrics = metrics;
                s.pods = pods;
                s.last_refresh = Some(Instant::now());
                s.refreshing = false;
            }
        }

        let secs = interval.load(Ordering::Relaxed).max(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = nudge.notified() => {}
        }
    }
}

/// Fan metric fetches out across the fleet concurrently, with a short connect timeout
/// so a single unreachable pod can't hold up the sweep.
async fn fetch_metrics(
    pods: &[Pod],
    cfg: &Config,
    opts: &ProbeOpts,
) -> HashMap<String, PodMetrics> {
    let mut set = JoinSet::new();
    for pod in pods {
        if let Ok(mut target) = SshTarget::from_pod(pod, cfg) {
            target.connect_timeout_secs = METRICS_CONNECT_TIMEOUT;
            let name = pod.name.clone();
            let opts = opts.clone();
            set.spawn(async move { (name, metrics::fetch(&target, &opts).await) });
        }
    }
    let mut fresh = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((name, m)) = joined {
            fresh.insert(name, m);
        }
    }
    fresh
}

fn status_line(s: &FleetSummary) -> String {
    format!(
        "{} pods · {} reporting{}",
        s.pods,
        s.reporting,
        if s.unreachable > 0 {
            format!(" · {} unreachable", s.unreachable)
        } else {
            String::new()
        }
    )
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Read the currently selected pod (a clone, so we don't hold the lock).
fn selected_pod(shared: &Arc<Mutex<Shared>>, idx: usize) -> Option<Pod> {
    shared.lock().unwrap().pods.get(idx).cloned()
}

/// Advance the cadence to the next step in `REFRESH_STEPS` (wrapping).
fn cycle_interval(interval: &AtomicU64) {
    let cur = interval.load(Ordering::Relaxed);
    let idx = REFRESH_STEPS.iter().position(|&s| s == cur).unwrap_or(usize::MAX);
    let next = REFRESH_STEPS[idx.wrapping_add(1) % REFRESH_STEPS.len()];
    interval.store(next, Ordering::Relaxed);
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    provider: &Arc<dyn Provider>,
    interval: &AtomicU64,
    nudge: &Notify,
    mut ui: Ui,
) -> Result<()> {
    loop {
        // Clamp the cursor in case the fleet shrank under us.
        let len = shared.lock().unwrap().pods.len();
        ui.selected = if len == 0 { 0 } else { ui.selected.min(len - 1) };

        let secs = interval.load(Ordering::Relaxed);
        {
            let s = shared.lock().unwrap();
            terminal.draw(|f| view(f, &s, &ui, secs))?;
        }

        // Short poll: the loop keeps redrawing (~7 fps) so live data and the
        // "updated Ns ago" line stay current without any key press.
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue; // ignore key-release/repeat on terminals that emit them
        }
        // Ctrl+C always quits, from any mode. Raw mode swallows the usual SIGINT, so
        // we handle the keystroke ourselves and let `restore_terminal` clean up.
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            break;
        }
        let code = k.code;

        match ui.mode.clone() {
            Mode::List | Mode::Detail => {
                let in_detail = matches!(ui.mode, Mode::Detail);
                match code {
                    KeyCode::Char('q') => break,
                    KeyCode::Esc => {
                        if in_detail {
                            ui.mode = Mode::List;
                        } else {
                            break;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui.selected = ui.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if ui.selected + 1 < len {
                            ui.selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if len > 0 {
                            ui.mode = Mode::Detail;
                        }
                    }
                    KeyCode::Char('r') => nudge.notify_one(),
                    KeyCode::Char('s') => {
                        // Toggle short/full names and persist the choice.
                        ui.short_names = !ui.short_names;
                        Prefs { short_names: ui.short_names }.save();
                    }
                    KeyCode::Char('f') => {
                        cycle_interval(interval);
                        nudge.notify_one(); // apply a shorter cadence immediately
                    }
                    KeyCode::Char('a') => {
                        if len > 0 {
                            ui.mode = Mode::Menu;
                        }
                    }
                    KeyCode::Char('A') => {
                        if len > 0 {
                            ui.mode = Mode::FleetMenu;
                        }
                    }
                    KeyCode::Char('n') => {
                        // Fresh list (not the cached snapshot) so we never allocate a
                        // name that already exists and create a duplicate.
                        match provider.list_pods().await {
                            Ok(existing) => {
                                let prefix = ui.cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
                                let free = naming::next_free_names(
                                    prefix,
                                    &ui.cfg.machine_names,
                                    &existing,
                                    usize::MAX,
                                );
                                let gpu_count = PodSpec::from_config(&ui.cfg).gpu_count.max(1);
                                ui.mode = Mode::NewPod {
                                    free,
                                    count: 1,
                                    gpu_types: gpu_type_choices(&ui.cfg),
                                    gpu_idx: 0,
                                    gpu_count,
                                };
                            }
                            Err(e) => ui.mode = Mode::Result(format!("✗ list failed: {e}")),
                        }
                    }
                    _ => {}
                }
            }
            Mode::Menu => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Char(ch) => {
                    if let (Some(action), Some(pod)) =
                        (Action::from_key(ch), selected_pod(shared, ui.selected))
                    {
                        let preview = build_preview(&ui.cfg, action, &pod);
                        let shown = ui.shown_name(&pod.name);
                        ui.mode = Mode::Confirm(Confirm::new(action, shown, pod.id, preview));
                    }
                }
                _ => {}
            },
            Mode::Confirm(mut c) => {
                let typed = c.action.requires_typed_name();
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Enter if c.is_satisfied() => {
                        apply_action(terminal, shared, &mut ui, provider, secs, &c).await?;
                        nudge.notify_one();
                    }
                    KeyCode::Char('y') if !typed => {
                        apply_action(terminal, shared, &mut ui, provider, secs, &c).await?;
                        nudge.notify_one();
                    }
                    KeyCode::Char('n') if !typed => ui.mode = Mode::List,
                    KeyCode::Backspace if typed => {
                        c.typed.pop();
                        ui.mode = Mode::Confirm(c);
                    }
                    KeyCode::Char(ch) if typed => {
                        c.typed.push(ch);
                        ui.mode = Mode::Confirm(c);
                    }
                    _ => ui.mode = Mode::Confirm(c),
                }
            }
            Mode::FleetMenu => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                // Safe ops only — stop/terminate are deliberately not fleet-wide.
                KeyCode::Char(ch @ ('r' | 'b' | 'p')) => {
                    let action = match ch {
                        'r' => Action::Restart,
                        'b' => Action::Backup,
                        _ => Action::Setup,
                    };
                    ui.mode = Mode::FleetConfirm { action, typed: String::new() };
                }
                _ => {}
            },
            Mode::FleetConfirm { action, mut typed } => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if typed == "ALL" => {
                    let pods = shared.lock().unwrap().pods.clone();
                    ui.mode = Mode::Result(format!(
                        "running {} on all {} pods…",
                        action.label(),
                        pods.len()
                    ));
                    {
                        let s = shared.lock().unwrap();
                        terminal.draw(|f| view(f, &s, &ui, secs))?;
                    }
                    let msg = execute_fleet(provider, &ui.cfg, action, pods).await;
                    ui.mode = Mode::Result(msg);
                    nudge.notify_one();
                }
                KeyCode::Backspace => {
                    typed.pop();
                    ui.mode = Mode::FleetConfirm { action, typed };
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    ui.mode = Mode::FleetConfirm { action, typed };
                }
                _ => ui.mode = Mode::FleetConfirm { action, typed },
            },
            Mode::NewPod { free, count, gpu_types, gpu_idx, gpu_count } => {
                let n_types = gpu_types.len().max(1);
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Enter => {
                        let names: Vec<String> = free.iter().take(count).cloned().collect();
                        if names.is_empty() {
                            ui.mode = Mode::Result("✗ no free machine names available".into());
                        } else {
                            let gpu_type = gpu_types.get(gpu_idx).cloned().unwrap_or_default();
                            ui.mode = Mode::Result(format!(
                                "creating {} × {}gpu {} pod(s)…",
                                names.len(),
                                gpu_count,
                                gpu_type
                            ));
                            {
                                let s = shared.lock().unwrap();
                                terminal.draw(|f| view(f, &s, &ui, secs))?;
                            }
                            let msg =
                                create_pods(provider, &ui.cfg, &names, &gpu_type, gpu_count).await;
                            ui.mode = Mode::Result(msg);
                            nudge.notify_one();
                        }
                    }
                    other => {
                        // Adjust one of the three knobs and rebuild the form state.
                        let (count, gpu_idx, gpu_count) = match other {
                            KeyCode::Up | KeyCode::Char('k') => {
                                ((count + 1).min(free.len().max(1)), gpu_idx, gpu_count)
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                (count.saturating_sub(1).max(1), gpu_idx, gpu_count)
                            }
                            KeyCode::Left | KeyCode::Char('h') => {
                                (count, (gpu_idx + n_types - 1) % n_types, gpu_count)
                            }
                            KeyCode::Right | KeyCode::Char('l') => {
                                (count, (gpu_idx + 1) % n_types, gpu_count)
                            }
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                (count, gpu_idx, (gpu_count + 1).min(8))
                            }
                            KeyCode::Char('-') => (count, gpu_idx, gpu_count.saturating_sub(1).max(1)),
                            _ => (count, gpu_idx, gpu_count),
                        };
                        ui.mode = Mode::NewPod { free, count, gpu_types, gpu_idx, gpu_count };
                    }
                }
            }
            Mode::Result(_) => {
                // Any key dismisses the result, then nudge a refresh to re-sync.
                ui.mode = Mode::List;
                nudge.notify_one();
            }
        }
    }
    Ok(())
}

/// Run a safe action against every pod concurrently, returning a summary line plus the
/// first few failures. Used by the fleet menu (restart / backup / setup only).
async fn execute_fleet(
    provider: &Arc<dyn Provider>,
    cfg: &Config,
    action: Action,
    pods: Vec<Pod>,
) -> String {
    let total = pods.len();
    let mut set = JoinSet::new();
    for pod in pods {
        let provider = provider.clone();
        let cfg = cfg.clone();
        set.spawn(async move { execute(provider.as_ref(), &cfg, action, &pod).await });
    }
    let mut ok = 0usize;
    let mut fails: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(msg) = joined {
            if msg.starts_with('✓') {
                ok += 1;
            } else {
                fails.push(msg);
            }
        }
    }
    let mut s = format!("fleet {}: {ok}/{total} ok", action.label());
    if !fails.is_empty() {
        s.push_str(&format!(", {} failed:", fails.len()));
        for f in fails.iter().take(8) {
            s.push('\n');
            s.push_str(f);
        }
        if fails.len() > 8 {
            s.push_str(&format!("\n… +{} more", fails.len() - 8));
        }
    }
    s
}

/// Common GPU types offered in the add-pod form, after the config's own default. These
/// are best-effort presets (provider-specific strings); the configured default is
/// known-good and listed first.
const GPU_PRESETS: &[&str] = &[
    "NVIDIA GeForce RTX 4090",
    "NVIDIA GeForce RTX 3090",
    "NVIDIA RTX A4000",
    "NVIDIA RTX A5000",
    "NVIDIA RTX A6000",
    "NVIDIA A100 80GB PCIe",
    "NVIDIA H100 80GB HBM3",
    "NVIDIA L40S",
];

/// Build the GPU-type choices for the add-pod form: the config default first (so the
/// default create matches the CLI), then the presets, de-duplicated.
fn gpu_type_choices(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let default = PodSpec::from_config(cfg).gpu_type;
    if !default.is_empty() {
        out.push(default);
    }
    for t in GPU_PRESETS {
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        out.push("NVIDIA GeForce RTX 4090".to_string());
    }
    out
}

/// Create the named pods (sequentially, no capacity-wait), returning a summary line.
/// Each create is gated by the add-pod form's Enter, mirroring `arena pods create`.
async fn create_pods(
    provider: &Arc<dyn Provider>,
    cfg: &Config,
    names: &[String],
    gpu_type: &str,
    gpu_count: u32,
) -> String {
    let mut base = PodSpec::from_config(cfg);
    base.gpu_type = gpu_type.to_string();
    base.gpu_count = gpu_count;
    let mut ok = 0usize;
    let mut fails: Vec<String> = Vec::new();
    for name in names {
        let mut spec = base.clone();
        spec.name = name.clone();
        spec.env.push(("MACHINE_NAME".into(), name.clone()));
        match provider.create_pod(&spec).await {
            Ok(_) => ok += 1,
            Err(e) => fails.push(format!("✗ {name}: {e}")),
        }
    }
    let mut s = format!("created {ok}/{}", names.len());
    for f in fails.iter().take(8) {
        s.push('\n');
        s.push_str(f);
    }
    s
}

/// Run a confirmed action against its pod, show a busy line, then park the outcome in a
/// result modal. The pod is looked up fresh by id (it may have moved in the list).
async fn apply_action(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    ui: &mut Ui,
    provider: &Arc<dyn Provider>,
    secs: u64,
    c: &Confirm,
) -> Result<()> {
    let Some(pod) = shared.lock().unwrap().pods.iter().find(|p| p.id == c.pod_id).cloned() else {
        ui.mode = Mode::Result(format!("{} is gone — refresh", c.pod_name));
        return Ok(());
    };
    ui.mode = Mode::Result(format!("{}ing {}…", c.action.label(), pod.name));
    {
        let s = shared.lock().unwrap();
        terminal.draw(|f| view(f, &s, ui, secs))?;
    }

    let msg = execute(provider.as_ref(), &ui.cfg, c.action, &pod).await;
    ui.mode = Mode::Result(msg);
    Ok(())
}

/// Perform one action against a pod, returning a one-line human-readable outcome.
/// This is the *only* place the dashboard mutates anything.
async fn execute(provider: &dyn Provider, cfg: &Config, action: Action, pod: &Pod) -> String {
    let name = &pod.name;
    match action {
        Action::Restart => match provider.restart_pod(&pod.id).await {
            Ok(()) => format!("✓ restarted {name}"),
            Err(e) => format!("✗ restart {name} failed: {e}"),
        },
        Action::Stop => match provider.stop_pod(&pod.id).await {
            Ok(()) => format!("✓ stopped {name}"),
            Err(e) => format!("✗ stop {name} failed: {e}"),
        },
        Action::Terminate => match provider.terminate_pod(&pod.id).await {
            Ok(()) => format!("✓ terminated {name}"),
            Err(e) => format!("✗ terminate {name} failed: {e}"),
        },
        Action::Backup => run_backup(cfg, pod).await,
        Action::Setup => run_setup(cfg, pod).await,
    }
}

/// Commit + push the pod's ARENA tree to its autocommit branch over SSH (mirrors
/// `arena backup --apply` for a single pod).
async fn run_backup(cfg: &Config, pod: &Pod) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let (week, day) = match resolve_week_day(cfg) {
        Ok(wd) => wd,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
    let msg = format!("arena-tui backup {}", pod.name);
    let cmd = arena_core::backup::backup_command(&bcfg, &pod.name, &msg);
    match ssh::run(&target, &cmd).await {
        Ok(out) if out.success => {
            if out.stdout.contains("NO_CHANGES") {
                format!("✓ {} — no changes", pod.name)
            } else {
                format!("✓ backed up {} → {}", pod.name, bcfg.branch_for(&pod.name))
            }
        }
        Ok(out) => format!("✗ backup {} (exit {:?}): {}", pod.name, out.code, out.stderr.trim()),
        Err(e) => format!("✗ backup {} failed: {e}", pod.name),
    }
}

/// Provision the pod over SSH: copy the deploy key, then run the setup script (mirrors
/// `arena setup --apply` for a single pod; non-force, matching the CLI default).
async fn run_setup(cfg: &Config, pod: &Pod) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let scfg = match arena_core::setup::SetupConfig::from_config(cfg) {
        Ok(s) => s,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match ssh::scp(&target, &scfg.key_local, &scfg.key_remote).await {
        Ok(out) if out.success => {}
        Ok(out) => return format!("✗ setup {} (scp key): {}", pod.name, out.stderr.trim()),
        Err(e) => return format!("✗ setup {} (scp key): {e}", pod.name),
    }
    match ssh::run(&target, &scfg.remote_command(&pod.name, false)).await {
        Ok(out) if out.success => format!("✓ set up {}", pod.name),
        Ok(out) => format!("✗ setup {} (exit {:?}): {}", pod.name, out.code, out.stderr.trim()),
        Err(e) => format!("✗ setup {} failed: {e}", pod.name),
    }
}

/// The exact command(s) a safe (non-typed) action will run, for the confirm modal.
/// Lifecycle actions return `None` (their modal shows the typed-name prompt instead).
fn build_preview(cfg: &Config, action: Action, pod: &Pod) -> Option<String> {
    let target = SshTarget::from_pod(pod, cfg);
    match action {
        Action::Backup => Some(match (&target, resolve_week_day(cfg)) {
            (Ok(t), Ok((week, day))) => {
                let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
                let msg = format!("arena-tui backup {}", pod.name);
                t.display_command(&arena_core::backup::backup_command(&bcfg, &pod.name, &msg))
            }
            (Err(e), _) => format!("⚠ {e}"),
            (_, Err(e)) => format!("⚠ {e}"),
        }),
        Action::Setup => Some(match (&target, arena_core::setup::SetupConfig::from_config(cfg)) {
            (Ok(t), Ok(scfg)) => format!(
                "{}\n{}",
                t.display_scp(&scfg.key_local, &scfg.key_remote),
                t.display_command(&scfg.remote_command(&pod.name, false))
            ),
            (Err(e), _) => format!("⚠ {e}"),
            (_, Err(e)) => format!("⚠ {e}"),
        }),
        _ => None,
    }
}

/// Resolve the iteration (week, day) from `ARENA_START_DATE` (the start date is w0d1).
/// Mirrors the CLI's resolver; the TUI has no `--week/--day` overrides.
fn resolve_week_day(cfg: &Config) -> anyhow::Result<(u32, u32)> {
    let start = cfg
        .get("ARENA_START_DATE")
        .and_then(arena_core::schedule::parse_ymd)
        .context("backup needs ARENA_START_DATE=YYYY-MM-DD in config")?;
    let start_days = arena_core::schedule::days_from_civil(start.0, start.1, start.2);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today = arena_core::schedule::days_from_unix(now_secs);
    Ok(arena_core::schedule::week_day(start_days, today))
}

fn temp_style(t: Option<u32>) -> Style {
    match t {
        Some(t) if t >= 85 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(t) if t >= 75 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn util_style(u: Option<u32>, err: bool) -> Style {
    if err {
        return Style::default().fg(Color::Red);
    }
    match u {
        Some(0) => Style::default().fg(Color::DarkGray), // idle GPU — worth noticing
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

/// Truncate a string to `max` display columns, adding an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A one-letter, color-coded provider badge: R=runpod, V=vast, H=hetzner.
fn provider_cell(provider: &str) -> Cell<'static> {
    let (letter, color) = match provider {
        "runpod" => ("R", Color::Cyan),
        "vast" => ("V", Color::Magenta),
        "hetzner" => ("H", Color::Yellow),
        other => (other.get(0..1).unwrap_or("?"), Color::Gray),
    };
    Cell::from(Span::styled(
        letter.to_uppercase(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

/// The compact setup-health cell: three glyphs for `~/.name`, the deploy key, and the
/// git origin pointing at GitHub. ✓ green / ✗ red / · gray (unknown or unreachable).
fn health_cell(m: Option<&PodMetrics>) -> Cell<'static> {
    let glyph = |ok: Option<bool>| match ok {
        Some(true) => Span::styled("✓", Style::default().fg(Color::Green)),
        Some(false) => Span::styled("✗", Style::default().fg(Color::Red)),
        None => Span::styled("·", Style::default().fg(Color::DarkGray)),
    };
    match m {
        Some(m) if m.error.is_none() => {
            let origin_ok = m.origin.as_deref().map(|o| o.contains("github.com"));
            Cell::from(Line::from(vec![glyph(m.has_name), glyph(m.has_key), glyph(origin_ok)]))
        }
        _ => Cell::from(Span::styled("···", Style::default().fg(Color::DarkGray))),
    }
}

/// The right-hand detail cell: progress if we have it, else the (truncated) fetch
/// error, else "-".
fn detail_cell(m: Option<&PodMetrics>) -> (String, Style) {
    match m {
        Some(m) if m.progress.is_some() => (m.progress.clone().unwrap(), Style::default()),
        Some(m) if m.error.is_some() => {
            let mut e = m.error.clone().unwrap();
            if e.len() > 48 {
                e.truncate(47);
                e.push('…');
            }
            (e, Style::default().fg(Color::Red).add_modifier(Modifier::DIM))
        }
        _ => ("-".into(), Style::default().fg(Color::DarkGray)),
    }
}

fn view(f: &mut Frame, shared: &Shared, ui: &Ui, secs: u64) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Length(1), // fleet summary
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer / hints
        ])
        .split(f.area());

    let title = Paragraph::new(format!(
        "arena-infra-rs dashboard · provider: {} · {}",
        ui.provider_name, ui.config_path
    ))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    f.render_widget(summary_line(&shared.summary), chunks[1]);

    if matches!(ui.mode, Mode::Detail) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[2]);
        pods_table(f, shared, ui, cols[0]);
        detail_pane(f, shared, ui, cols[1]);
    } else {
        pods_table(f, shared, ui, chunks[2]);
    }

    f.render_widget(Paragraph::new(footer_hint(shared, ui, secs)), chunks[3]);

    match &ui.mode {
        Mode::Menu => render_menu(f, shared, ui),
        Mode::Confirm(c) => render_confirm(f, c),
        Mode::FleetMenu => render_fleet_menu(f, shared),
        Mode::FleetConfirm { action, typed } => render_fleet_confirm(f, shared, *action, typed),
        Mode::NewPod { free, count, gpu_types, gpu_idx, gpu_count } => {
            render_new_pod(f, free, *count, gpu_types, *gpu_idx, *gpu_count)
        }
        Mode::Result(msg) => render_result(f, msg),
        _ => {}
    }
}

fn summary_line(s: &FleetSummary) -> Paragraph<'static> {
    let mem = if s.mem_total_mb > 0 {
        format!(
            "{:.0}/{:.0}G",
            s.mem_used_mb as f64 / 1024.0,
            s.mem_total_mb as f64 / 1024.0
        )
    } else {
        "-".into()
    };
    let util = s.mean_util.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into());
    let unreachable = if s.unreachable > 0 {
        format!("  ·  {} unreachable", s.unreachable)
    } else {
        String::new()
    };
    Paragraph::new(format!(
        " fleet: {} pods · {} GPUs · mean util {} · mem {} · ${:.2}/hr (${:.0}/day){}",
        s.pods,
        s.total_gpus,
        util,
        mem,
        s.total_cost,
        s.total_cost * 24.0,
        unreachable
    ))
    .style(Style::default().add_modifier(Modifier::BOLD))
}

fn pods_table(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let header = Row::new(vec![
        "P", "NAME", "STATUS", "SET", "GPU", "GPU%", "MEM", "TEMP", "$/HR", "BRANCH",
        "PROGRESS / ERROR",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = shared
        .pods
        .iter()
        .map(|p| {
            let m = shared.metrics.get(&p.name);
            let util = m.and_then(|m| m.mean_util());
            let err = m.map(|m| m.error.is_some()).unwrap_or(false);
            let util_str = match (util, err) {
                (Some(u), _) => format!("{u}%"),
                (None, true) => "err".into(),
                (None, false) => "-".into(),
            };
            let mem = match m.and_then(|m| m.mem_summary()) {
                Some((u, t)) => format!("{:.0}/{:.0}G", u as f64 / 1024.0, t as f64 / 1024.0),
                None => "-".into(),
            };
            let temp = m.and_then(|m| m.max_temp());
            let temp_str = temp.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into());
            let cost = p.cost_per_hr.map(|c| format!("${c:.2}")).unwrap_or_else(|| "-".into());
            // GPU now comes from the live nvidia-smi readout (provider list omits it).
            let gpu = m
                .and_then(|m| m.gpu_summary())
                .or_else(|| p.gpu_type.clone())
                .unwrap_or_else(|| "-".into());
            let branch = match m.and_then(|m| m.branch.clone()) {
                Some(b) => truncate(&short_branch(&b, &ui.prefix), 18),
                None => "-".into(),
            };
            let (detail, detail_style) = detail_cell(m);
            Row::new(vec![
                provider_cell(&p.provider),
                Cell::from(ui.shown_name(&p.name)),
                Cell::from(p.status.clone()),
                health_cell(m),
                Cell::from(gpu),
                Cell::from(util_str).style(util_style(util, err)),
                Cell::from(mem),
                Cell::from(temp_str).style(temp_style(temp)),
                Cell::from(cost),
                Cell::from(branch),
                Cell::from(detail).style(detail_style),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),  // P (provider glyph)
        Constraint::Length(16), // NAME
        Constraint::Length(8),  // STATUS
        Constraint::Length(3),  // SET (✓✓✓)
        Constraint::Length(13), // GPU (e.g. "2×RTX A4000")
        Constraint::Length(5),  // GPU%
        Constraint::Length(8),  // MEM (e.g. "120/240G")
        Constraint::Length(4),  // TEMP (e.g. "85C")
        Constraint::Length(7),  // $/HR
        Constraint::Length(18), // BRANCH
        Constraint::Min(10),    // PROGRESS / ERROR
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .block(Block::default().borders(Borders::ALL).title("Pods"));

    let mut ts = TableState::default();
    if !shared.pods.is_empty() {
        ts.select(Some(ui.selected.min(shared.pods.len() - 1)));
    }
    f.render_stateful_widget(table, area, &mut ts);
}

/// The per-pod detail pane (shown in Detail mode): identity + endpoint, a per-GPU
/// table, full progress text, and util/temp sparklines from the rolling history.
fn detail_pane(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let Some(pod) = shared.pods.get(ui.selected) else { return };
    let m = shared.metrics.get(&pod.name);

    let block =
        Block::default().borders(Borders::ALL).title(format!(" {} ", ui.shown_name(&pod.name)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // header facts
            Constraint::Min(3),    // per-GPU table
            Constraint::Length(3), // util sparkline
            Constraint::Length(3), // temp sparkline
        ])
        .split(inner);

    let endpoint = match (&pod.ssh_ip, pod.ssh_port) {
        (Some(ip), Some(port)) => format!("{ip}:{port}"),
        _ => "(no SSH endpoint yet)".into(),
    };
    let progress = m
        .and_then(|m| m.progress.clone())
        .or_else(|| m.and_then(|m| m.error.clone()))
        .unwrap_or_else(|| "-".into());
    let gpu = m
        .and_then(|m| m.gpu_summary())
        .or_else(|| pod.gpu_type.clone())
        .unwrap_or_else(|| "-".into());
    let cost = pod
        .cost_per_hr
        .map(|c| format!("${c:.2}/hr  (${:.2}/day)", c * 24.0))
        .unwrap_or_else(|| "-".into());
    let ok = |b: Option<bool>| match b {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "·",
    };
    let origin = m.and_then(|m| m.origin.clone()).unwrap_or_else(|| "-".into());
    let origin_ok = m.and_then(|m| m.origin.as_deref().map(|o| o.contains("github.com")));
    let facts = format!(
        "status:   {}\ngpu:      {}\nendpoint: {}\ncost:     {}\nbranch:   {}\norigin:   {} {}\nsetup:    .name {}   key {}   origin→gh {}\nprogress: {}",
        pod.status,
        gpu,
        endpoint,
        cost,
        m.and_then(|m| m.branch.clone()).unwrap_or_else(|| "-".into()),
        truncate(&origin, 32),
        ok(origin_ok),
        ok(m.and_then(|m| m.has_name)),
        ok(m.and_then(|m| m.has_key)),
        ok(origin_ok),
        progress,
    );
    f.render_widget(Paragraph::new(facts).wrap(Wrap { trim: true }), rows[0]);

    let gpu_rows: Vec<Row> = m
        .map(|m| {
            m.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let mem = match (g.mem_used_mb, g.mem_total_mb) {
                        (Some(u), Some(t)) => {
                            format!("{:.1}/{:.1}G", u as f64 / 1024.0, t as f64 / 1024.0)
                        }
                        _ => "-".into(),
                    };
                    Row::new(vec![
                        Cell::from(format!("{i}")),
                        Cell::from(g.util_pct.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into()))
                            .style(util_style(g.util_pct, false)),
                        Cell::from(mem),
                        Cell::from(g.temp_c.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into()))
                            .style(temp_style(g.temp_c)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let gpu_table = Table::new(
        gpu_rows,
        [
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(13),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec!["GPU", "UTIL", "MEM", "TEMP"]).style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::TOP).title("per-GPU"));
    f.render_widget(gpu_table, rows[1]);

    let hist = shared.history.get(&pod.name);
    let util_data = hist.map(|h| h.util_data()).unwrap_or_default();
    let temp_data = hist.map(|h| h.temp_data()).unwrap_or_default();
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::TOP).title("util % (history)"))
            .data(&util_data)
            .max(100)
            .style(Style::default().fg(Color::Green)),
        rows[2],
    );
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::TOP).title("temp C (history)"))
            .data(&temp_data)
            .max(100)
            .style(Style::default().fg(Color::Yellow)),
        rows[3],
    );
}

fn footer_hint(shared: &Shared, ui: &Ui, secs: u64) -> String {
    let age = match shared.last_refresh {
        Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
        None => "never updated".into(),
    };
    let spin = if shared.refreshing { " ⟳" } else { "" };
    let keys = match ui.mode {
        Mode::List => "[enter] detail  [a] act  [A] fleet  [n] new  [s] names  [f] interval  [r] now  [q] quit",
        Mode::Detail => "[a] act  [A] fleet  [n] new  [s] names  [f] interval  [r] now  [esc] back  [q] quit",
        Mode::Menu => "[r/s/t/b/p] choose action  [esc] cancel",
        Mode::Confirm(_) => "type to confirm  [enter] apply  [esc] cancel",
        Mode::FleetMenu => "[r/b/p] choose fleet action  [esc] cancel",
        Mode::FleetConfirm { .. } => "type ALL to confirm  [enter] apply  [esc] cancel",
        Mode::NewPod { .. } => "[↑↓] pods  [+-] gpus  [←→] type  [enter] create  [esc] cancel",
        Mode::Result(_) => "[any key] dismiss",
    };
    format!("{}{} · {} · every {}s · {}", shared.status, spin, age, secs, keys)
}

/// A centered popup rect `pct_x` × `pct_y` percent of the screen.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

fn render_menu(f: &mut Frame, shared: &Shared, ui: &Ui) {
    let name = shared
        .pods
        .get(ui.selected)
        .map(|p| ui.shown_name(&p.name))
        .unwrap_or_else(|| "?".into());
    let lines = vec![
        format!("Actions for {name}:"),
        String::new(),
        "  [r]  restart  (in place)".into(),
        "  [s]  stop".into(),
        "  [t]  terminate  (irreversible)".into(),
        "  [b]  backup   (commit + push tree)".into(),
        "  [p]  setup    (provision / re-point git)".into(),
        String::new(),
        "[esc] cancel".into(),
    ];
    let area = centered_rect(50, 45, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" action ")),
        area,
    );
}

fn render_confirm(f: &mut Frame, c: &Confirm) {
    let mut text = String::new();
    if c.action.requires_typed_name() {
        let warn = if c.action.is_destructive() { "  ⚠ IRREVERSIBLE" } else { "" };
        text.push_str(&format!(
            "{} pod '{}'{}\n\nType the pod name to confirm:\n\n  > {}\n\n[enter] apply  [esc] cancel",
            c.action.label(),
            c.pod_name,
            warn,
            c.typed,
        ));
    } else {
        text.push_str(&format!("{} pod '{}'\n\n", c.action.label(), c.pod_name));
        if let Some(p) = &c.preview {
            text.push_str("Will run:\n");
            text.push_str(p);
            text.push_str("\n\n");
        }
        text.push_str("[y] apply   [n/esc] cancel");
    }
    let border = if c.action.is_destructive() { Color::Red } else { Color::Yellow };
    let area = centered_rect(70, 55, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border))
                    .title(format!(" confirm {} ", c.action.label())),
            ),
        area,
    );
}

fn render_fleet_menu(f: &mut Frame, shared: &Shared) {
    let n = shared.pods.len();
    let lines = vec![
        format!("Fleet actions — all {n} pods:"),
        String::new(),
        "  [r]  restart all  (in place)".into(),
        "  [b]  backup all   (commit + push trees)".into(),
        "  [p]  setup all    (provision / re-point git)".into(),
        String::new(),
        "(stop/terminate are per-pod only — use [a])".into(),
        "[esc] cancel".into(),
    ];
    let area = centered_rect(56, 45, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" fleet action ")),
        area,
    );
}

fn render_fleet_confirm(f: &mut Frame, shared: &Shared, action: Action, typed: &str) {
    let n = shared.pods.len();
    let text = format!(
        "{} ALL {n} pods.\n\nThis runs across the whole fleet. Type ALL to confirm:\n\n  > {}\n\n[enter] apply  [esc] cancel",
        action.label(),
        typed
    );
    let area = centered_rect(64, 45, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(format!(" confirm fleet {} ", action.label())),
        ),
        area,
    );
}

fn render_new_pod(
    f: &mut Frame,
    free: &[String],
    count: usize,
    gpu_types: &[String],
    gpu_idx: usize,
    gpu_count: u32,
) {
    let planned: Vec<&String> = free.iter().take(count).collect();
    let gpu_type = gpu_types.get(gpu_idx).map(String::as_str).unwrap_or("-");
    let mut text = format!(
        "Add pods:\n\n  GPU type:  ‹ {gpu_type} ›        (← →)\n  GPUs/pod:  {gpu_count}                       (+ -)\n  pods:      {}  of {} free        (↑ ↓)\n\nwill create:\n",
        planned.len(),
        free.len(),
    );
    if planned.is_empty() {
        text.push_str("  (no free machine names left in MACHINE_NAME_LIST)\n");
    } else {
        for name in &planned {
            text.push_str(&format!("  • {name}\n"));
        }
    }
    text.push_str("\n[enter] CREATE   [esc] cancel");
    let area = centered_rect(64, 65, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" add pod "),
        ),
        area,
    );
}

fn render_result(f: &mut Frame, msg: &str) {
    // Grow the box for multi-line results (e.g. a fleet summary with failures).
    let lines = msg.lines().count() as u16 + 2;
    let pct_y = (lines * 100 / f.area().height.max(1) + 6).clamp(20, 80);
    let area = centered_rect(60, pct_y, f.area());
    f.render_widget(Clear, area);
    let color = if msg.contains('✗') { Color::Red } else { Color::Green };
    f.render_widget(
        Paragraph::new(format!("{msg}\n\n[any key] dismiss"))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .title(" result "),
            ),
        area,
    );
}
