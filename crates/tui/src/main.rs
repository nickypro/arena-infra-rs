//! `arena-tui` — the interactive surface. This first cut is a read-only pod
//! dashboard: it fetches the pod list once and renders it in a table. `r` refetches,
//! `q`/Esc quits. It deliberately has *no* mutating actions yet — wiring create/stop
//! into the TUI comes after the lifecycle vertical is trusted from the CLI.

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

use arena_core::provider::{runpod::RunpodProvider, Provider};
use arena_core::{Config, Pod};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

struct App {
    provider: RunpodProvider,
    pods: Vec<Pod>,
    status: String,
}

impl App {
    async fn refresh(&mut self) {
        match self.provider.list_pods().await {
            Ok(pods) => {
                self.status = format!("{} pods", pods.len());
                self.pods = pods;
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = PathBuf::from(
        std::env::var("ARENA_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
    );
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let key = cfg.require("RUNPOD_API_KEY")?.to_string();

    let mut app = App {
        provider: RunpodProvider::new(key),
        pods: Vec::new(),
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

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let title = Paragraph::new("arena-infra-rs — pods (read-only)   [r] refresh   [q] quit")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let header = Row::new(vec!["NAME", "STATUS", "GPU", "IP", "PORT"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .pods
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(p.status.clone()),
                Cell::from(p.gpu_type.clone().unwrap_or_else(|| "-".into())),
                Cell::from(p.ssh_ip.clone().unwrap_or_else(|| "-".into())),
                Cell::from(
                    p.ssh_port
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(24),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Pods"));
    f.render_widget(table, chunks[1]);

    let footer = Paragraph::new(app.status.clone());
    f.render_widget(footer, chunks[2]);
}
