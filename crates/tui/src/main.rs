//! `arena-tui` — the interactive dashboard. Read-only: it lists pods from the
//! configured provider and, for each pod with an SSH endpoint, fetches GPU stats
//! (`nvidia-smi`) and an optional operator-defined progress signal (`PROGRESS_CMD`)
//! over SSH. It auto-refreshes on an interval, `r` refreshes now, `q`/Esc quits. No
//! mutating actions — spin-up/backup stay in the CLI where they're explicitly gated.
//!
//! Provider is chosen by `ARENA_PROVIDER` (default `runpod`); config path by
//! `ARENA_CONFIG`; auto-refresh seconds by `ARENA_REFRESH_SECS` (default 20). Metrics
//! for the whole fleet are fetched concurrently so one slow or down pod doesn't stall
//! the others.

use std::collections::HashMap;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use tokio::task::JoinSet;

use arena_core::metrics::{self, PodMetrics};
use arena_core::provider::Provider;
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

struct App {
    provider: Box<dyn Provider>,
    provider_name: String,
    config_path: String,
    cfg: Config,
    progress_cmd: Option<String>,
    pods: Vec<Pod>,
    metrics: HashMap<String, PodMetrics>,
    status: String,
    /// When the last successful refresh completed (for the "updated Ns ago" line).
    last_refresh: Option<Instant>,
    auto_refresh: Duration,
}

impl App {
    async fn refresh(&mut self) {
        let pods = match self.provider.list_pods().await {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error listing pods: {e}");
                return;
            }
        };

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

        self.metrics = fresh;
        self.pods = pods;
        self.last_refresh = Some(Instant::now());
        let reporting = self.metrics.values().filter(|m| !m.gpus.is_empty()).count();
        let errs = self.metrics.values().filter(|m| m.error.is_some()).count();
        self.status = format!(
            "{} pods · {} reporting{}",
            self.pods.len(),
            reporting,
            if errs > 0 { format!(" · {errs} unreachable") } else { String::new() }
        );
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
        status: "loading…".into(),
        last_refresh: None,
        auto_refresh,
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

        // Auto-refresh when the interval elapses; otherwise poll for keys.
        let due = app
            .last_refresh
            .map(|t| t.elapsed() >= app.auto_refresh)
            .unwrap_or(true);
        if due {
            app.status = "refreshing…".into();
            terminal.draw(|f| ui(f, app))?;
            app.refresh().await;
            continue;
        }

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => {
                        app.status = "refreshing…".into();
                        terminal.draw(|f| ui(f, app))?;
                        app.refresh().await;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
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
        Some(m) if m.progress.is_some() => {
            (m.progress.clone().unwrap(), Style::default())
        }
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
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let title = Paragraph::new(format!(
        "arena-infra-rs dashboard (read-only) · provider: {} · {}   [r] refresh  [q] quit",
        app.provider_name, app.config_path
    ))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let header = Row::new(vec!["NAME", "STATUS", "GPU", "GPU%", "MEM", "TEMP", "$/HR", "PROGRESS / ERROR"])
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
            let cost = p
                .cost_per_hr
                .map(|c| format!("${c:.2}"))
                .unwrap_or_else(|| "-".into());
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
        Constraint::Length(20),
        Constraint::Length(9),
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Pods"));
    f.render_widget(table, chunks[1]);

    let age = match app.last_refresh {
        Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
        None => "never updated".into(),
    };
    let footer = Paragraph::new(format!(
        "{} · {} · auto-refresh {}s",
        app.status,
        age,
        app.auto_refresh.as_secs()
    ));
    f.render_widget(footer, chunks[2]);
}
