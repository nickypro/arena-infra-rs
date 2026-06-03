//! `arena-tui` — the interactive dashboard.
//!
//! It lists pods from the configured provider and, for each pod with an SSH endpoint,
//! fetches GPU stats (`nvidia-smi`) and an optional operator-defined progress signal
//! (`PROGRESS_CMD`) over SSH. Fetching runs in a **background task** so the UI never
//! blocks: the loop redraws and handles keys continuously while fresh data lands as
//! soon as each sweep finishes. Move the cursor with `↑/↓`/`j/k`, open a per-pod detail
//! pane (per-GPU breakdown + util/temp sparklines) with `Enter`, act on the selected
//! pod with `a` (restart / stop / terminate / backup / setup / test / run / set-branch),
//! `f` cycles the refresh cadence, `r` refreshes now.
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
use arena_core::status::display_status;
use state::{
    display_name, short_branch, spark, summarize, Action, Confirm, FleetSummary, History,
    NewPodForm, NpField, ProviderOpt,
};

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
    /// Interactive add-pod form (↑↓ field, ←→ value).
    NewPod(NewPodForm),
    /// Confirm terminating the marked (multi-selected) pods; type the count to confirm.
    TermMarked { typed: String },
    /// Collect a free-text argument (a command, or a branch) for `Run`/`SetBranch`,
    /// against one pod or the whole fleet, then execute on Enter.
    Input { action: Action, scope: InputScope, value: String },
    /// The outcome of the last action; any key dismisses (then we nudge a refresh).
    Result(String),
}

/// Who an [`Mode::Input`] action targets.
#[derive(Debug, Clone)]
enum InputScope {
    Pod { name: String, id: String },
    Fleet,
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
    /// Optimistic placeholders for pods we just created but the provider's list API
    /// hasn't reported yet — shown as "pending" so a new card appears immediately. The
    /// fetcher drops each one as soon as the real pod shows up.
    pending: Vec<Pod>,
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
    /// Pod ids marked for a bulk action (multi-select via space).
    marked: std::collections::HashSet<String>,
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
        marked: std::collections::HashSet::new(),
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

                let mut s = shared.lock().unwrap();
                // Drop optimistic placeholders the provider now reports, then show the
                // real fleet plus any still-pending placeholders.
                s.pending.retain(|pp| !pods.iter().any(|r| r.name == pp.name));
                let mut display = pods;
                display.extend(s.pending.iter().cloned());
                display.sort_by(|a, b| a.name.cmp(&b.name));

                for pod in &display {
                    let m = metrics.get(&pod.name);
                    let mem_pct = m.and_then(|m| m.mem_summary()).map(|(u, t)| {
                        if t > 0 { (u as u64 * 100 / t as u64) as u32 } else { 0 }
                    });
                    s.history.entry(pod.name.clone()).or_default().push(
                        m.and_then(|m| m.mean_util()),
                        m.and_then(|m| m.max_temp()),
                        mem_pct,
                    );
                }
                s.summary = summarize(&display, &metrics);
                s.status = status_line(&s.summary);
                s.metrics = metrics;
                s.pods = display;
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
                    KeyCode::Char(' ') => {
                        // Toggle multi-select mark on the cursor pod.
                        if let Some(pod) = selected_pod(shared, ui.selected) {
                            if !ui.marked.remove(&pod.id) {
                                ui.marked.insert(pod.id);
                            }
                        }
                    }
                    KeyCode::Char('c') => ui.marked.clear(),
                    KeyCode::Char('x') => {
                        if !ui.marked.is_empty() {
                            ui.mode = Mode::TermMarked { typed: String::new() };
                        }
                    }
                    KeyCode::Char('n') => {
                        // Fresh list (not the cached snapshot) so we never allocate a
                        // name that already exists and create a duplicate.
                        match provider.list_pods().await {
                            Ok(existing) => {
                                ui.mode =
                                    Mode::NewPod(build_new_pod_form(&ui.cfg, &ui.provider_name, &existing));
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
                        let shown = ui.shown_name(&pod.name);
                        ui.mode = if action.needs_input() {
                            // Run / set-branch: collect the command/branch first.
                            Mode::Input {
                                action,
                                scope: InputScope::Pod { name: shown, id: pod.id },
                                value: String::new(),
                            }
                        } else {
                            let preview = build_preview(&ui.cfg, action, &pod);
                            Mode::Confirm(Confirm::new(action, shown, pod.id, preview))
                        };
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
                // Safe mutating ops (require typing ALL) — stop/terminate stay per-pod.
                KeyCode::Char(ch @ ('r' | 'b' | 'p')) => {
                    let action = match ch {
                        'r' => Action::Restart,
                        'b' => Action::Backup,
                        _ => Action::Setup,
                    };
                    ui.mode = Mode::FleetConfirm { action, typed: String::new() };
                }
                // Set-branch / run need an argument typed first.
                KeyCode::Char(ch @ ('x' | 'g')) => {
                    let action = if ch == 'x' { Action::Run } else { Action::SetBranch };
                    ui.mode = Mode::Input { action, scope: InputScope::Fleet, value: String::new() };
                }
                // Test is read-only — run it across the fleet right away, no ALL gate.
                KeyCode::Char('e') => {
                    let pods = shared.lock().unwrap().pods.clone();
                    ui.mode = Mode::Result(format!("testing torch on {} pods…", pods.len()));
                    {
                        let s = shared.lock().unwrap();
                        terminal.draw(|f| view(f, &s, &ui, secs))?;
                    }
                    let msg = execute_fleet(provider, &ui.cfg, Action::Test, pods, None).await;
                    ui.mode = Mode::Result(msg);
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
                    let msg = execute_fleet(provider, &ui.cfg, action, pods, None).await;
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
            Mode::NewPod(mut form) => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Up | KeyCode::Char('k') => {
                    form.move_field(-1);
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.move_field(1);
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                    let delta = if matches!(code, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
                    if form.change(delta) {
                        // Provider changed — re-list its pods for accurate free names.
                        if let Some(free) = relist_free(&ui.cfg, form.provider_name()).await {
                            form.count = form.count.min(free.len().max(1));
                            form.free = free;
                        }
                    }
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Enter => {
                    let pname = form.provider_name().to_string();
                    match build_provider(&ui.cfg, &pname) {
                        Err(e) => ui.mode = Mode::Result(format!("✗ {pname}: {e}")),
                        Ok(prov) => match prov.list_pods().await {
                            Err(e) => ui.mode = Mode::Result(format!("✗ list {pname}: {e}")),
                            Ok(existing) => {
                                let prefix = ui.cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
                                let free = naming::next_free_names(
                                    prefix,
                                    &ui.cfg.machine_names,
                                    &existing,
                                    form.count,
                                );
                                if free.is_empty() {
                                    ui.mode = Mode::Result("✗ no free machine names".into());
                                } else {
                                    let cloud = form.cloud_type().map(String::from);
                                    let gpu_type = form.gpu_type().to_string();
                                    let disk = form.disk_gb;
                                    let volume = form.volume_gb;
                                    ui.mode = Mode::Result(format!(
                                        "creating {} pod(s) on {pname}…",
                                        free.len()
                                    ));
                                    {
                                        let s = shared.lock().unwrap();
                                        terminal.draw(|f| view(f, &s, &ui, secs))?;
                                    }
                                    let (msg, created) = create_pods(
                                        &prov,
                                        &ui.cfg,
                                        &free,
                                        cloud.as_deref(),
                                        &gpu_type,
                                        form.gpu_count,
                                        Some(disk),
                                        Some(volume),
                                    )
                                    .await;
                                    // Show the new pods immediately as "pending" until
                                    // the provider's list API reports them.
                                    {
                                        let mut s = shared.lock().unwrap();
                                        for mut p in created {
                                            p.status = "pending".into();
                                            p.ssh_ip = None;
                                            p.ssh_port = None;
                                            if !s.pending.iter().any(|x| x.name == p.name) {
                                                s.pending.push(p);
                                            }
                                        }
                                    }
                                    ui.mode = Mode::Result(msg);
                                    nudge.notify_one();
                                }
                            }
                        },
                    }
                }
                _ => ui.mode = Mode::NewPod(form),
            },
            Mode::TermMarked { mut typed } => {
                let n = ui.marked.len();
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    // Require typing the count — a deliberate gate for a bulk delete.
                    KeyCode::Enter if typed == n.to_string() => {
                        let pods: Vec<Pod> = {
                            let s = shared.lock().unwrap();
                            s.pods.iter().filter(|p| ui.marked.contains(&p.id)).cloned().collect()
                        };
                        ui.mode = Mode::Result(format!("terminating {} pod(s)…", pods.len()));
                        {
                            let s = shared.lock().unwrap();
                            terminal.draw(|f| view(f, &s, &ui, secs))?;
                        }
                        let msg = terminate_many(provider, pods).await;
                        ui.marked.clear();
                        ui.mode = Mode::Result(msg);
                        nudge.notify_one();
                    }
                    KeyCode::Backspace => {
                        typed.pop();
                        ui.mode = Mode::TermMarked { typed };
                    }
                    KeyCode::Char(ch) => {
                        typed.push(ch);
                        ui.mode = Mode::TermMarked { typed };
                    }
                    _ => ui.mode = Mode::TermMarked { typed },
                }
            }
            Mode::Input { action, scope, mut value } => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if !value.trim().is_empty() => {
                    match scope {
                        InputScope::Pod { name, id } => {
                            let pod = {
                                let s = shared.lock().unwrap();
                                s.pods.iter().find(|p| p.id == id).cloned()
                            };
                            match pod {
                                None => ui.mode = Mode::Result(format!("{name} is gone — refresh")),
                                Some(pod) => {
                                    ui.mode = Mode::Result(format!("{} on {name}…", action.label()));
                                    {
                                        let s = shared.lock().unwrap();
                                        terminal.draw(|f| view(f, &s, &ui, secs))?;
                                    }
                                    let msg = execute(provider.as_ref(), &ui.cfg, action, &pod, Some(&value)).await;
                                    ui.mode = Mode::Result(msg);
                                    nudge.notify_one();
                                }
                            }
                        }
                        InputScope::Fleet => {
                            let pods = shared.lock().unwrap().pods.clone();
                            ui.mode = Mode::Result(format!("{} on {} pods…", action.label(), pods.len()));
                            {
                                let s = shared.lock().unwrap();
                                terminal.draw(|f| view(f, &s, &ui, secs))?;
                            }
                            let msg = execute_fleet(provider, &ui.cfg, action, pods, Some(value.clone())).await;
                            ui.mode = Mode::Result(msg);
                            nudge.notify_one();
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                    ui.mode = Mode::Input { action, scope, value };
                }
                KeyCode::Char(ch) => {
                    value.push(ch);
                    ui.mode = Mode::Input { action, scope, value };
                }
                _ => ui.mode = Mode::Input { action, scope, value },
            },
            Mode::Result(_) => {
                // Any key dismisses the result, then nudge a refresh to re-sync.
                ui.mode = Mode::List;
                nudge.notify_one();
            }
        }
    }
    Ok(())
}

/// Terminate several pods concurrently, returning a one-line summary + first failures.
async fn terminate_many(provider: &Arc<dyn Provider>, pods: Vec<Pod>) -> String {
    let total = pods.len();
    let mut set = JoinSet::new();
    for pod in pods {
        let provider = provider.clone();
        set.spawn(async move {
            match provider.terminate_pod(&pod.id).await {
                Ok(()) => Ok(pod.name),
                Err(e) => Err(format!("✗ {}: {e}", pod.name)),
            }
        });
    }
    let mut ok = 0usize;
    let mut fails: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(_)) => ok += 1,
            Ok(Err(e)) => fails.push(e),
            Err(_) => fails.push("✗ task failed".into()),
        }
    }
    let mut s = format!("terminated {ok}/{total}");
    for f in fails.iter().take(8) {
        s.push('\n');
        s.push_str(f);
    }
    s
}

/// Run a safe action against every pod concurrently, returning a summary line plus the
/// first few failures. Used by the fleet menu (restart / backup / setup only).
async fn execute_fleet(
    provider: &Arc<dyn Provider>,
    cfg: &Config,
    action: Action,
    pods: Vec<Pod>,
    arg: Option<String>,
) -> String {
    let total = pods.len();
    let mut set = JoinSet::new();
    for pod in pods {
        let provider = provider.clone();
        let cfg = cfg.clone();
        let arg = arg.clone();
        set.spawn(async move { execute(provider.as_ref(), &cfg, action, &pod, arg.as_deref()).await });
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

/// Build the GPU-type choices for the add-pod form: the config default first (so the
/// default create matches the CLI), then the shared core presets, de-duplicated.
fn gpu_type_choices(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let default = PodSpec::from_config(cfg).gpu_type;
    if !default.is_empty() {
        out.push(default);
    }
    for api in arena_core::gpu::preset_apis() {
        if !out.iter().any(|x| *x == api) {
            out.push(api);
        }
    }
    if out.is_empty() {
        out.push("NVIDIA RTX A4000".to_string());
    }
    out
}

/// Build a provider by name into an `Arc` (the TUI shares providers across tasks).
fn build_provider(cfg: &Config, name: &str) -> Result<Arc<dyn Provider>> {
    Ok(Arc::from(arena_core::provider::build(name, cfg)?))
}

/// List a provider's pods and compute the free machine names. None on any failure.
async fn relist_free(cfg: &Config, provider_name: &str) -> Option<Vec<String>> {
    let prov = build_provider(cfg, provider_name).ok()?;
    let existing = prov.list_pods().await.ok()?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    Some(naming::next_free_names(prefix, &cfg.machine_names, &existing, usize::MAX))
}

/// Seed the add-pod form: provider availability from configured API keys (default to
/// the launch provider if it has a key), GPU choices, and the free names for `existing`.
fn build_new_pod_form(cfg: &Config, launch_provider: &str, existing: &[Pod]) -> NewPodForm {
    let key_for = |p: &str| match p {
        "runpod" => "RUNPOD_API_KEY",
        "vast" => "VAST_API_KEY",
        "hetzner" => "HETZNER_API_KEY",
        _ => "",
    };
    let providers: Vec<ProviderOpt> = ["runpod", "vast", "hetzner"]
        .iter()
        .map(|p| ProviderOpt {
            name: p.to_string(),
            available: cfg.get(key_for(p)).map(|v| !v.is_empty()).unwrap_or(false),
        })
        .collect();
    let provider_idx = providers
        .iter()
        .position(|o| o.name == launch_provider && o.available)
        .or_else(|| providers.iter().position(|o| o.available))
        .unwrap_or(0);
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let free = naming::next_free_names(prefix, &cfg.machine_names, existing, usize::MAX);
    let spec = PodSpec::from_config(cfg);
    NewPodForm {
        providers,
        provider_idx,
        cloud_types: vec!["COMMUNITY".into(), "SECURE".into()],
        cloud_idx: usize::from(spec.cloud_type.eq_ignore_ascii_case("SECURE")),
        gpu_types: gpu_type_choices(cfg),
        gpu_idx: 0,
        gpu_count: spec.gpu_count.max(1),
        disk_gb: spec.disk_gb.max(20),
        volume_gb: spec.volume_gb,
        count: 1,
        free,
        field: 0,
    }
}

/// Create the named pods (sequentially, no capacity-wait), returning a summary line.
/// Each create is gated by the add-pod form's Enter, mirroring `arena pods create`.
async fn create_pods(
    provider: &Arc<dyn Provider>,
    cfg: &Config,
    names: &[String],
    cloud_type: Option<&str>,
    gpu_type: &str,
    gpu_count: u32,
    disk_gb: Option<u32>,
    volume_gb: Option<u32>,
) -> (String, Vec<Pod>) {
    let mut base = PodSpec::from_config(cfg);
    base.gpu_type = gpu_type.to_string();
    base.gpu_count = gpu_count;
    if let Some(c) = cloud_type {
        base.cloud_type = c.to_string();
    }
    if let Some(d) = disk_gb {
        base.disk_gb = d;
    }
    if let Some(v) = volume_gb {
        base.volume_gb = v;
    }
    let mut created: Vec<Pod> = Vec::new();
    let mut fails: Vec<String> = Vec::new();
    for name in names {
        let mut spec = base.clone();
        spec.name = name.clone();
        spec.env.push(("MACHINE_NAME".into(), name.clone()));
        match provider.create_pod(&spec).await {
            Ok(pod) => created.push(pod),
            Err(e) => fails.push(format!("✗ {name}: {e}")),
        }
    }
    let mut s = format!("created {}/{}", created.len(), names.len());
    for f in fails.iter().take(8) {
        s.push('\n');
        s.push_str(f);
    }
    (s, created)
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

    let msg = execute(provider.as_ref(), &ui.cfg, c.action, &pod, None).await;
    ui.mode = Mode::Result(msg);
    Ok(())
}

/// Perform one action against a pod, returning a one-line human-readable outcome.
/// This is the *only* place the dashboard mutates anything. `arg` carries the typed
/// command (`Run`) or branch (`SetBranch`); it's `None` for the other actions.
async fn execute(provider: &dyn Provider, cfg: &Config, action: Action, pod: &Pod, arg: Option<&str>) -> String {
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
        Action::Test => run_ssh_oneline(cfg, pod, TORCH_TEST_CMD, "torch").await,
        Action::Run => match arg {
            Some(cmd) if !cmd.trim().is_empty() => run_ssh_oneline(cfg, pod, cmd, "run").await,
            _ => format!("✗ {name}: no command given"),
        },
        Action::SetBranch => match arg {
            Some(branch) if !branch.trim().is_empty() => run_set_branch(cfg, pod, branch.trim()).await,
            _ => format!("✗ {name}: no branch given"),
        },
    }
}

/// The torch health check (mirrors `arena pods test`).
const TORCH_TEST_CMD: &str =
    "python -c 'import torch; print(torch.__version__)' 2>&1 || python3 -c 'import torch; print(torch.__version__)'";

/// Run a command over SSH and report its last output line (read-only flows: test / run).
async fn run_ssh_oneline(cfg: &Config, pod: &Pod, cmd: &str, what: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match ssh::run(&target, cmd).await {
        Ok(out) if out.success => {
            let line = out.stdout.lines().last().unwrap_or("").trim();
            format!("✓ {} {what}: {line}", pod.name)
        }
        Ok(out) => format!("✗ {} {what} (exit {:?}): {}", pod.name, out.code, out.stderr.trim()),
        Err(e) => format!("✗ {} {what} failed: {e}", pod.name),
    }
}

/// Gently switch a pod's ARENA checkout to `branch` (mirrors `arena pods set-branch`).
async fn run_set_branch(cfg: &Config, pod: &Pod, branch: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0"))
    });
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, cfg.get("GIT_SSH_KEY_REMOTE"));
    match ssh::run(&target, &cmd).await {
        Ok(out) if out.success => format!("✓ {} → {branch}", pod.name),
        Ok(out) => format!("✗ {} set-branch (exit {:?}): {}", pod.name, out.code, out.stderr.trim()),
        Err(e) => format!("✗ {} set-branch failed: {e}", pod.name),
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
        Action::Test => Some(match &target {
            Ok(t) => t.display_command(TORCH_TEST_CMD),
            Err(e) => format!("⚠ {e}"),
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
        // No room for the history sparklines in the split view.
        pods_table(f, shared, ui, cols[0], false);
        detail_pane(f, shared, ui, cols[1]);
    } else {
        pods_table(f, shared, ui, chunks[2], true);
    }

    f.render_widget(Paragraph::new(footer_hint(shared, ui, secs)), chunks[3]);

    match &ui.mode {
        Mode::Menu => render_menu(f, shared, ui),
        Mode::Confirm(c) => render_confirm(f, c),
        Mode::FleetMenu => render_fleet_menu(f, shared),
        Mode::FleetConfirm { action, typed } => render_fleet_confirm(f, shared, *action, typed),
        Mode::NewPod(form) => render_new_pod(f, form),
        Mode::TermMarked { typed } => render_term_marked(f, shared, ui, typed),
        Mode::Input { action, scope, value } => render_input(f, shared, *action, scope, value),
        Mode::Result(msg) => render_result(f, msg),
        _ => {}
    }
}

/// The free-text input modal for `run` / `set-branch` (one pod or the whole fleet).
fn render_input(f: &mut Frame, shared: &Shared, action: Action, scope: &InputScope, value: &str) {
    let who = match scope {
        InputScope::Pod { name, .. } => name.clone(),
        InputScope::Fleet => format!("ALL {} pods", shared.pods.len()),
    };
    let text = format!(
        "{} on {who}\n\n{}\n\n  > {value}\n\n[enter] run  [esc] cancel",
        action.label(),
        action.input_prompt(),
    );
    let area = centered_rect(70, 40, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(" {} ", action.label())),
            ),
        area,
    );
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

fn pods_table(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect, with_spark: bool) {
    const SPARK_W: usize = 12;
    // Responsive: the essential columns (~92 wide) always show; the nice-to-haves
    // (PROGRESS, then the GPU%/MEM% graphs) are dropped when the terminal is too narrow
    // so the core data isn't crushed to one column each.
    let w = area.width as usize;
    let show_progress = w >= 104;
    let show_spark = with_spark && w >= 132;

    let mut header_cells =
        vec!["", "P", "NAME", "STATUS", "SET", "GPU", "GPU%", "MEM", "TEMP", "DISK", "$/HR", "BRANCH"];
    if show_progress {
        header_cells.push("PROGRESS / ERROR");
    }
    if show_spark {
        header_cells.push("GPU%~");
        header_cells.push("MEM%~");
    }
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

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
            let disk = match m.and_then(|m| m.disk_summary()) {
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
                Some(b) => truncate(&short_branch(&b, &ui.prefix), 6),
                None => "-".into(),
            };
            // RUNNING-but-unreachable reads as "init" (still coming up), in yellow.
            let status_label = display_status(&p.status, m.map(|m| m.error.is_none()));
            let status_cell = if status_label == "init" {
                Cell::from(status_label).style(Style::default().fg(Color::Yellow))
            } else {
                Cell::from(status_label)
            };
            let mark = if ui.marked.contains(&p.id) {
                Cell::from("•").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let mut cells = vec![
                mark,
                provider_cell(&p.provider),
                Cell::from(ui.shown_name(&p.name)),
                status_cell,
                health_cell(m),
                Cell::from(gpu),
                Cell::from(util_str).style(util_style(util, err)),
                Cell::from(mem),
                Cell::from(temp_str).style(temp_style(temp)),
                Cell::from(disk),
                Cell::from(cost),
                Cell::from(branch),
            ];
            if show_progress {
                let (detail, detail_style) = detail_cell(m);
                cells.push(Cell::from(detail).style(detail_style));
            }
            if show_spark {
                let h = shared.history.get(&p.name);
                let gpu_hist = h.map(|h| h.util_data()).unwrap_or_default();
                let mem_hist = h.map(|h| h.mem_data()).unwrap_or_default();
                // Grey the sparklines out when the pod isn't reachable — the history is
                // just zeros then, so colour would imply live data that isn't there.
                let connected = m.map(|m| m.error.is_none() && !m.gpus.is_empty()).unwrap_or(false);
                let (gpu_c, mem_c) = if connected {
                    (Color::Green, Color::Cyan)
                } else {
                    (Color::DarkGray, Color::DarkGray)
                };
                cells.push(Cell::from(spark(&gpu_hist, SPARK_W)).style(Style::default().fg(gpu_c)));
                cells.push(Cell::from(spark(&mem_hist, SPARK_W)).style(Style::default().fg(mem_c)));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1),  // mark (•)
        Constraint::Length(1),  // P (provider glyph)
        Constraint::Length(16), // NAME
        Constraint::Length(4),  // STATUS (abbreviated: run/exit/stop…)
        Constraint::Length(3),  // SET (✓✓✓)
        Constraint::Length(16), // GPU (e.g. "A100 80GB PCIe")
        Constraint::Length(5),  // GPU%
        Constraint::Length(9),  // MEM (e.g. "120/240G")
        Constraint::Length(4),  // TEMP (e.g. "85C")
        Constraint::Length(9),  // DISK (e.g. "12/100G")
        Constraint::Length(7),  // $/HR
        Constraint::Length(6),  // BRANCH (e.g. "w1d2")
    ];
    if show_progress {
        widths.push(Constraint::Min(10)); // PROGRESS / ERROR (flexible)
    }
    if show_spark {
        widths.push(Constraint::Length(SPARK_W as u16)); // GPU% history
        widths.push(Constraint::Length(SPARK_W as u16)); // MEM% history
    }
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

    // Only show the history graphs if the pane is tall enough; otherwise give the space
    // to the facts + per-GPU table (the graphs are a nice-to-have).
    let show_graphs = inner.height >= 18;
    let constraints: &[Constraint] = if show_graphs {
        &[
            Constraint::Length(9), // header facts
            Constraint::Min(3),    // per-GPU table
            Constraint::Length(3), // util sparkline
            Constraint::Length(3), // temp sparkline
        ]
    } else {
        &[
            Constraint::Length(9), // header facts
            Constraint::Min(3),    // per-GPU table
        ]
    };
    let rows = Layout::default().direction(Direction::Vertical).constraints(constraints).split(inner);

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
    let disk = match m.and_then(|m| m.disk_summary()) {
        Some((u, t)) => {
            let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
            format!("{:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0)
        }
        None => "-".into(),
    };
    let ok = |b: Option<bool>| match b {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "·",
    };
    let origin = m.and_then(|m| m.origin.clone()).unwrap_or_else(|| "-".into());
    let origin_ok = m.and_then(|m| m.origin.as_deref().map(|o| o.contains("github.com")));
    let facts = format!(
        "status:   {}\ngpu:      {}\nendpoint: {}\ncost:     {}\ndisk:     {}\nbranch:   {}\norigin:   {} {}\nsetup:    .name {}   key {}   origin→gh {}\nprogress: {}",
        display_status(&pod.status, m.map(|m| m.error.is_none())),
        gpu,
        endpoint,
        cost,
        disk,
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

    if show_graphs {
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
}

fn footer_hint(shared: &Shared, ui: &Ui, secs: u64) -> String {
    let age = match shared.last_refresh {
        Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
        None => "never updated".into(),
    };
    let spin = if shared.refreshing { " ⟳" } else { "" };
    let keys = match ui.mode {
        Mode::List => "[enter] detail  [space] mark  [x] term marked  [a] act  [A] fleet  [n] new  [s] names  [r] now  [q] quit",
        Mode::Detail => "[space] mark  [x] term marked  [a] act  [A] fleet  [n] new  [s] names  [r] now  [esc] back  [q] quit",
        Mode::Menu => "[r/s/t/b/p/e/x/g] choose action  [esc] cancel",
        Mode::Confirm(_) => "type to confirm  [enter] apply  [esc] cancel",
        Mode::FleetMenu => "[r/b/p/e/x/g] choose fleet action  [esc] cancel",
        Mode::FleetConfirm { .. } => "type ALL to confirm  [enter] apply  [esc] cancel",
        Mode::NewPod { .. } => "[↑↓] pods  [+-] gpus  [←→] type  [enter] create  [esc] cancel",
        Mode::TermMarked { .. } => "type the count to confirm  [enter] terminate  [esc] cancel",
        Mode::Input { .. } => "type the value  [enter] run  [esc] cancel",
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
        "  [r]  restart   (in place)".into(),
        "  [s]  stop".into(),
        "  [t]  terminate (irreversible)".into(),
        "  [b]  backup    (commit + push tree)".into(),
        "  [p]  setup     (provision / re-point git)".into(),
        "  [e]  test      (torch version)".into(),
        "  [x]  run       (type a shell command)".into(),
        "  [g]  set-branch (type a branch)".into(),
        String::new(),
        "[esc] cancel".into(),
    ];
    let area = centered_rect(54, 55, f.area());
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
        "  [r]  restart all   (in place)".into(),
        "  [b]  backup all    (commit + push trees)".into(),
        "  [p]  setup all     (provision / re-point git)".into(),
        "  [e]  test all      (torch version)".into(),
        "  [x]  run all       (type a shell command)".into(),
        "  [g]  set-branch all (type a branch)".into(),
        String::new(),
        "(stop/terminate are per-pod only — use [a])".into(),
        "[esc] cancel".into(),
    ];
    let area = centered_rect(60, 55, f.area());
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

fn render_new_pod(f: &mut Frame, form: &NewPodForm) {
    let sel = form.selected();
    let dim = Style::default().fg(Color::DarkGray);
    let cur = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let label = Style::default().add_modifier(Modifier::BOLD);

    // One line per applicable field: "▶ label  ‹ value ›", selected one highlighted.
    let mut lines: Vec<Line> = Vec::new();
    for fld in form.fields() {
        let selected = fld == sel;
        let (name, value) = match fld {
            NpField::Provider => ("provider", form.provider_name().to_string()),
            NpField::CloudType => ("cloud", form.cloud_type().unwrap_or("-").to_string()),
            NpField::GpuType => {
                let price = arena_core::gpu::price_label(form.gpu_type(), form.provider_name(), form.cloud_type())
                    .map(|p| format!("  {p}"))
                    .unwrap_or_default();
                ("gpu type", format!("{}{price}", arena_core::gpu::label(form.gpu_type())))
            }
            NpField::GpuCount => ("gpus/pod", form.gpu_count.to_string()),
            NpField::Disk => ("disk", format!("{}GB", form.disk_gb)),
            NpField::Volume => (
                "volume",
                if form.volume_gb == 0 { "none".to_string() } else { format!("{}GB", form.volume_gb) },
            ),
            NpField::Pods => ("pods", format!("{} of {} free", form.count, form.free.len())),
        };
        let marker = if selected { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{name:<9} "), if selected { cur } else { label }),
            Span::styled(
                format!("‹ {value} ›"),
                if selected { cur } else { Style::default() },
            ),
        ]));
    }

    lines.push(Line::from(""));
    // Options for the currently-selected choice field — so the GPU/provider/cloud
    // menus are visible, not guessed.
    match sel {
        NpField::Provider => {
            lines.push(Line::styled("providers:", label));
            for p in &form.providers {
                let mark = if p.name == form.provider_name() { "●" } else { "○" };
                let text = if p.available {
                    format!("  {mark} {}", p.name)
                } else {
                    format!("  {mark} {} (no api key)", p.name)
                };
                lines.push(Line::styled(
                    text,
                    if !p.available {
                        dim
                    } else if p.name == form.provider_name() {
                        cur
                    } else {
                        Style::default()
                    },
                ));
            }
        }
        NpField::CloudType => {
            lines.push(Line::styled("cloud types:", label));
            for (i, c) in form.cloud_types.iter().enumerate() {
                let mark = if i == form.cloud_idx { "●" } else { "○" };
                lines.push(Line::styled(
                    format!("  {mark} {c}"),
                    if i == form.cloud_idx { cur } else { Style::default() },
                ));
            }
        }
        NpField::GpuType => {
            lines.push(Line::styled("gpu types (← → · VRAM · ~price for this provider/cloud):", label));
            for (i, g) in form.gpu_types.iter().enumerate() {
                let mark = if i == form.gpu_idx { "●" } else { "○" };
                let price = arena_core::gpu::price_label(g, form.provider_name(), form.cloud_type())
                    .map(|p| format!("   {p}"))
                    .unwrap_or_default();
                lines.push(Line::styled(
                    format!("  {mark} {:<16}{price}", arena_core::gpu::label(g)),
                    if i == form.gpu_idx { cur } else { Style::default() },
                ));
            }
        }
        NpField::GpuCount => lines.push(Line::styled("GPUs per pod: 1–8", dim)),
        NpField::Disk => lines.push(Line::styled(
            "container disk in 50GB steps — the main storage (wiped if the pod is destroyed).",
            dim,
        )),
        NpField::Volume => lines.push(Line::styled(
            "persistent volume in 100GB steps (0 = none, the default). Survives restarts.",
            dim,
        )),
        NpField::Pods => {
            let planned = form.planned_names();
            lines.push(Line::styled("will create:", label));
            if planned.is_empty() {
                lines.push(Line::styled("  (no free machine names left)", dim));
            } else {
                for n in planned {
                    lines.push(Line::from(format!("  • {n}")));
                }
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[↑↓] field   [←→] change   [enter] CREATE   [esc] cancel",
        dim,
    ));

    let area = centered_rect(60, 75, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" add pod "),
        ),
        area,
    );
}

fn render_term_marked(f: &mut Frame, shared: &Shared, ui: &Ui, typed: &str) {
    let names: Vec<String> = shared
        .pods
        .iter()
        .filter(|p| ui.marked.contains(&p.id))
        .map(|p| ui.shown_name(&p.name))
        .collect();
    let n = names.len();

    let area = centered_rect(64, 80, f.area());
    let dim = Style::default().fg(Color::DarkGray);

    // Header first, so the count-to-type + input field are NEVER pushed off-screen by a
    // long list. The list below is then capped to whatever space remains.
    let mut lines: Vec<Line> = vec![
        Line::styled(
            format!("TERMINATE {n} pod(s) — IRREVERSIBLE"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from(format!("Type {n} to confirm:   > {typed}")),
        Line::styled("[enter] terminate   [esc] cancel", dim),
        Line::from(""),
        Line::styled("pods:", Style::default().add_modifier(Modifier::BOLD)),
    ];
    let header = lines.len();
    // Rows available for names inside the bordered box.
    let cap = (area.height as usize).saturating_sub(2).saturating_sub(header).max(1);
    if n <= cap {
        for name in &names {
            lines.push(Line::from(format!("  • {name}")));
        }
    } else {
        // Leave one row for the "+K more" marker.
        for name in names.iter().take(cap - 1) {
            lines.push(Line::from(format!("  • {name}")));
        }
        lines.push(Line::styled(format!("  … +{} more", n - (cap - 1)), dim));
    }

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" confirm terminate "),
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
