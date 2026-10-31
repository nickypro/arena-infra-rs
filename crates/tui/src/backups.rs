//! The backups screen (`b`, or a click on the 💾 header): which backups happen, when, and how
//! they're doing — `arena_core::backups`' status, the one `arena backups` prints. Three parts: the
//! schedule (each job, how often, its last run and result, when it's next due — or what sets it
//! up), what's running now, and the pods (failing first, then the longest since a good backup;
//! `o` orders them otherwise) with where the copies live under them. Enter on a pod opens its
//! history, which offers "back up now" (`b`: the action menu's git backup, its confirm and all)
//! and "copy home now" (`h`: `arena pods pull <pod>` in the runs panel, after a confirm); either
//! comes back here when it's done.
//!
//! The status is read off the UI thread (the crontab, the run record, the backup dir, the
//! archive's state, the locks — local files only, never a pod), when the screen opens and every
//! [`EVERY`] while it's open; the demo's is made up (`demo::backups`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arena_core::backups::{self, render as words, Health, PodIn, PodStatus, Status};
use arena_core::provider::Provider;
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::state::{Action, Tone};
use crate::{centered_rect, demo, tone_style, Mode, Shared, Ui};

/// How often the screen reads the status again while it's open.
pub(crate) const EVERY: Duration = Duration::from_secs(10);

/// How the pods are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Order {
    /// Failing first, then the longest since a good backup (the status's own order).
    #[default]
    Failing,
    /// The longest since a good backup first, whatever else.
    Oldest,
    /// By name.
    Name,
}

impl Order {
    fn next(self) -> Order {
        match self {
            Order::Failing => Order::Oldest,
            Order::Oldest => Order::Name,
            Order::Name => Order::Failing,
        }
    }

    fn words(self) -> &'static str {
        match self {
            Order::Failing => "failing first, then oldest",
            Order::Oldest => "oldest first",
            Order::Name => "by name",
        }
    }
}

/// The screen as the operator left it: the cursor's pod, the order, a pod's history open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct View {
    pub pod: Option<String>,
    pub order: Order,
    pub history: bool,
    pub scroll: u16,
}

/// The screen's state that outlives a mode: where to come back after an action it started, the
/// status generation last drawn, and where the 💾 header was drawn (a click on it opens this).
#[derive(Debug, Default)]
pub(crate) struct State {
    pub back_to: Option<View>,
    pub seen: u64,
    pub saved_at: std::cell::Cell<Option<(u16, u16)>>,
    /// The pods table as last drawn (area, scroll offset): a click picks a row.
    pub table: std::cell::Cell<(Rect, usize)>,
}

/// The status as last read, in `Shared`.
#[derive(Debug, Default)]
pub(crate) struct Landed {
    pub status: Option<Status>,
    pub at: Option<Instant>,
    pub reading: bool,
    /// Bumped as each status lands: the loop draws again.
    pub generation: u64,
    /// Read again at the next tick (a key asked).
    pub wanted: bool,
}

/// Open the screen and read the status at once.
pub(crate) fn open(ui: &mut Ui, shared: &Arc<Mutex<Shared>>) {
    ui.mode = Mode::Backups(View::default());
    shared.lock().unwrap().backups.wanted = true;
}

/// The pods the status is about: the dashboard's rows, with what it knows of each.
pub(crate) fn pods_in(s: &Shared, cfg: &arena_core::Config) -> Vec<PodIn> {
    let naming = arena_core::selector::Naming::from_config(cfg);
    // As `arena backups` has them (`listed_pods`), with the repo's last commit each pod reported.
    // A commit older than the machine (a fresh box's course repo) isn't a save on it.
    let saved = |p: &arena_core::Pod| s.metrics.get(&p.name).and_then(|m| arena_core::metrics::saved_on_machine(m.last_commit, p.created_at));
    backups::listed_pods(&s.pods, &naming, |p| saved(p).and_then(|t| u64::try_from(t).ok()))
}

/// Each pass of the loop: read the status again when the screen is open and it's due (or asked
/// for), in the background; `true` when a new one landed since the last frame.
pub(crate) fn tick(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, now: Instant) -> bool {
    let open = matches!(ui.mode, Mode::Backups(_));
    let mut s = shared.lock().unwrap();
    let landed = s.backups.generation != ui.backups.seen;
    ui.backups.seen = s.backups.generation;
    let due = s.backups.wanted || s.backups.at.is_none_or(|t| now.duration_since(t) >= EVERY);
    if !open || s.backups.reading || !due {
        return landed;
    }
    s.backups.wanted = false;
    let pods = pods_in(&s, &ui.cfg);
    // The configured names no row has, for the line under the pods (as `arena backups` says it) —
    // only from a whole listing: a provider that failed would read as names without machines.
    let without = if s.snap.partial.is_empty() { backups::without_machine(&ui.cfg, &pods) } else { Vec::new() };
    if ui.demo {
        let mut status = demo::backups(&ui.cfg, &pods, arena_core::snapshot::unix_now());
        status.without_machine = without;
        put(&mut s, status, now);
        ui.backups.seen = s.backups.generation;
        return true;
    }
    s.backups.reading = true;
    drop(s);
    let (cfg, config, shared) = (ui.cfg.clone(), ui.config_path.clone(), shared.clone());
    tokio::spawn(async move {
        let read = tokio::task::spawn_blocking(move || {
            let now = arena_core::snapshot::unix_now();
            let mut status = backups::judge(&backups::read(&cfg, Some(&config), backups::read_crontab(), pods, now));
            status.without_machine = without;
            status
        })
        .await;
        let mut s = shared.lock().unwrap();
        s.backups.reading = false;
        match read {
            Ok(status) => put(&mut s, status, Instant::now()),
            Err(e) => s.notify(format!("✗ couldn't read the backups' status: {e}"), true),
        }
    });
    landed
}

fn put(s: &mut Shared, status: Status, at: Instant) {
    s.backups.status = Some(status);
    s.backups.at = Some(at);
    s.backups.generation += 1;
}

/// The pods in the screen's order.
fn ordered(status: &Status, order: Order) -> Vec<&PodStatus> {
    let mut v: Vec<&PodStatus> = status.pods.iter().collect();
    match order {
        Order::Failing => {}
        Order::Oldest => v.sort_by(|a, b| (a.oldest, &a.name).cmp(&(b.oldest, &b.name))),
        Order::Name => v.sort_by(|a, b| a.name.cmp(&b.name)),
    }
    v
}

/// The cursor's row in `pods` (the first when its pod is gone).
fn cursor(pods: &[&PodStatus], view: &View) -> usize {
    view.pod.as_ref().and_then(|n| pods.iter().position(|p| p.name == *n)).unwrap_or(0)
}

/// The screen's keys — and `b` from the list or the detail view, which opens it. `false`: not
/// ours.
pub(crate) fn key(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, provider: &Arc<dyn Provider>, code: KeyCode) -> bool {
    let mut view = match &ui.mode {
        Mode::List | Mode::Detail if code == KeyCode::Char('b') => {
            open(ui, shared);
            return true;
        }
        Mode::Backups(v) => v.clone(),
        _ => return false,
    };
    let names: Vec<String> = {
        let s = shared.lock().unwrap();
        s.backups.status.as_ref().map(|st| ordered(st, view.order).iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
    };
    let at = view.pod.as_ref().and_then(|n| names.iter().position(|x| x == n)).unwrap_or(0);
    let pick = |i: usize| names.get(i).cloned();
    if view.history {
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => view.history = false,
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.scroll = crate::scroll_by(view.scroll, ui.result_max.get(), 1),
            KeyCode::Char('b') => return act(ui, shared, provider, view, Action::Backup),
            KeyCode::Char('h') => return act(ui, shared, provider, view, Action::Pull),
            _ => {}
        }
        ui.mode = Mode::Backups(view);
        return true;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 'b') => {
            ui.mode = Mode::List;
            return true;
        }
        KeyCode::Up | KeyCode::Char('k') => view.pod = pick(at.saturating_sub(1)).or(view.pod),
        KeyCode::Down | KeyCode::Char('j') => view.pod = pick((at + 1).min(names.len().saturating_sub(1))).or(view.pod),
        KeyCode::Home | KeyCode::Char('g') => view.pod = pick(0),
        KeyCode::End | KeyCode::Char('G') => view.pod = pick(names.len().saturating_sub(1)),
        KeyCode::Enter if !names.is_empty() => {
            view.pod = pick(at);
            view.history = true;
            view.scroll = 0;
        }
        KeyCode::Char('o') => view.order = view.order.next(),
        KeyCode::Char('r') => shared.lock().unwrap().backups.wanted = true,
        _ => {}
    }
    ui.mode = Mode::Backups(view);
    true
}

/// The mouse on the screen: the wheel moves the cursor (or scrolls a history), a click picks a
/// pod's row — a click on the picked row opens its history. `true`: something changed.
pub(crate) fn mouse(m: crossterm::event::MouseEvent, ui: &mut Ui, shared: &Arc<Mutex<Shared>>) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    let Mode::Backups(mut view) = ui.mode.clone() else { return false };
    let names: Vec<String> = {
        let s = shared.lock().unwrap();
        s.backups.status.as_ref().map(|st| ordered(st, view.order).iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
    };
    let at = view.pod.as_ref().and_then(|n| names.iter().position(|x| x == n)).unwrap_or(0);
    match (m.kind, view.history) {
        (MouseEventKind::ScrollUp, true) => view.scroll = view.scroll.saturating_sub(1),
        (MouseEventKind::ScrollDown, true) => view.scroll = crate::scroll_by(view.scroll, ui.result_max.get(), 1),
        (MouseEventKind::ScrollUp, false) => view.pod = names.get(at.saturating_sub(1)).cloned().or(view.pod),
        (MouseEventKind::ScrollDown, false) => view.pod = names.get((at + 1).min(names.len().saturating_sub(1))).cloned().or(view.pod),
        (MouseEventKind::Down(MouseButton::Left), false) => {
            let (area, offset) = ui.backups.table.get();
            // Rows start under the header.
            let top = area.y.saturating_add(1);
            if m.row < top || m.row >= area.y.saturating_add(area.height) || m.column < area.x || m.column >= area.x.saturating_add(area.width) {
                return false;
            }
            let Some(name) = names.get(offset + (m.row - top) as usize).cloned() else { return false };
            if view.pod.as_deref() == Some(name.as_str()) || (view.pod.is_none() && offset + ((m.row - top) as usize) == 0) {
                view.history = true;
                view.scroll = 0;
            }
            view.pod = Some(name);
        }
        _ => return false,
    }
    ui.mode = Mode::Backups(view);
    true
}

/// "Back up now" / "copy home now" on the history's pod: the dashboard's own action — its
/// confirm, then its work or its run — with the cursor on that pod, coming back here after.
fn act(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, provider: &Arc<dyn Provider>, view: View, action: Action) -> bool {
    if ui.demo {
        shared.lock().unwrap().notify("demo: the pods are made up — nothing to back up", true);
        ui.mode = Mode::Backups(view);
        return true;
    }
    let Some(name) = view.pod.clone() else { return true };
    let row = shared.lock().unwrap().pods.iter().position(|p| p.name == name);
    let Some(row) = row else {
        shared.lock().unwrap().notify(format!("✗ {name} isn't in the dashboard's listing (stopped, gone, or another provider's) — `arena pods backup {name}`"), true);
        ui.mode = Mode::Backups(view);
        return true;
    };
    ui.selected = row;
    ui.backups.back_to = Some(view);
    crate::choose_pod_action(shared, ui, provider, action);
    true
}

/// After a key: an action the screen started has ended (its result dismissed, its confirm
/// cancelled, its run's panel hidden) — back to the screen, as it was.
pub(crate) fn come_back(ui: &mut Ui, shared: &Arc<Mutex<Shared>>) {
    if matches!(ui.mode, Mode::List) {
        if let Some(view) = ui.backups.back_to.take() {
            ui.mode = Mode::Backups(view);
            shared.lock().unwrap().backups.wanted = true;
        }
    }
}

/// Record the dashboard's own git backup of `pod` (the action menu's backup, the screen's "back
/// up now", set-branch's backup first) in the backups' run record — the pod's result only, never
/// the job's run (that's a fleet-wide one: the cron's). Best effort and silent (the result modal
/// says what happened); never in this crate's tests unless the config names a state dir.
pub(crate) fn note_git(cfg: &arena_core::Config, pod: &str, outcome: arena_core::backups::record::Outcome, detail: String) {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return;
    }
    let Ok(path) = arena_core::snapshot::state_file_path_for(cfg, arena_core::backups::record::RECORD_FILE) else { return };
    let ev = arena_core::backups::record::Event::new(arena_core::snapshot::unix_now(), arena_core::backups::Job::Git, outcome, detail);
    let _ = arena_core::backups::record::update(&path, Duration::from_secs(2), |r| r.add_events(vec![(pod.to_string(), ev)]));
}

/// A health's colour.
fn style(h: Health) -> Style {
    match h {
        Health::Fail => tone_style(Tone::Bad),
        Health::Warn => tone_style(Tone::Warn),
        Health::Unknown | Health::Off => tone_style(Tone::Dim),
        Health::Ok => Style::default(),
    }
}

/// The footer's keys.
pub(crate) fn keys(view: &View) -> &'static str {
    if view.history {
        "[b] back up now (git)  [h] copy home now  [↑↓] scroll  [esc] back"
    } else {
        "[esc/b] close  [↑↓] move  [enter] history  [o] order  [r] read again"
    }
}

/// Draw the screen in `area` (the body: the top line and the footer stay).
pub(crate) fn render(f: &mut Frame, shared: &Shared, ui: &Ui, view: &View, area: Rect) {
    let age = shared.backups.at.map(|t| format!("read {}s ago", t.elapsed().as_secs())).unwrap_or_default();
    let reading = if shared.backups.reading { " · reading…" } else { "" };
    let block = Block::default().borders(Borders::ALL).title(format!(" 💾 Backups — what's backed up, where and when · {age}{reading} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(status) = shared.backups.status.as_ref() else {
        let text = if ui.demo { "…" } else { "reading the crontab, the backups' record, the backup dir and the archive's state…" };
        f.render_widget(Paragraph::new(text).style(tone_style(Tone::Dim)), inner);
        return;
    };
    let now = status.generated_at;
    let jobs = schedule_lines(status, now, inner.width);
    let storage = storage_lines(status);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(jobs.len() as u16),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(storage.len() as u16),
        ])
        .split(inner);
    f.render_widget(Paragraph::new(jobs), rows[0]);
    f.render_widget(Paragraph::new(running_line(status, now)), rows[1]);
    pods_table(f, status, ui, view, rows[2], now);
    f.render_widget(Paragraph::new(storage), rows[3]);
    if view.history {
        let pods = ordered(status, view.order);
        if let Some(p) = pods.get(cursor(&pods, view)) {
            history(f, ui, p, view.scroll, now);
        }
    }
}

/// The schedule: one line per job — its mark, name, how often, last run, next — and, under one
/// that isn't fine, why.
fn schedule_lines(status: &Status, now: u64, width: u16) -> Vec<Line<'static>> {
    // The last run's words get what the other columns leave (the next run's ~30 kept).
    let last_w = (width as usize).saturating_sub(2 + 21 + 37 + 30).clamp(24, 80);
    let mut out = vec![Line::styled(format!("  {:<20} {:<36} {:<last_w$} NEXT", "JOB", "HOW OFTEN", "LAST RUN"), tone_style(Tone::Dim))];
    for j in &status.jobs {
        let last = words::last_run(j, now);
        out.push(Line::from(vec![
            Span::styled(format!("{} ", j.health.mark()), style(j.health)),
            Span::styled(format!("{:<20} ", j.name), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{:<36} ", truncate(&words::how_often(j), 36))),
            Span::styled(format!("{:<last_w$} ", truncate(&last, last_w)), if j.last.as_ref().is_some_and(|l| !l.ok) { style(Health::Fail) } else { Style::default() }),
            Span::raw(words::next_run(j, now)),
        ]));
        if let Some(why) = &j.why {
            out.push(Line::styled(format!("    └ {why}"), style(j.health)));
        }
    }
    out
}

/// What's running now, in one line.
fn running_line(status: &Status, now: u64) -> Line<'static> {
    if let Some(why) = &status.running_unknown {
        return Line::styled(format!("running now: unknown ({why})"), tone_style(Tone::Dim));
    }
    if status.running.is_empty() {
        return Line::styled("running now: nothing", tone_style(Tone::Dim));
    }
    let parts: Vec<String> = status.running.iter().map(|r| r.since.map_or_else(|| r.what.clone(), |t| format!("{} since {}", r.what, backups::ago(now, t)))).collect();
    Line::styled(format!("running now: {}", parts.join(" · ")), tone_style(Tone::Busy))
}

/// Where the copies live, one line each, and what no backup takes — after the configured names
/// with no machine now, on one line (`arena backups` says the same).
fn storage_lines(status: &Status) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if !status.without_machine.is_empty() {
        let n = status.without_machine.len();
        let names = words::names_line(&status.without_machine, 8);
        out.push(Line::styled(format!("{n} configured name{} no machine now: {names}", if n == 1 { " has" } else { "s have" }), tone_style(Tone::Dim)));
    }
    out.extend(status
        .storage
        .iter()
        .map(|s| {
            Line::from(vec![
                Span::styled(format!("{} ", s.health.mark()), style(s.health)),
                Span::styled(format!("{}: ", s.what), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{} — ", s.place)),
                Span::styled(s.text.clone(), style(s.health)),
            ])
        }));
    out.extend(status.skips.iter().map(|l| Line::styled(l.clone(), style(Health::Fail))));
    out.extend(status.notes.iter().map(|n| Line::styled(format!("note: {n}"), tone_style(Tone::Warn))));
    out.push(Line::styled(format!("not covered: {}", backups::NOT_COVERED[0]), tone_style(Tone::Dim)));
    out
}

/// `21:40` today, else `10-29 21:40` (UTC, as the record keeps it).
fn at(t: u64, now: u64) -> String {
    let hm = format!("{:02}:{:02}", t % 86_400 / 3600, t % 3600 / 60);
    if t / 86_400 == now / 86_400 {
        hm
    } else {
        let (_, m, d) = arena_core::schedule::civil_from_days((t / 86_400) as i64);
        format!("{m:02}-{d:02} {hm}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// The pods: name, git, home copy, archive, why — each cell in its health's colour.
fn pods_table(f: &mut Frame, status: &Status, ui: &Ui, view: &View, area: Rect, now: u64) {
    let pods = ordered(status, view.order);
    let name_w = pods.iter().map(|p| ui.shown_name(&p.name).chars().count() + 2).max().unwrap_or(4).max(5);
    let rows: Vec<Row> = pods
        .iter()
        .map(|p| {
            let why = match (p.running, p.why.clone().filter(|_| p.health != Health::Ok)) {
                (true, Some(w)) => Span::styled(format!("backing up now · {w}"), tone_style(Tone::Busy)),
                (true, None) => Span::styled("backing up now", tone_style(Tone::Busy)),
                (false, w) => Span::styled(w.unwrap_or_default(), style(p.health)),
            };
            Row::new(vec![
                Cell::from(Span::styled(format!("{} {}", p.health.mark(), ui.shown_name(&p.name)), style(p.health))),
                Cell::from(Span::styled(words::cell(&p.git, now), style(p.git.health))),
                Cell::from(Span::styled(words::cell(&p.copy, now), style(p.copy.health))),
                Cell::from(Span::styled(words::cell(&p.archive, now), style(p.archive.health))),
                Cell::from(why),
            ])
        })
        .collect();
    let header = Row::new(vec!["  POD".to_string(), "GIT".into(), "HOME COPY".into(), "ARCHIVE".into(), format!("WHY  ({}: o)", view.order.words())])
    .style(tone_style(Tone::Dim));
    // A cell's longest: `✗ failed 13m (ok 58m)`, `✓ 3.6 GB · w3d5 12m`, `✓ w3d5 + all files 33m`.
    let widths = [Constraint::Length(name_w.max(14) as u16 + 2), Constraint::Length(23), Constraint::Length(24), Constraint::Length(24), Constraint::Min(10)];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    let mut ts = TableState::default();
    if !pods.is_empty() {
        ts.select(Some(cursor(&pods, view)));
    }
    f.render_stateful_widget(table, area, &mut ts);
    ui.backups.table.set((area, ts.offset()));
}

/// A pod's history, over the screen: each backup's last result, then what the record holds
/// (newest first), then what it offers.
fn history(f: &mut Frame, ui: &Ui, p: &PodStatus, scroll: u16, now: u64) {
    let area = centered_rect(84, 76, f.area());
    f.render_widget(Clear, area);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (what, c) in [("git", &p.git), ("home copy", &p.copy), ("archive", &p.archive)] {
        let ok = match c.ok_at {
            Some(t) => format!(" · last good {}", backups::ago(now, t)),
            None if c.health != Health::Off => " · never good on record".into(),
            None => String::new(),
        };
        let why = c.why.as_ref().map(|w| format!(" — {w}")).unwrap_or_default();
        lines.push(Line::from(vec![Span::styled(format!("{what:<10} "), bold), Span::styled(format!("{}{ok}{why}", words::cell(c, now)), style(c.health))]));
    }
    if let Some(l) = &p.label {
        lines.push(Line::raw(format!("{:<10} newest snapshot here: {l} (`arena pods restore {}` puts it back)", "", p.name)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("history (newest first)", bold));
    if p.history.is_empty() {
        lines.push(Line::styled("  none recorded yet — the record starts with this arena version's first backup of it", tone_style(Tone::Dim)));
    }
    for e in &p.history {
        let when = match e.since {
            Some(s) => format!("{} ×{} since {}", at(e.at, now), e.times, at(s, now)),
            None => at(e.at, now),
        };
        let h = if e.outcome.ok() { Health::Ok } else if e.outcome == arena_core::backups::record::Outcome::Failed { Health::Fail } else { Health::Warn };
        lines.push(Line::from(vec![
            Span::raw(format!("  {when:<28} ")),
            Span::styled(format!("{:<10} {:<17} ", e.job.name(), e.outcome.words()), style(h)),
            Span::raw(e.detail.clone()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("[b] back up now (git: commit + push its branch)   [h] copy home now (`arena pods pull`)   [esc] back", tone_style(Tone::Busy)));
    let visible = area.height.saturating_sub(2);
    ui.result_max.set((lines.len() as u16).saturating_sub(visible));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(ui.result_max.get()), 0))
            .block(Block::default().borders(Borders::ALL).title(format!(" {} — backups ", p.name))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{cfg, draw, ui};
    use arena_core::remote::{FakeRemote, FakeReply};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    /// The demo's fleet, published, and its backups' status read (as the screen's first tick does).
    fn demo_world(n: usize) -> (Arc<Mutex<Shared>>, Ui) {
        let conf = cfg("");
        let shared = Arc::new(Mutex::new(Shared::default()));
        demo::publish(&shared, &conf, n, 0);
        let mut ui = ui(conf, Arc::new(FakeRemote::new()));
        ui.demo = true;
        (shared, ui)
    }

    fn provider() -> Arc<dyn Provider> {
        Arc::new(demo::NoProvider)
    }

    fn view(ui: &Ui) -> View {
        match &ui.mode {
            Mode::Backups(v) => v.clone(),
            m => panic!("not the backups screen: {m:?}"),
        }
    }

    /// `b` opens the screen, its first tick reads the status (the demo's, made up), and it draws
    /// the three parts: each job's schedule with its last run and next one, what's running now,
    /// the pods failing first with their reasons — then where the copies live.
    #[test]
    fn the_screen_draws_the_demo_fleets_backups() {
        let (shared, mut ui) = demo_world(14);
        assert!(key(&mut ui, &shared, &provider(), KeyCode::Char('b')));
        assert_eq!(view(&ui), View::default());
        assert!(tick(&mut ui, &shared, Instant::now()), "the status landed");
        let s = shared.lock().unwrap();
        let lines = draw(170, 46, &s, &ui).join("\n");
        for want in [
            "Backups — what's backed up, where and when",
            "✗ git backup",
            "every 15 min",
            "with each git backup (every 15 min)",
            "archive: snapshots",
            "hourly at :07 :22 :37 :52",
            "hourly at :40",
            "└ 1 failed: devtest-demo04",
            "running now: home copy since 1 min ago · backing up now: devtest-demo11, devtest-demo12",
            "WHY  (failing first, then oldest: o)",
            "✗ demo04",
            "✗ failed",
            "git: exit 255: ssh: connect to host 10.9.0.4 port 22",
            "⚠ on main",
            "backing up now",
            "all files: /mnt/big/big — 8 GB free of 500 GB on /mnt/big — under twice the floor",
            "git: github.com/example/ARENA_3.0",
            // The test config's list (alpha..delta) has no demo machine: one line, as `arena backups`.
            "4 configured names have no machine now: devtest-alpha, devtest-bravo, devtest-charlie, devtest-delta",
            "[esc/b] close  [↑↓] move  [enter] history  [o] order  [r] read again",
        ] {
            assert!(lines.contains(want), "no {want:?} in\n{lines}");
        }
        // Failing first — demo06 (its archive copy), demo04 (its push and copy), demo09 (its copy) —
        // then the pods on main.
        let rows: Vec<&str> = lines.lines().filter(|l| l.contains(" demo")).collect();
        assert!(rows[0].contains("✗ demo06") && rows[1].contains("✗ demo04") && rows[2].contains("✗ demo09") && rows[3].contains("⚠ demo02"), "{rows:#?}");
    }

    /// Not a test: the screen as the README and the PR show it. `cargo test -p arena-tui --
    /// --ignored --nocapture backups_picture`.
    #[test]
    #[ignore = "prints the backups screen"]
    fn backups_picture() {
        let (shared, mut ui) = demo_world(12);
        key(&mut ui, &shared, &provider(), KeyCode::Char('b'));
        tick(&mut ui, &shared, Instant::now());
        let s = shared.lock().unwrap();
        println!("{}", draw(160, 40, &s, &ui).join("\n"));
        drop(s);
        key(&mut ui, &shared, &provider(), KeyCode::Enter);
        let s = shared.lock().unwrap();
        println!("{}", draw(160, 40, &s, &ui).join("\n"));
    }

    /// The screen's keys: the cursor moves by pod (kept across a re-read), `o` cycles the order,
    /// enter opens the pod's history and esc closes it, esc (or `b`) closes the screen.
    #[test]
    fn the_screens_keys_move_order_and_open_a_history() {
        let (shared, mut ui) = demo_world(14);
        let p = provider();
        key(&mut ui, &shared, &p, KeyCode::Char('b'));
        tick(&mut ui, &shared, Instant::now());
        let first = shared.lock().unwrap().backups.status.as_ref().unwrap().pods[0].name.clone();
        key(&mut ui, &shared, &p, KeyCode::Down);
        let second = shared.lock().unwrap().backups.status.as_ref().unwrap().pods[1].name.clone();
        assert_eq!(view(&ui).pod.as_deref(), Some(second.as_str()));
        key(&mut ui, &shared, &p, KeyCode::Up);
        assert_eq!(view(&ui).pod.as_deref(), Some(first.as_str()));
        for want in [Order::Oldest, Order::Name, Order::Failing] {
            key(&mut ui, &shared, &p, KeyCode::Char('o'));
            assert_eq!(view(&ui).order, want);
        }
        key(&mut ui, &shared, &p, KeyCode::Enter);
        assert!(view(&ui).history);
        let lines = draw(170, 46, &shared.lock().unwrap(), &ui).join("\n");
        let title = format!(" {first} — backups ");
        for want in [title.as_str(), "history (newest first)", "[b] back up now (git: commit + push its branch)", "[h] copy home now"] {
            assert!(lines.contains(want), "no {want:?} in\n{lines}");
        }
        assert_eq!(crate::mode_keys(&ui.mode), "[b] back up now (git)  [h] copy home now  [↑↓] scroll  [esc] back");
        // In the demo the actions are refused, said in the footer.
        key(&mut ui, &shared, &p, KeyCode::Char('b'));
        assert!(view(&ui).history);
        assert!(shared.lock().unwrap().notice.as_ref().is_some_and(|n| n.text.starts_with("demo:")));
        key(&mut ui, &shared, &p, KeyCode::Esc);
        assert!(!view(&ui).history);
        key(&mut ui, &shared, &p, KeyCode::Char('b'));
        assert!(matches!(ui.mode, Mode::List));
        // Not ours elsewhere: `b` in a popup is that popup's.
        ui.mode = Mode::Info { scroll: 0, detail: false };
        assert!(!key(&mut ui, &shared, &p, KeyCode::Char('b')));
    }

    /// "Back up now" and "copy home now" are the dashboard's own actions on that pod — the git
    /// backup's confirm (with its command a `v` away), `arena pods pull <pod> --yes` after one —
    /// and the screen comes back, as it was, once the action's result or confirm is closed.
    #[test]
    fn back_up_now_and_copy_home_now_are_the_dashboards_actions_and_come_back() {
        let (shared, mut ui) = demo_world(14);
        ui.demo = false;
        let p = provider();
        let status = demo::backups(&ui.cfg, &pods_in(&shared.lock().unwrap(), &ui.cfg), arena_core::snapshot::unix_now());
        shared.lock().unwrap().backups.status = Some(status);
        let pod = "devtest-demo07".to_string();
        let open_history = |ui: &mut Ui| ui.mode = Mode::Backups(View { pod: Some(pod.clone()), history: true, ..View::default() });
        open_history(&mut ui);
        key(&mut ui, &shared, &p, KeyCode::Char('b'));
        let Mode::Confirm(c) = &ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!((c.action, c.pod_name.as_str()), (Action::Backup, "demo07"));
        assert!(c.preview.as_deref().is_some_and(|v| v.contains("arena-backup.sh")), "{:?}", c.preview);
        assert_eq!(shared.lock().unwrap().pods[ui.selected].name, pod);
        // Its confirm cancelled (the main loop's esc): back to the history.
        ui.mode = Mode::List;
        come_back(&mut ui, &shared);
        assert_eq!(view(&ui), View { pod: Some(pod.clone()), history: true, ..View::default() });
        assert!(shared.lock().unwrap().backups.wanted, "and read again");
        come_back(&mut ui, &shared);
        key(&mut ui, &shared, &p, KeyCode::Char('h'));
        let Mode::Confirm(c) = &ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!(c.action, Action::Pull);
        assert_eq!(crate::cli_action_args(Action::Pull, &pod, None).map(|(_, a)| a), Some(vec!["pods".to_string(), "pull".into(), pod.clone(), "--yes".into()]));
        assert!(Action::Pull.cli() && !Action::MENU.iter().any(|(_, a)| *a == Action::Pull), "the screen's own, in no menu");
        // A pod the dashboard doesn't list can't be acted on from here: said, the screen stays.
        ui.mode = Mode::Backups(View { pod: Some("devtest-gone".into()), history: true, ..View::default() });
        key(&mut ui, &shared, &p, KeyCode::Char('b'));
        assert!(matches!(ui.mode, Mode::Backups(_)));
        assert!(shared.lock().unwrap().notice.as_ref().is_some_and(|n| n.text.contains("isn't in the dashboard's listing")));
    }

    /// A click on the 💾 header opens the screen; on the screen a click picks a pod, a second
    /// click on it opens its history, the wheel moves.
    #[test]
    fn a_click_on_the_saved_header_opens_the_screen() {
        let (shared, mut ui) = demo_world(6);
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        let lines = draw(200, 20, &shared.lock().unwrap(), &ui);
        let (x, y) = ui.backups.saved_at.get().expect("the 💾 header was drawn");
        assert!(lines[y as usize].contains(crate::columns::SAVED_HEADER), "{}", lines[y as usize]);
        assert!(crate::handle_mouse(click(x, y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Backups(_)));
        tick(&mut ui, &shared, Instant::now());
        let _ = draw(200, 40, &shared.lock().unwrap(), &ui);
        let (area, _) = ui.backups.table.get();
        let names: Vec<String> = shared.lock().unwrap().backups.status.as_ref().unwrap().pods.iter().map(|p| p.name.clone()).collect();
        assert!(crate::handle_mouse(click(area.x + 3, area.y + 2), &mut ui, &shared));
        assert_eq!(view(&ui).pod.as_deref(), Some(names[1].as_str()));
        assert!(!view(&ui).history);
        assert!(crate::handle_mouse(click(area.x + 3, area.y + 2), &mut ui, &shared));
        assert!(view(&ui).history, "a second click opens its history");
        ui.mode = Mode::Backups(View::default());
        let wheel = MouseEvent { kind: MouseEventKind::ScrollDown, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        assert!(crate::handle_mouse(wheel, &mut ui, &shared));
        assert_eq!(view(&ui).pod.as_deref(), Some(names[1].as_str()));
    }

    /// The dashboard's own git backup is recorded for the screen and `arena backups` — the pod's
    /// result, never the job's run (that's the cron's).
    #[tokio::test]
    async fn the_dashboards_backup_is_recorded_as_the_pods() {
        let dir = std::env::temp_dir().join(format!("arena-tui-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let conf = cfg(&format!("ARENA_STATE_DIR={}\nLOCAL_BACKUP_DIR={}/backup\n", dir.display(), dir.display()));
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        let pod = arena_core::Pod {
            id: "rp1".into(),
            name: "devtest-alpha".into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(22001),
            ..Default::default()
        };
        let (line, _) = crate::backup_and_report(fake.as_ref(), &conf, &pod).await;
        assert!(line.starts_with("✓ backed up devtest-alpha → autocommit-a"), "{line}");
        let rec = arena_core::backups::record::Record::load(&dir.join("devtest").join("backups.json")).unwrap().unwrap();
        let e = rec.pods["devtest-alpha"].git.as_ref().unwrap();
        assert_eq!((e.outcome, e.detail.as_str()), (arena_core::backups::record::Outcome::Pushed, "→ autocommit-a"));
        assert!(rec.runs.is_empty(), "never the job's run");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
