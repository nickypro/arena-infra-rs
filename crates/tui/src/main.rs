//! `arena-tui` — the interactive dashboard. Read-only: it lists pods from the
//! configured provider and, for each pod with an SSH endpoint, fetches GPU stats
//! (`nvidia-smi`) and an optional operator-defined progress signal (`PROGRESS_CMD`)
//! over SSH. `r` refreshes, `q`/Esc quits. No mutating actions — spin-up/backup stay
//! in the CLI where they're explicitly gated.
//!
//! Provider is chosen by `ARENA_PROVIDER` (default `runpod`); config path by
//! `ARENA_CONFIG`. Metrics for the whole fleet are fetched concurrently so one slow
//! or down pod doesn't stall the others.

use std::collections::HashMap;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::time::Duration;

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
    cfg: Config,
    progress_cmd: Option<String>,
    pods: Vec<Pod>,
    metrics: HashMap<String, PodMetrics>,
    status: String,
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
        self.status = format!(
            "{} pods · {} reporting metrics · [r] refresh  [q] quit",
            self.pods.len(),
            self.metrics.values().filter(|m| !m.gpus.is_empty()).count(),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = PathBuf::from(
        std::env::var("ARENA_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
    );
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let provider_name = std::env::var("ARENA_PROVIDER").unwrap_or_else(|_| "runpod".to_string());
    let provider = arena_core::provider::build(&provider_name, &cfg)?;
    let progress_cmd = cfg.get("PROGRESS_CMD").map(String::from);

    let mut app = App {
        provider,
        cfg,
        progress_cmd,
        pods: Vec::new(),
        metrics: HashMap::new(),
        status: "loading…".into(),
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
        if event::poll(Duration::from_millis(200))? {
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

/// Format one pod's metric cells: (util, mem, temp, progress), each falling back to
/// "-" when unknown, or "err" for the util cell when the fetch failed outright.
fn metric_cells(m: Option<&PodMetrics>) -> (String, String, String, String) {
    let Some(m) = m else {
        return ("-".into(), "-".into(), "-".into(), "-".into());
    };
    let util = match (m.mean_util(), &m.error) {
        (Some(u), _) => format!("{u}%"),
        (None, Some(_)) => "err".into(),
        (None, None) => "-".into(),
    };
    let mem = match m.mem_summary() {
        Some((used, total)) => format!("{:.0}/{:.0}G", used as f64 / 1024.0, total as f64 / 1024.0),
        None => "-".into(),
    };
    let temp = m.max_temp().map(|t| format!("{t}C")).unwrap_or_else(|| "-".into());
    let progress = m.progress.clone().unwrap_or_else(|| "-".into());
    (util, mem, temp, progress)
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let title = Paragraph::new("arena-infra-rs — dashboard (read-only)   [r] refresh   [q] quit")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let header = Row::new(vec!["NAME", "STATUS", "GPU", "GPU%", "MEM", "TEMP", "PROGRESS"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .pods
        .iter()
        .map(|p| {
            let (util, mem, temp, progress) = metric_cells(app.metrics.get(&p.name));
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(p.status.clone()),
                Cell::from(p.gpu_type.clone().unwrap_or_else(|| "-".into())),
                Cell::from(util),
                Cell::from(mem),
                Cell::from(temp),
                Cell::from(progress),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(22),
        Constraint::Length(10),
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Length(12),
        Constraint::Length(6),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Pods"));
    f.render_widget(table, chunks[1]);

    let footer = Paragraph::new(app.status.clone());
    f.render_widget(footer, chunks[2]);
}
