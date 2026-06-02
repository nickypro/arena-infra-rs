//! `arena-tui` — the interactive dashboard.
//!
//! It lists pods from the configured provider and, for each pod with an SSH endpoint,
//! fetches GPU stats (`nvidia-smi`) and an optional operator-defined progress signal
//! (`PROGRESS_CMD`) over SSH, auto-refreshing on an interval. Beyond viewing, it is
//! interactive: move the cursor with `↑/↓`/`j/k`, open a per-pod detail pane (per-GPU
//! breakdown + util/temp sparklines) with `Enter`, and act on the selected pod with
//! `a` (restart / stop / terminate / backup / setup).
//!
//! Safety against live prod is built into the *interaction*, not bolted on: a mutating
//! action always pops a confirmation modal. Lifecycle actions (restart/stop/terminate)
//! make you type the pod's exact name back before they apply; backup/setup show the
//! precise command(s) that will run and take a single `y`. There is no way to mutate a
//! pod without going through that modal — the dashboard's reads stay reads.
//!
//! Provider is chosen by `ARENA_PROVIDER` (default `runpod`); config path by
//! `ARENA_CONFIG`; auto-refresh seconds by `ARENA_REFRESH_SECS` (default 20). Metrics
//! for the whole fleet are fetched concurrently so one slow or down pod doesn't stall
//! the others.

mod state;

use std::collections::HashMap;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
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
use tokio::task::JoinSet;

use arena_core::metrics::{self, PodMetrics};
use arena_core::provider::Provider;
use arena_core::ssh::{self, SshTarget};
use arena_core::{Config, Pod};

use state::{summarize, Action, Confirm, FleetSummary, History};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

/// What the UI is currently showing. `List`/`Detail` are the normal views (and the
/// only ones that auto-refresh); the rest are modal and pause auto-refresh so an
/// in-progress confirmation can't be yanked out from under the operator.
#[derive(Debug, Clone)]
enum Mode {
    List,
    Detail,
    /// Action chooser for the selected pod.
    Menu,
    /// A pending action awaiting confirmation.
    Confirm(Confirm),
    /// The outcome of the last action; any key dismisses (then we refresh).
    Result(String),
}

struct App {
    provider: Box<dyn Provider>,
    provider_name: String,
    config_path: String,
    cfg: Config,
    progress_cmd: Option<String>,
    pods: Vec<Pod>,
    metrics: HashMap<String, PodMetrics>,
    history: HashMap<String, History>,
    summary: FleetSummary,
    status: String,
    last_refresh: Option<Instant>,
    auto_refresh: Duration,
    mode: Mode,
    /// Cursor into `pods` (kept clamped to a valid row).
    selected: usize,
}

impl App {
    async fn refresh(&mut self) {
        let mut pods = match self.provider.list_pods().await {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error listing pods: {e}");
                return;
            }
        };
        pods.sort_by(|a, b| a.name.cmp(&b.name));

        // Fan out metric fetches across the fleet; one down pod can't stall the rest.
        let mut set = JoinSet::new();
        for pod in &pods {
            if let Ok(target) = SshTarget::from_pod(pod, &self.cfg) {
                let name = pod.name.clone();
                let progress_cmd = self.progress_cmd.clone();
                set.spawn(async move {
                    (name, metrics::fetch(&target, progress_cmd.as_deref()).await)
                });
            }
        }
        let mut fresh = HashMap::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, m)) = joined {
                fresh.insert(name, m);
            }
        }

        // Record a sample per pod so the sparklines have a steady time axis.
        for pod in &pods {
            let m = fresh.get(&pod.name);
            let entry = self.history.entry(pod.name.clone()).or_default();
            entry.push(m.and_then(|m| m.mean_util()), m.and_then(|m| m.max_temp()));
        }

        self.summary = summarize(&pods, &fresh);
        self.metrics = fresh;
        self.pods = pods;
        self.clamp_selection();
        self.last_refresh = Some(Instant::now());
        self.status = format!(
            "{} pods · {} reporting{}",
            self.summary.pods,
            self.summary.reporting,
            if self.summary.unreachable > 0 {
                format!(" · {} unreachable", self.summary.unreachable)
            } else {
                String::new()
            }
        );
    }

    fn clamp_selection(&mut self) {
        if self.pods.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.pods.len() {
            self.selected = self.pods.len() - 1;
        }
    }

    fn selected_pod(&self) -> Option<&Pod> {
        self.pods.get(self.selected)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path =
        std::env::var("ARENA_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string());
    let cfg = Config::load(&PathBuf::from(&config_path))
        .with_context(|| format!("loading config {config_path}"))?;
    let provider_name = std::env::var("ARENA_PROVIDER").unwrap_or_else(|_| "runpod".to_string());
    let provider = arena_core::provider::build(&provider_name, &cfg)?;
    let progress_cmd = cfg.get("PROGRESS_CMD").map(String::from);
    let auto_refresh = Duration::from_secs(
        std::env::var("ARENA_REFRESH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20),
    );

    let mut app = App {
        provider,
        provider_name,
        config_path,
        cfg,
        progress_cmd,
        pods: Vec::new(),
        metrics: HashMap::new(),
        history: HashMap::new(),
        summary: FleetSummary::default(),
        status: "loading…".into(),
        last_refresh: None,
        auto_refresh,
        mode: Mode::List,
        selected: 0,
    };
    app.refresh().await;

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    res
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

async fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        // Auto-refresh only in the non-modal views — never mid-confirmation.
        let modal = !matches!(app.mode, Mode::List | Mode::Detail);
        let due = !modal
            && app.last_refresh.map(|t| t.elapsed() >= app.auto_refresh).unwrap_or(true);
        if due {
            app.status = "refreshing…".into();
            terminal.draw(|f| ui(f, app))?;
            app.refresh().await;
            continue;
        }

        if !event::poll(Duration::from_millis(250))? {
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

        // Clone the mode so we can freely mutate `app` while deciding what to do.
        match app.mode.clone() {
            Mode::List | Mode::Detail => {
                let in_detail = matches!(app.mode, Mode::Detail);
                match code {
                    KeyCode::Char('q') => break,
                    KeyCode::Esc => {
                        if in_detail {
                            app.mode = Mode::List;
                        } else {
                            break;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.selected = app.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if app.selected + 1 < app.pods.len() {
                            app.selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if app.selected_pod().is_some() {
                            app.mode = Mode::Detail;
                        }
                    }
                    KeyCode::Char('r') => {
                        app.status = "refreshing…".into();
                        terminal.draw(|f| ui(f, app))?;
                        app.refresh().await;
                    }
                    KeyCode::Char('a') => {
                        if app.selected_pod().is_some() {
                            app.mode = Mode::Menu;
                        }
                    }
                    _ => {}
                }
            }
            Mode::Menu => match code {
                KeyCode::Esc => app.mode = Mode::List,
                KeyCode::Char(ch) => {
                    if let (Some(action), Some(pod)) =
                        (Action::from_key(ch), app.selected_pod().cloned())
                    {
                        let preview = build_preview(&app.cfg, action, &pod);
                        app.mode = Mode::Confirm(Confirm::new(
                            action,
                            pod.name,
                            pod.id,
                            preview,
                        ));
                    }
                }
                _ => {}
            },
            Mode::Confirm(mut c) => {
                let typed = c.action.requires_typed_name();
                match code {
                    KeyCode::Esc => app.mode = Mode::List,
                    // Apply: typed-name actions need the exact name; others confirm on y/Enter.
                    KeyCode::Enter if c.is_satisfied() => {
                        apply_action(terminal, app, &c).await?;
                    }
                    KeyCode::Char('y') if !typed => {
                        apply_action(terminal, app, &c).await?;
                    }
                    KeyCode::Char('n') if !typed => app.mode = Mode::List,
                    KeyCode::Backspace if typed => {
                        c.typed.pop();
                        app.mode = Mode::Confirm(c);
                    }
                    KeyCode::Char(ch) if typed => {
                        c.typed.push(ch);
                        app.mode = Mode::Confirm(c);
                    }
                    _ => app.mode = Mode::Confirm(c),
                }
            }
            Mode::Result(_) => {
                // Any key dismisses the result, then re-sync with reality.
                app.mode = Mode::List;
                app.status = "refreshing…".into();
                terminal.draw(|f| ui(f, app))?;
                app.refresh().await;
            }
        }
    }
    Ok(())
}

/// Run a confirmed action against its pod, show a busy line, then park the outcome in
/// a result modal. The pod is looked up fresh by id (it may have moved in the list).
async fn apply_action(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    c: &Confirm,
) -> Result<()> {
    let Some(pod) = app.pods.iter().find(|p| p.id == c.pod_id).cloned() else {
        app.mode = Mode::Result(format!("{} is gone — refresh", c.pod_name));
        return Ok(());
    };
    app.status = format!("{}ing {}…", c.action.label(), pod.name);
    app.mode = Mode::Result(format!("{}ing {}…", c.action.label(), pod.name));
    terminal.draw(|f| ui(f, app))?;

    let msg = execute(app.provider.as_ref(), &app.cfg, c.action, &pod).await;
    app.mode = Mode::Result(msg);
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

fn ui(f: &mut Frame, app: &App) {
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
        app.provider_name, app.config_path
    ))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    f.render_widget(summary_line(&app.summary), chunks[1]);

    // In Detail mode, split the body so the table and the per-pod pane sit side by side.
    if matches!(app.mode, Mode::Detail) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[2]);
        pods_table(f, app, cols[0]);
        detail_pane(f, app, cols[1]);
    } else {
        pods_table(f, app, chunks[2]);
    }

    f.render_widget(Paragraph::new(footer_hint(app)), chunks[3]);

    // Modal overlays.
    match &app.mode {
        Mode::Menu => render_menu(f, app),
        Mode::Confirm(c) => render_confirm(f, c),
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
        " fleet: {} pods · {} GPUs · mean util {} · mem {} · ${:.2}/hr{}",
        s.pods, s.total_gpus, util, mem, s.total_cost, unreachable
    ))
    .style(Style::default().add_modifier(Modifier::BOLD))
}

fn pods_table(f: &mut Frame, app: &App, area: Rect) {
    let header =
        Row::new(vec!["NAME", "STATUS", "GPU", "GPU%", "MEM", "TEMP", "$/HR", "PROGRESS / ERROR"])
            .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .pods
        .iter()
        .map(|p| {
            let m = app.metrics.get(&p.name);
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
            let (detail, detail_style) = detail_cell(m);
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(p.status.clone()),
                Cell::from(p.gpu_type.clone().unwrap_or_else(|| "-".into())),
                Cell::from(util_str).style(util_style(util, err)),
                Cell::from(mem),
                Cell::from(temp_str).style(temp_style(temp)),
                Cell::from(cost),
                Cell::from(detail).style(detail_style),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(20), // NAME
        Constraint::Length(9),  // STATUS
        Constraint::Length(14), // GPU
        Constraint::Length(6),  // GPU%
        Constraint::Length(8),  // MEM (e.g. "120/240G")
        Constraint::Length(4),  // TEMP (e.g. "85C")
        Constraint::Length(7),  // $/HR
        Constraint::Min(10),    // PROGRESS / ERROR
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .block(Block::default().borders(Borders::ALL).title("Pods"));

    let mut ts = TableState::default();
    if !app.pods.is_empty() {
        ts.select(Some(app.selected));
    }
    f.render_stateful_widget(table, area, &mut ts);
}

/// The per-pod detail pane (shown in Detail mode): identity + endpoint, a per-GPU
/// table, full progress text, and util/temp sparklines from the rolling history.
fn detail_pane(f: &mut Frame, app: &App, area: Rect) {
    let Some(pod) = app.selected_pod() else { return };
    let m = app.metrics.get(&pod.name);

    let block = Block::default().borders(Borders::ALL).title(format!(" {} ", pod.name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // header facts
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
    let facts = format!(
        "status:   {}\ngpu:      {}\nendpoint: {}\ncost:     {}\nprogress: {}",
        pod.status,
        pod.gpu_type.clone().unwrap_or_else(|| "-".into()),
        endpoint,
        pod.cost_per_hr.map(|c| format!("${c:.2}/hr")).unwrap_or_else(|| "-".into()),
        progress,
    );
    f.render_widget(Paragraph::new(facts).wrap(Wrap { trim: true }), rows[0]);

    // Per-GPU breakdown (rather than the table's aggregate).
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

    // Sparklines from the rolling history (steady time axis; missing = 0).
    let hist = app.history.get(&pod.name);
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

fn footer_hint(app: &App) -> String {
    let age = match app.last_refresh {
        Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
        None => "never updated".into(),
    };
    let keys = match app.mode {
        Mode::List => "[↑↓/jk] select  [enter] detail  [a] actions  [r] refresh  [q] quit",
        Mode::Detail => "[↑↓/jk] select  [a] actions  [r] refresh  [esc] back  [q] quit",
        Mode::Menu => "[r/s/t/b/p] choose action  [esc] cancel",
        Mode::Confirm(_) => "type to confirm  [enter] apply  [esc] cancel",
        Mode::Result(_) => "[any key] dismiss",
    };
    format!("{} · {} · {}", app.status, age, keys)
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

fn render_menu(f: &mut Frame, app: &App) {
    let name = app.selected_pod().map(|p| p.name.as_str()).unwrap_or("?");
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

fn render_result(f: &mut Frame, msg: &str) {
    let area = centered_rect(60, 25, f.area());
    f.render_widget(Clear, area);
    let color = if msg.starts_with('✗') { Color::Red } else { Color::Green };
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
