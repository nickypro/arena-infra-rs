//! The pods table, and its cells' words and colours.

use arena_core::metrics::PodMetrics;
use arena_core::openrouter::KeyUsage;
use arena_core::status::pod_status;
use arena_core::{fleet, Pod};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};

use crate::state::{self, display_name, health_cell, mark_key, proxy_cell, rtt_cell, short_branch, spark, Tone};
use crate::{columns, Shared, Ui};

pub(crate) fn temp_style(t: Option<u32>) -> Style {
    match t {
        Some(t) if t >= 85 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(t) if t >= 75 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

pub(crate) fn util_style(u: Option<u32>, err: bool) -> Style {
    if err {
        return Style::default().fg(Color::Red);
    }
    match u {
        Some(0) => Style::default().fg(Color::DarkGray), // idle GPU — worth noticing
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

/// Style a fullness percentage (RAM / disk): plain until ~90%, yellow when nearly full,
/// red when critically full — so a pod about to run out of space/RAM stands out.
pub(crate) fn capacity_style(pct: Option<u32>) -> Style {
    match pct {
        Some(p) if p >= 95 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(p) if p >= 90 => Style::default().fg(Color::Yellow),
        None => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// Truncate a string to `max` display columns, adding an ellipsis if cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A one-letter, color-coded provider badge: R=runpod, V=vast, H=hetzner, S=shadeform.
fn provider_cell(provider: &str) -> Cell<'static> {
    let (letter, color) = match provider {
        "runpod" => ("R", Color::Cyan),
        "vast" => ("V", Color::Magenta),
        "hetzner" => ("H", Color::Yellow),
        "shadeform" => ("S", Color::Green),
        other => (other.get(0..1).unwrap_or("?"), Color::Gray),
    };
    Cell::from(Span::styled(
        letter.to_uppercase(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

/// The compact setup cluster (SET): four glyphs for `~/.name`, the deploy key, the git
/// origin→GitHub, and an API key — `.name`/key/origin/api. ✓ green / ✗ red / · gray.
/// (Detail pane spells them out. Not to be confused with HEALTH, the deep check.)
fn setup_cell(m: Option<&PodMetrics>) -> Cell<'static> {
    let glyph = |ok: Option<bool>| match ok {
        Some(true) => Span::styled("✓", Style::default().fg(Color::Green)),
        Some(false) => Span::styled("✗", Style::default().fg(Color::Red)),
        None => Span::styled("·", Style::default().fg(Color::DarkGray)),
    };
    match m {
        Some(m) if m.error.is_none() => {
            let origin_ok = m.origin.as_deref().map(|o| o.contains("github.com"));
            Cell::from(Line::from(vec![
                glyph(m.has_name),
                glyph(m.has_key),
                glyph(origin_ok),
                glyph(m.has_api_key),
            ]))
        }
        _ => Cell::from(Span::styled("····", Style::default().fg(Color::DarkGray))),
    }
}

/// A glyph + style marking how the local branch diverges from its upstream:
/// `↑` ahead (committed but unpushed — e.g. a blocked push), `↓` behind, `⇕` diverged,
/// or none when in sync / no upstream / unreachable.
pub(crate) fn sync_marker(m: Option<&PodMetrics>) -> (&'static str, Style) {
    match m {
        Some(m) if m.error.is_none() => match (m.ahead.unwrap_or(0), m.behind.unwrap_or(0)) {
            (a, b) if a > 0 && b > 0 => ("⇕", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            (a, _) if a > 0 => ("↑", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            (_, b) if b > 0 => ("↓", Style::default().fg(Color::Cyan)),
            _ => ("", Style::default()),
        },
        _ => ("", Style::default()),
    }
}

/// A human description of branch-vs-origin sync, for the detail pane.
pub(crate) fn sync_summary(m: Option<&PodMetrics>) -> String {
    let Some(m) = m else { return "-".into() };
    if m.error.is_some() {
        return "-".into();
    }
    match (m.ahead, m.behind) {
        (Some(a), Some(b)) if a > 0 && b > 0 => {
            format!("⇕ diverged — {a} ahead, {b} behind origin (push blocked?)")
        }
        (Some(a), _) if a > 0 => format!("↑ {a} commit(s) NOT on origin — push blocked/failed?"),
        (_, Some(b)) if b > 0 => format!("↓ {b} commit(s) behind origin"),
        (Some(_), Some(_)) => "✓ in sync with origin".into(),
        _ => "· no upstream (branch never pushed)".into(),
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

/// A [`Tone`] as a colour.
pub(crate) fn tone_style(t: Tone) -> Style {
    match t {
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Warn => Style::default().fg(Color::Yellow),
        Tone::Bad => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Tone::Busy => Style::default().fg(Color::Cyan),
        Tone::Dim => Style::default().fg(Color::DarkGray),
    }
}

/// A pod's `AI$` cell: its OpenRouter key's spend against the cap (`-`: no key named after
/// it), yellow from 80% of the cap, red at the cap or disabled, a grey `paused` for a $0 cap
/// nothing was spent against — grey when the last listing failed (`stale`: these are older
/// numbers, the summary bar says `(n/a)`).
fn ai_cell(k: Option<&KeyUsage>, stale: bool, read: bool) -> Cell<'static> {
    match k {
        // Before the first read lands: not known yet, rather than "no key".
        None if !read => Cell::from("…").style(tone_style(Tone::Dim)),
        None => Cell::from("-").style(tone_style(Tone::Dim)),
        Some(k) if stale || k.paused() => Cell::from(k.cell()).style(tone_style(Tone::Dim)),
        Some(k) => {
            let style = state::fill_tone(k.fill()).map(tone_style).unwrap_or_default();
            Cell::from(k.cell()).style(style)
        }
    }
}

/// The GPU column: the probe's count and card (`2×RTX A4000`) — the provider's when it wasn't
/// read (`fleet::gpu_label`) — with each GPU's VRAM (`16G`); `RTX ` dropped when `narrow`. A MIG
/// slice is named as every surface names it, `1×B300 MIG 34G`: nvidia-smi names only the parent
/// card under MIG (`B300 SXM6 AC`, its memory unreadable, one row for two slices of it), so the
/// slice's name, memory and count come from the provider (`fleet::mig_slice`) — the probe's
/// rows only when the provider gave no count (review ai-review codex r1 #1). Pure.
pub(crate) fn gpu_text(p: &Pod, m: Option<&PodMetrics>, narrow: bool) -> String {
    let probed = m.map(|m| m.gpus.len()).filter(|&n| n > 0);
    if let Some((model, gb)) = p.gpu_type.as_deref().and_then(fleet::mig_slice) {
        let n = p.gpu_count.filter(|&n| n > 0).map(|n| n as usize).or(probed).unwrap_or(1);
        return format!("{n}×{model} MIG {gb}G");
    }
    let mut gpu = m.and_then(|m| m.gpu_summary()).unwrap_or_else(|| fleet::gpu_label(p));
    if narrow {
        gpu = gpu.replace("RTX ", "");
    }
    if gpu != "-" {
        if let Some(g) = m.and_then(|m| m.vram_gb()) {
            gpu = format!("{gpu} {g}G");
        }
    }
    gpu
}

/// Rows kept in view past the cursor, as vim's `scrolloff` keeps them: moving down shows the next
/// pods before the cursor reaches the edge, so the end of the list is never a guess. Live (the
/// human): with the cursor on the bottom row "its kinda hard to know if you are at the end or not".
pub(crate) const SCROLLOFF: usize = 2;

/// The first row a list of `len` rows shows in `visible` rows: the view stays at `prev` (the last
/// frame's) until the cursor at `selected` comes within `margin` rows of an edge, then moves just
/// enough to keep `margin` rows past it in view. On a short list `margin` is cut to
/// `(visible - 1) / 2`, so a 3-row view still has a middle; at the first and last row there's
/// nothing more to show. Never past the end: a list that shrank (or a view that grew) shows no
/// empty rows below while there are rows above. Pure.
pub(crate) fn scroll_offset(prev: usize, selected: usize, len: usize, visible: usize, margin: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if visible == 0 {
        return prev.min(len - 1); // nothing shows: the view stays for when it's back
    }
    let selected = selected.min(len - 1);
    let margin = margin.min((visible - 1) / 2);
    // The rows that must show: `margin` each side of the cursor, as far as the list goes.
    let (first, last) = (selected.saturating_sub(margin), (selected + margin).min(len - 1));
    let prev = prev.min(len.saturating_sub(visible));
    if first < prev {
        first
    } else if last >= prev + visible {
        last + 1 - visible
    } else {
        prev
    }
}

/// A list's view, kept across frames so it scrolls by [`scroll_offset`]: where it was last drawn
/// and its first row (a click maps to a row by them), and the row a click put the cursor on. A
/// click doesn't scroll the view — the row stays under the pointer, for a second click or a
/// shift-click — until the cursor moves from it.
#[derive(Debug, Default)]
pub(crate) struct ListView {
    /// `(area, first row)` as last drawn. Cells because `view` borrows `&Ui` (the TUI is
    /// single-threaded).
    pub(crate) at: std::cell::Cell<(Rect, usize)>,
    pub(crate) clicked: std::cell::Cell<Option<usize>>,
}

impl ListView {
    /// This frame's state for a table of `len` rows, `visible` of them in view, the cursor on
    /// `selected` (none when there are no rows).
    pub(crate) fn state(&self, selected: Option<usize>, len: usize, visible: usize) -> TableState {
        let Some(sel) = selected else { return TableState::default() };
        let margin = if self.clicked.get() == Some(sel) {
            0
        } else {
            self.clicked.set(None);
            SCROLLOFF
        };
        TableState::new().with_offset(scroll_offset(self.at.get().1, sel, len, visible, margin)).with_selected(sel)
    }

    /// After the draw: where the rows landed, so a click maps to the pod under it.
    pub(crate) fn drawn(&self, area: Rect, ts: &TableState) {
        self.at.set((area, ts.offset()));
    }
}

pub(crate) fn pods_table(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect, with_spark: bool) {
    // Which columns show at this width, and how wide: `columns.rs` (its rules, in order, are
    // there). PROXY is as wide as its widest cell, never under its header.
    let proxy_w = shared.snap.pods.iter().map(|sp| Span::raw(proxy_cell(sp).0).width()).max().unwrap_or(0);
    // AI$: each pod's OpenRouter key, spent against its cap — only with a provisioning key.
    // PROGRESS is what PROGRESS_CMD reports, so only with one set.
    let want_progress = ui.cfg.get("PROGRESS_CMD").is_some_and(|c| !c.trim().is_empty());
    // OWNER: while some pod has an owner (`--owner`, `spend assign`).
    let c = columns::columns(area.width as usize, proxy_w, shared.ai.is_some(), !shared.owners.is_empty(), want_progress, with_spark);
    let now = shared.snap.generated_at;

    let mut header_cells = vec!["", "P", "NAME", "STATUS", "M", "SET", "GPU", "GPU%"];
    for (show, title) in [(c.mem, "MEM"), (c.temp, "TEMP"), (c.disk, "DISK")] {
        if show {
            header_cells.push(title);
        }
    }
    header_cells.extend(["$/H", "HLTH", "PROXY"]);
    if c.rtt {
        header_cells.push("RTT");
    }
    if c.branch {
        header_cells.push("BRANCH");
    }
    if c.saved {
        header_cells.push(columns::SAVED_HEADER);
    }
    if c.ai {
        header_cells.push("AI$");
    }
    if c.owner {
        header_cells.push("OWNER");
    }
    if c.host {
        header_cells.push("CPU%");
        header_cells.push("RAM%");
    }
    if c.progress {
        header_cells.push("PROGRESS");
    }
    if c.spark {
        // GPU and its memory (green, cyan); a CPU box's CPU and RAM (magenta, blue).
        header_cells.push("UTIL%~");
        header_cells.push("MEM%~");
    }
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = shared
        .pods
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let m = shared.metrics.get(&p.name);
            // This row's snapshot entry (same index: `Shared::publish`).
            let sp = shared.snap.pods.get(i);
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
            let disk_pct = m.and_then(|m| m.disk_summary()).filter(|(_, t)| *t > 0)
                .map(|(u, t)| (u as u64 * 100 / t as u64) as u32);
            let disk = match m.and_then(|m| m.disk_summary()) {
                Some((u, t)) => format!("{:.0}/{:.0}G", u as f64 / 1024.0, t as f64 / 1024.0),
                None => "-".into(),
            };
            let temp = m.and_then(|m| m.max_temp());
            let temp_str = temp.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into());
            // As `pods list` shows it: the provider's currency, `-` unless billing.
            let cost = fleet::price_label(p);
            // The one-letter badge: M = host maintenance on record (the more urgent, so it
            // wins), else L = locked (stop/restart/terminate refused; the detail pane says so).
            let maint = if state::has_maintenance(p) {
                Cell::from("M").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else if arena_core::lock::is_locked(p) {
                Cell::from("L").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let (health, health_tone) =
                health_cell(sp.and_then(|sp| sp.health.as_ref()), shared.deep.is_running(p), now);
            let (proxy, proxy_tone) = sp.map(proxy_cell).unwrap_or(("?".into(), Tone::Dim));
            let gpu = gpu_text(p, m, c.narrow_gpu);
            let branch = match m.and_then(|m| m.branch.clone()) {
                Some(b) => truncate(&short_branch(&b, &ui.prefix), 6),
                None => "-".into(),
            };
            // RUNNING-but-unreachable reads as "init" (still coming up), in yellow.
            let status_label = pod_status(p, m.map(|m| m.error.is_none()));
            let status_cell = Cell::from(status_label.clone()).style(status_style(&status_label));
            let mark = if ui.marked.contains(&mark_key(p)) {
                Cell::from("•").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let mut cells = vec![
                mark,
                provider_cell(&p.provider),
                // Force the short name when cramped, regardless of the persisted pref.
                Cell::from(if c.compact_names {
                    display_name(&p.name, &ui.prefix, true)
                } else {
                    ui.shown_name(&p.name)
                }),
                status_cell,
                maint,
                setup_cell(m),
                Cell::from(gpu),
                Cell::from(util_str).style(util_style(util, err)),
            ];
            if c.mem {
                cells.push(Cell::from(mem));
            }
            if c.temp {
                cells.push(Cell::from(temp_str).style(temp_style(temp)));
            }
            if c.disk {
                cells.push(Cell::from(disk).style(capacity_style(disk_pct)));
            }
            cells.extend([
                Cell::from(cost),
                Cell::from(health).style(tone_style(health_tone)),
                Cell::from(proxy).style(tone_style(proxy_tone)),
            ]);
            if c.rtt {
                let (rtt, tone) = rtt_cell(shared.rtts.of(p));
                cells.push(Cell::from(rtt).style(tone_style(tone)));
            }
            if c.branch {
                cells.push(Cell::from(branch));
            }
            if c.saved {
                // A sync glyph (↑ unpushed / ↓ behind / ⇕ diverged) sits right before the
                // time, so a blocked/failed push is obvious next to "when it was saved".
                let (sync, sync_style) = sync_marker(m);
                cells.push(Cell::from(Line::from(vec![
                    Span::styled(sync, sync_style),
                    Span::styled(rel_time_short(saved_commit(p, m)), saved_style(p, m)),
                ])));
            }
            if c.ai {
                let ai = shared.ai.as_ref();
                cells.push(ai_cell(
                    ai.and_then(|a| a.keys.get(&p.name)),
                    ai.is_some_and(|a| a.keys_error.is_some()),
                    ai.is_some_and(|a| a.read),
                ));
            }
            if c.owner {
                // Empty for a pod with no owner; a long name is cut (the detail view has it whole).
                cells.push(Cell::from(shared.owners.get(&mark_key(p)).map(|o| truncate(o, columns::OWNER_W)).unwrap_or_default()));
            }
            if c.host {
                // Host CPU% and RAM% (both coloured by load); exact RAM GB is in detail.
                let cpu = m.and_then(|m| m.cpu_pct);
                cells.push(Cell::from(cpu.map(|c| format!("{c}%")).unwrap_or_else(|| "-".into()))
                    .style(util_style(cpu, err)));
                let ram_pct = m.and_then(|m| m.host_mem_summary())
                    .filter(|(_, t)| *t > 0)
                    .map(|(u, t)| (u as u64 * 100 / t as u64) as u32);
                cells.push(Cell::from(ram_pct.map(|p| format!("{p}%")).unwrap_or_else(|| "-".into()))
                    .style(capacity_style(ram_pct)));
            }
            if c.progress {
                let (detail, detail_style) = detail_cell(m);
                cells.push(Cell::from(detail).style(detail_style));
            }
            if c.spark {
                let h = shared.history.get(&p.name);
                let gpu_hist = h.map(|h| h.util_data()).unwrap_or_default();
                let mem_hist = h.map(|h| h.mem_data()).unwrap_or_default();
                // Grey the sparklines out when the pod isn't reachable — the history is
                // just zeros then, so colour would imply live data that isn't there.
                let cpu = p.cpu_only();
                let connected =
                    m.is_some_and(|m| m.error.is_none() && if cpu { m.cpu_pct.is_some() } else { !m.gpus.is_empty() });
                let (gpu_c, mem_c) = match (connected, cpu) {
                    (false, _) => (Color::DarkGray, Color::DarkGray),
                    (true, false) => (Color::Green, Color::Cyan),
                    (true, true) => (Color::Magenta, Color::Blue),
                };
                cells.push(Cell::from(spark(&gpu_hist, c.spark_w)).style(Style::default().fg(gpu_c)));
                cells.push(Cell::from(spark(&mem_hist, c.spark_w)).style(Style::default().fg(mem_c)));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1),  // mark (•)
        Constraint::Length(1),  // P (provider glyph)
        Constraint::Length(c.name_w as u16), // NAME (auto-short when cramped)
        Constraint::Length(4),  // STATUS (abbreviated: run/exit/stop…)
        Constraint::Length(1),  // M (host maintenance badge; L = locked)
        Constraint::Length(4),  // SET (.name/key/origin/api)
        Constraint::Length(c.gpu_w as u16), // GPU (count, card, VRAM: `2×A4000 16G`, `1×B300 MIG 34G`)
        Constraint::Length(4),  // GPU% (`100%`)
    ];
    if c.mem {
        widths.push(Constraint::Length(columns::MID_W[2] as u16)); // MEM (e.g. "120/240G")
    }
    if c.temp {
        widths.push(Constraint::Length(columns::MID_W[4] as u16)); // TEMP (e.g. "85C")
    }
    if c.disk {
        widths.push(Constraint::Length(columns::MID_W[0] as u16)); // DISK (e.g. "12/100G")
    }
    widths.extend([
        Constraint::Length(6),                        // $/H (e.g. "$0.17", "€0.006", "$21.52")
        Constraint::Length(columns::HEALTH_W as u16), // HLTH (e.g. "✓12m", "✗2h", "…")
        Constraint::Length(c.proxy_w as u16),         // PROXY (e.g. "9500", "9501!" stale)
    ]);
    if c.rtt {
        widths.push(Constraint::Length(columns::MID_W[3] as u16)); // RTT (e.g. "23ms", "180ms")
    }
    if c.branch {
        widths.push(Constraint::Length(columns::MID_W[1] as u16)); // BRANCH (e.g. "w1d2")
    }
    if c.saved {
        widths.push(Constraint::Length(columns::SAVED_W as u16)); // 💾 SAVED (e.g. "3h", "↑2d")
    }
    if c.ai {
        widths.push(Constraint::Length(columns::AI_W as u16)); // AI$ (e.g. "1.23/5")
    }
    if c.owner {
        widths.push(Constraint::Length(columns::OWNER_W as u16)); // OWNER (e.g. "alice")
    }
    if c.host {
        widths.push(Constraint::Length(4)); // CPU%
        widths.push(Constraint::Length(4)); // RAM%
    }
    if c.progress {
        // Fixed budget when sparks are filling the rest; otherwise flex to fill.
        widths.push(if c.spark { Constraint::Length(columns::PROGRESS_W as u16) } else { Constraint::Min(10) });
    }
    if c.spark {
        widths.push(Constraint::Length(c.spark_w as u16)); // GPU% history
        widths.push(Constraint::Length(c.spark_w as u16)); // MEM% history
    }
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .block(Block::default().borders(Borders::ALL).title("Pods"));

    // The rows sit inside the border, under the header.
    let len = shared.pods.len();
    let mut ts = ui.last_table.state((len > 0).then(|| ui.selected.min(len - 1)), len, area.height.saturating_sub(3) as usize);
    f.render_stateful_widget(table, area, &mut ts);
    ui.last_table.drawn(area, &ts);
    // And where the 💾 header landed: a click on it opens the backups screen.
    let header_y = area.y.saturating_add(1);
    let buf = f.buffer_mut();
    let saved = (area.x..area.x.saturating_add(area.width)).find(|&x| header_y < buf.area.bottom() && buf[(x, header_y)].symbol() == columns::SAVED_HEADER);
    ui.backups.saved_at.set(saved.map(|x| (x, header_y)));
}

/// STATUS's colour (the table's and the detail pane's): `run` plain (most rows), a pod coming up
/// — `init` (listed running, not reachable yet), `prov`, `new`, `rstr` — yellow, `no-ssh` red,
/// a stopped pod grey.
pub(crate) fn status_style(label: &str) -> Style {
    match label {
        "init" | "prov" | "new" | "rstr" => Style::default().fg(Color::Yellow),
        "no-ssh" => Style::default().fg(Color::Red),
        "exit" | "stop" | "term" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// The repo's last commit as a save on `p` itself: none while it's older than the machine (a
/// fresh box's course-repo checkout — `metrics::saved_on_machine`).
fn saved_commit(p: &Pod, m: Option<&PodMetrics>) -> Option<i64> {
    arena_core::metrics::saved_on_machine(m?.last_commit, p.created_at)
}

/// SAVED's time colour: yellow when there's uncommitted work; grey when unknown OR when it's a
/// clean tree that's been quiet for a day+ (nothing to do); plain otherwise.
pub(crate) fn saved_style(p: &Pod, m: Option<&PodMetrics>) -> Style {
    let stale = saved_commit(p, m)
        .map(|ts| now_unix().saturating_sub(ts) > 86_400)
        .unwrap_or(false);
    match m.and_then(|m| m.dirty_files) {
        Some(n) if n > 0 => Style::default().fg(Color::Yellow),
        Some(0) if stale => Style::default().fg(Color::DarkGray),
        None => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// When the pod's repo was last committed (= last backup, since `pods backup` commits +
/// pushes), plus whether there's uncommitted work since. `-` if not reported yet, or when the
/// last commit is older than the machine (nothing saved on it yet).
pub(crate) fn backup_summary(p: &Pod, m: Option<&PodMetrics>) -> String {
    let Some(m) = m else { return "-".into() };
    if m.last_commit.is_some() && saved_commit(p, Some(m)).is_none() {
        return "- (none on this machine yet: its repo's last commit predates it)".into();
    }
    match m.last_commit {
        Some(ts) => {
            let state = match m.dirty_files {
                Some(0) => "clean".to_string(),
                Some(n) => format!("⚠ {n} uncommitted"),
                None => "?".to_string(),
            };
            format!("{}  ({state})", rel_time(ts))
        }
        None => "-".into(),
    }
}

/// Current unix time in seconds.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A short relative time like "3h ago" / "2d ago" from a unix timestamp.
fn rel_time(unix_secs: i64) -> String {
    let d = now_unix() - unix_secs;
    match d {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86_400),
    }
}

/// A compact relative time for the table cell: "3h", "2d", "12m", "now", or "-".
fn rel_time_short(saved: Option<i64>) -> String {
    match saved {
        None => "-".into(),
        Some(ts) => {
            let d = now_unix() - ts;
            match d {
                d if d < 60 => "now".into(),
                d if d < 3600 => format!("{}m", d / 60),
                d if d < 86_400 => format!("{}h", d / 3600),
                d => format!("{}d", d / 86_400),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use arena_core::metrics;
    use arena_core::openrouter::UsageView;
    use arena_core::remote::FakeRemote;
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use ratatui::backend::TestBackend;
    use crate::detail::gpu_detail;
    use crate::refresh::owners_by_key;
    use crate::state::{dashboard_snapshot, Rtts};
    use crate::tests::{cfg, draw, fixture_shared, pod, row, screen, ui, NOW};
    use crate::view::view;
    use crate::{handle_mouse, Mode};

    #[test]
    fn rows_show_cost_maintenance_health_and_proxy_from_the_snapshot() {
        let shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(180, 12, &shared, &ui);
        let all = lines.join("\n");
        let header = lines.iter().find(|l| l.contains("HLTH")).unwrap_or_else(|| panic!("{all}"));
        for col in ["$/H", "HLTH", "PROXY", " M "] {
            assert!(header.contains(col), "{col} in {header}");
        }
        // Rows by name, whatever order the providers listed them in.
        let order: Vec<usize> = ["alpha", "bravo", "charlie", "delta"].iter().map(|n| all.find(&format!(" {n} ")).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{all}");

        // (row, must show, must not show)
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("alpha", &["init M", "$0.17", "✓12m", " 9500 "], &["9500!"]),
            ("bravo", &["init L", "$0.25", "✗2h", " 9501! "], &["init M"]),
            ("charlie", &["€0.006"], &["$0.0", "init M", "init L", "✓", "✗", "95"]),
            ("delta", &["exit", " … "], &["$0.13", "exit M"]),
        ];
        for (name, shows, hides) in cases {
            let r = row(&lines, name);
            for s in *shows {
                assert!(r.contains(s), "{name} shows {s:?}: {r}");
            }
            for s in *hides {
                assert!(!r.contains(s), "{name} hides {s:?}: {r}");
            }
        }
        // The top line, in short: each provider's pods and burn, in its currency (Hetzner's €).
        assert!(lines[0].starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h  "), "{all}");
        // The rest is the info panel's: the old bar's words, and the deep check running.
        let mut ui = ui;
        ui.mode = Mode::Info { scroll: 0, detail: false };
        let info = draw(180, 60, &shared, &ui).join("\n");
        for want in [
            "4 pods · 0 GPUs · mean util - · fleet: $0.42/h across 3 billing pod(s) + €0.006/h hetzner ≈ $10.08/day + €0.13/day",
            "deep checks  1 running",
        ] {
            assert!(info.contains(want), "{want:?} in\n{info}");
        }
    }

    /// The human (2026-10-30, prod's dashboard): "B300 SXM6 AC -> B300 MIG". Under MIG
    /// nvidia-smi names the parent card (`NVIDIA B300 SXM6 AC`, its memory unreadable), and the
    /// GPU column showed that, cut: `1×B300 SXM6 AC`. A slice reads as every surface names it,
    /// with its memory as every GPU has it — `1×B300 MIG 34G`, whole at any width — whether the
    /// provider lists its id (v2) or its name (v1), read or not; the detail pane spells it out
    /// with the card it's cut from. Other GPUs are as before.
    #[test]
    fn a_mig_slice_reads_as_the_slice_in_the_table_and_the_detail_pane() {
        let smi = |out: &str| PodMetrics { gpus: metrics::parse_nvidia_smi(out), ..Default::default() };
        let slice = smi("NVIDIA B300 SXM6 AC, 7, [Insufficient Permissions], [Insufficient Permissions], 43\n");
        let mut mig = pod("alpha", "runpod", "rp1", 1, Some(22001));
        mig.gpu_type = Some("NVIDIA B300 SXM6 AC MIG 1g.34gb".into());
        mig.gpu_count = Some(1);
        let mut a4000 = pod("bravo", "runpod", "rp2", 2, Some(22002));
        a4000.gpu_type = Some("NVIDIA RTX A4000".into());
        a4000.gpu_count = Some(2);
        let two = smi("NVIDIA RTX A4000, 3, 100, 16376, 40\nNVIDIA RTX A4000, 0, 100, 16376, 41\n");
        let mut cx = pod("charlie", "hetzner", "88", 3, Some(22));
        cx.gpu_type = Some("cx23".into());
        // (pod, its probe, narrow, the GPU cell)
        let cases: &[(&Pod, Option<&PodMetrics>, bool, &str)] = &[
            (&mig, Some(&slice), false, "1×B300 MIG 34G"),
            (&mig, Some(&slice), true, "1×B300 MIG 34G"),
            (&mig, None, true, "1×B300 MIG 34G"),
            (&a4000, Some(&two), false, "2×RTX A4000 16G"),
            (&a4000, Some(&two), true, "2×A4000 16G"),
            (&a4000, None, false, "2×RTX A4000"),
            (&cx, None, true, "cx23"),
        ];
        for (p, m, narrow, want) in cases {
            assert_eq!(gpu_text(p, *m, *narrow), *want, "{} narrow={narrow}", p.name);
        }
        let mut v1 = mig.clone();
        v1.gpu_type = Some("B300 MIG 34GB".into());
        assert_eq!(gpu_text(&v1, Some(&slice), true), "1×B300 MIG 34G", "v1's name for it");
        // Two slices of one card: nvidia-smi lists the parent once; the provider's count stands.
        let mut two_slices = mig.clone();
        two_slices.gpu_count = Some(2);
        assert_eq!(gpu_text(&two_slices, Some(&slice), false), "2×B300 MIG 34G");
        two_slices.gpu_count = None;
        assert_eq!(gpu_text(&two_slices, Some(&slice), false), "1×B300 MIG 34G", "no count from the provider: the probe's");
        assert_eq!(gpu_detail(&mig, Some(&slice)), "1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)");
        assert_eq!(gpu_detail(&mig, None), "1×B300 MIG 34GB");
        assert_eq!(gpu_detail(&a4000, Some(&two)), "2×RTX A4000 16G");

        // On screen: whole in the narrowest table, and in the detail pane.
        let pods = vec![mig.clone(), a4000.clone()];
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        let mut shared = Shared::default();
        shared.publish(snap);
        shared.metrics.insert(mig.name.clone(), slice);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        for w in [72, 100, 200] {
            let lines = draw(w, 12, &shared, &ui);
            assert!(row(&lines, "alpha").contains(" 1×B300 MIG 34G "), "{w}: {}", row(&lines, "alpha"));
        }
        ui.mode = Mode::Detail;
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("gpu:      1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)"), "{pane}");
    }

    /// SAVED's header is a floppy disk: two cells wide (as `unicode-width`, tmux and the common
    /// terminals count it), the cell after it blank, and every column after it starts in the
    /// header where it starts in the rows. The column is as wide as its cells (`↑23h`).
    #[test]
    fn the_saved_header_is_a_floppy_disk_and_the_columns_after_it_line_up() {
        assert_eq!(Span::raw(columns::SAVED_HEADER).width(), 2);
        let shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let mut t = Terminal::new(TestBackend::new(160, 12)).unwrap();
        t.draw(|f| view(f, &shared, &ui, 5)).unwrap();
        let buf = t.backend().buffer().clone();
        let cells = |y: u16| (0..160u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<Vec<_>>();
        let hy = (0..12).find(|&y| cells(y).concat().contains("HLTH")).unwrap();
        let header = cells(hy);
        let disk = header.iter().position(|c| c == columns::SAVED_HEADER).unwrap_or_else(|| panic!("{}", header.concat()));
        assert_eq!(header[disk + 1], " ", "the second cell of the glyph");
        // The next column (CPU%: no AI key here) starts where its cells do, in every row.
        let cpu = (disk..160).find(|&x| header[x] == "C" && header[x + 1] == "P").unwrap();
        assert_eq!(cpu, disk + columns::SAVED_W + 1, "{}", header.concat());
        for y in hy + 1..hy + 5 {
            let row = cells(y);
            assert_eq!(row[cpu], "-", "row {y}: {}", row.concat());
            assert_eq!(row[cpu - 1], " ", "row {y}: {}", row.concat());
        }
    }

    /// On a fresh box the repo's last commit is the course repo's (the image's checkout, days old):
    /// not a save on that machine, so SAVED says `-` and the detail view says why — until a commit
    /// made after the machine was. Without a creation time, the commit as reported.
    #[test]
    fn a_fresh_boxs_course_repo_commit_isnt_its_last_save() {
        let mut shared = fixture_shared();
        let idx = shared.pods.iter().position(|p| p.name.ends_with("alpha")).unwrap();
        let name = shared.pods[idx].name.clone();
        let made = now_unix() - 3_600;
        shared.pods[idx].created_at = Some(made as u64);
        let mut m = PodMetrics { last_commit: Some(made - 3 * 86_400), dirty_files: Some(0), ..Default::default() };
        shared.metrics.insert(name.clone(), m.clone());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let saved = |shared: &Shared| rel_time_short(saved_commit(&shared.pods[idx], shared.metrics.get(&name)));
        assert_eq!(saved(&shared), "-", "the course repo's 3-day-old commit");
        ui.mode = Mode::Detail;
        ui.selected = idx;
        let pane = draw(200, 40, &shared, &ui).join("\n");
        assert!(pane.contains("backup:   - (none on this machine yet: its repo's last commit predates it)"), "{pane}");
        m.last_commit = Some(made + 600);
        shared.metrics.insert(name.clone(), m.clone());
        assert_eq!(saved(&shared), "50m", "a backup on it");
        shared.pods[idx].created_at = None;
        m.last_commit = Some(made - 3 * 86_400);
        shared.metrics.insert(name.clone(), m);
        assert_eq!(saved(&shared), "3d", "no creation time: as reported");
    }

    /// A narrow terminal — or the detail view's 55% split — keeps the essential columns
    /// whole (NAME, HLTH, PROXY as the port, marked when stale, $/H) and drops the mid
    /// columns in turn (TEMP, RTT, MEM, BRANCH, DISK) rather than letting every column be
    /// crushed: at 70–100 columns the names still read `alpha`, not `al`/`alph`.
    #[test]
    fn a_narrow_terminal_drops_mid_columns_and_keeps_names_and_health_whole() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // (mode, terminal width, mid columns shown, mid columns dropped)
        let cases: &[(Mode, u16, &[&str], &[&str])] = &[
            (Mode::List, 72, &[], &["DISK", "BRANCH", "MEM", "RTT", "TEMP"]),
            (Mode::List, 80, &["DISK"], &["BRANCH", "MEM", "RTT", "TEMP"]),
            (Mode::List, 90, &["DISK", "BRANCH"], &["MEM", "RTT", "TEMP"]),
            (Mode::List, 100, &["DISK", "BRANCH", "MEM"], &["RTT", "TEMP"]),
            (Mode::List, 104, &["DISK", "BRANCH", "MEM", "RTT"], &["TEMP"]),
            (Mode::List, 110, &["DISK", "BRANCH", "MEM", "RTT", "TEMP"], &[]),
            // Detail: the table gets 55% of 160 = 88 columns.
            (Mode::Detail, 160, &["DISK", "BRANCH"], &["MEM", "RTT", "TEMP"]),
        ];
        ui.selected = 3; // delta: the detail pane's title (" delta ") isn't a row we look at
        for (mode, w, shown, dropped) in cases {
            ui.mode = mode.clone();
            let lines = draw(*w, 12, &shared, &ui);
            let all = lines.join("\n");
            let header = lines.iter().find(|l| l.contains("HLTH")).unwrap_or_else(|| panic!("{w}: {all}"));
            for col in ["NAME", "STAT", "SET", "GPU%", "$/H", "HLTH", "PROXY"].iter().chain(shown.iter()) {
                assert!(header.contains(col), "{mode:?} {w}: {col} in {header}");
            }
            for col in *dropped {
                assert!(!header.contains(col), "{mode:?} {w}: {col} dropped from {header}");
            }
            for (name, cells) in [
                ("alpha", &["✓12m", "$0.17", " 9500 "][..]),
                ("bravo", &["✗2h", "$0.25", " 9501! "]),
                ("charlie", &["€0.006"]),
            ] {
                let r = row(&lines, name); // ` alpha ` — the whole short name
                for c in cells {
                    assert!(r.contains(c), "{mode:?} {w}: {name} shows {c:?}: {r}");
                }
            }
        }
    }

    /// The RTT column: each pod's round trip from this box as last timed, coloured (green
    /// near, yellow across the Atlantic, red beyond), `-` when not timed or no answer — and
    /// the detail pane says it beside where the pod runs.
    #[test]
    fn rtt_shows_in_its_column_and_the_detail_pane() {
        let mut shared = fixture_shared();
        let by = |shared: &Shared, name: &str| shared.pods.iter().find(|p| p.name.ends_with(name)).unwrap().clone();
        let (alpha, bravo) = (by(&shared, "alpha"), by(&shared, "bravo"));
        let ep = |p: &Pod| (p.ssh_ip.clone().unwrap(), p.ssh_port.unwrap());
        let mut rtts = Rtts::default();
        rtts.record(vec![(ep(&alpha), Some(Duration::from_millis(23))), (ep(&bravo), Some(Duration::from_millis(131)))], Instant::now());
        shared.rtts = rtts;
        let idx = shared.pods.iter().position(|p| p.name == alpha.name).unwrap();
        shared.pods[idx].location = Some("EUR-IS-1".into());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let mut t = Terminal::new(TestBackend::new(180, 12)).unwrap();
        t.draw(|f| view(f, &shared, &ui, 5)).unwrap();
        let buf = t.backend().buffer().clone();
        let lines = screen(&buf);
        let header = lines.iter().find(|l| l.contains("HLTH")).unwrap();
        let col = header[..header.find("RTT").unwrap_or_else(|| panic!("{header}"))].chars().count();
        let cell = |name: &str| {
            let y = lines.iter().position(|l| l.contains(&format!(" {name} "))).unwrap();
            let text: String = lines[y].chars().skip(col).take(5).collect();
            (text.trim().to_string(), buf[(col as u16, y as u16)].fg)
        };
        assert_eq!(cell("alpha"), ("23ms".to_string(), Color::Green));
        assert_eq!(cell("bravo"), ("131ms".to_string(), Color::Red));
        assert_eq!(cell("charlie"), ("-".to_string(), Color::DarkGray), "not timed");
        ui.mode = Mode::Detail;
        ui.selected = idx;
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("where:    EUR-IS-1  ·  rtt 23ms from this box"), "{pane}");
        ui.selected = shared.pods.iter().position(|p| p.name.ends_with("charlie")).unwrap();
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("where:    -  ·  rtt -"), "{pane}");
        // A place too long for the pane is cut with the rest: one row (review ai-review codex r2).
        ui.selected = idx;
        shared.pods[idx].location = Some("United States, California, San Francisco, Mission District".into());
        let lines = draw(120, 40, &shared, &ui);
        let at = lines.iter().position(|l| l.contains("where:")).unwrap();
        assert!(lines[at].contains("United States, California") && lines[at].contains('…'), "{}", lines[at]);
        assert!(lines[at + 1].contains("proxy:"), "the next fact on the next row:\n{}", lines.join("\n"));
    }

    /// OWNER: who each instance is for, from the spend ledger (per provider + id, never the name),
    /// in a short column that comes in once there's room and only while some pod has an owner —
    /// empty for a pod with none — and whole on the detail view's `owner:` line.
    #[test]
    fn the_owner_shows_in_its_column_and_the_detail_view() {
        use arena_core::spend::{self, Event, OwnerRule, SpendState};
        let mut shared = fixture_shared();
        let alpha = shared.pods.iter().find(|p| p.name.ends_with("alpha")).unwrap().clone();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let header_of = |lines: &[String]| lines.iter().find(|l| l.contains("HLTH")).cloned().unwrap();
        assert!(!header_of(&draw(200, 12, &shared, &ui)).contains("OWNER"), "no owners, no column");
        let state = spend::apply_event(SpendState::default(), &Event::Created { pod: &alpha, owner: &OwnerRule::Given("alexandria".into()) }, 1_800_000_000);
        shared.owners = owners_by_key(&state, &shared.pods);
        assert_eq!(shared.owners.len(), 1, "only alpha has one: {:?}", shared.owners);
        let lines = draw(200, 12, &shared, &ui);
        let header = header_of(&lines);
        assert!(header.contains(" OWNER "), "{header}");
        assert!(row(&lines, "alpha").contains(" alexand… "), "cut to the column: {}", row(&lines, "alpha"));
        assert!(!row(&lines, "bravo").contains("alex"), "no owner: nothing");
        assert!(!header_of(&draw(100, 12, &shared, &ui)).contains("OWNER"), "a narrow terminal drops it");
        ui.mode = Mode::Detail;
        ui.selected = shared.pods.iter().position(|p| p.name == alpha.name).unwrap();
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("owner:    alexandria  (`arena spend` has what it cost)"), "{pane}");
        ui.selected = shared.pods.iter().position(|p| p.name.ends_with("bravo")).unwrap();
        assert!(!draw(160, 40, &shared, &ui).join("\n").contains("owner:"), "no line without an owner");
    }

    /// The human (2026-10-29, prod's dashboard): "why does it say 1 ai key at cap?" — every AI$
    /// cell on screen was under its cap. The bar names the keys at their cap, `(no pod)` for one
    /// no row shows; the detail pane lists them and says where to look. A $0 key nothing was
    /// spent against reads `paused` in its row and isn't counted.
    #[test]
    fn keys_at_cap_are_named_and_a_paused_key_isnt_one() {
        use arena_core::openrouter::KeyInfo;
        let mut shared = fixture_shared();
        let key = |name: &str, limit: f64, left: f64, today: f64| KeyInfo {
            hash: name.into(),
            name: Some(name.into()),
            limit: Some(limit),
            limit_remaining: Some(left),
            usage_daily: Some(today),
            limit_reset: Some("daily".into()),
            ..Default::default()
        };
        let view = UsageView::from_keys(&[key("devtest-alpha", 0.0, 0.0, 0.0), key("devtest-bravo", 5.0, 3.0, 2.0), key("devtest-gone", 5.0, 0.0, 5.0)], None);
        let names: Vec<String> = shared.pods.iter().map(|p| p.name.clone()).collect();
        shared.ai_cohort = Some(view.cohort("devtest", &names));
        shared.ai = Some(view);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(220, 40, &shared, &ui);
        let screen = lines.join("\n");
        assert!(screen.contains(" │ ⚠ OpenRouter key at cap: devtest-gone (no pod) · $7.00 today"), "{screen}");
        assert!(row(&lines, "alpha").contains(" paused "), "{}", row(&lines, "alpha"));
        assert!(row(&lines, "bravo").contains(" 2.00/5 "), "{}", row(&lines, "bravo"));
        ui.mode = Mode::Detail;
        ui.selected = shared.pods.iter().position(|p| p.name == "devtest-alpha").unwrap();
        let pane = draw(220, 50, &shared, &ui).join("\n");
        assert!(pane.contains("ai key:   paused (a $0 cap, nothing spent against it) · $0.00 of $0.00 today"), "{pane}");
        // A key at its cap, or disabled: where to act on it, first (with the rows there to act).
        ui.ai_keys = true;
        let mut at_cap = shared.ai.clone().unwrap();
        at_cap.keys.insert("devtest-alpha".into(), at_cap.keys["devtest-gone"].clone());
        let mut s2 = Shared { ai: Some(at_cap.clone()), ..fixture_shared() };
        let hinted = draw(220, 50, &s2, &ui).join("\n");
        assert!(hinted.contains("ai key:   at its cap (a → raise cap) · $5.00 of $5.00 today"), "{hinted}");
        at_cap.keys.get_mut("devtest-alpha").unwrap().disabled = true;
        s2.ai = Some(at_cap);
        let hinted = draw(220, 50, &s2, &ui).join("\n");
        assert!(hinted.contains("ai key:   disabled (a → ai resume) · $5.00 of $5.00"), "{hinted}");
        assert!(pane.contains("ai caps:  ⚠ 1 key at cap (`arena keys list` has each one's cap and spend): devtest-gone (no pod)"), "{pane}");
        // A pod named after it: no `(no pod)`. None at cap (only the paused one): no line.
        shared.ai_cohort = Some(shared.ai.as_ref().unwrap().cohort("devtest", &[names, vec!["devtest-gone".into()]].concat()));
        let pane = draw(220, 50, &shared, &ui).join("\n");
        assert!(pane.contains("spend): devtest-gone ") && !pane.contains("(no pod)"), "{pane}");
        // A key at its cap that a row carries: the top line says how to raise it.
        assert!(pane.contains(" ⚠ OpenRouter key at cap: devtest-gone (a → raise cap) · "), "{pane}");
        shared.ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.0, 0.0, 0.0)], None));
        shared.ai_cohort = Some(shared.ai.as_ref().unwrap().cohort("devtest", &[]));
        assert!(!draw(220, 50, &shared, &ui).join("\n").contains("ai caps:"));
    }

    /// The view's first row, as vim's `scrolloff` moves it (9 rows in view, 2 kept past the
    /// cursor unless cut): still until the cursor comes within the margin of an edge, then just
    /// enough; never past the end, so a fleet that shrank or a view that grew leaves no empty rows
    /// below while there are pods above.
    #[test]
    fn the_view_keeps_rows_past_the_cursor_and_no_empty_rows() {
        for (prev, sel, len, visible, margin, want, why) in [
            (0, 0, 30, 9, 2, 0, "the first pod"),
            (0, 6, 30, 9, 2, 0, "two pods below it still in view"),
            (0, 7, 30, 9, 2, 1, "the next row comes into view a row early"),
            (0, 29, 30, 9, 2, 21, "the last pod (G): the cursor reaches the edge"),
            (21, 28, 30, 9, 2, 21, "up from the bottom: still"),
            (21, 23, 30, 9, 2, 21, "still while two rows above it show"),
            (21, 22, 30, 9, 2, 20, "then a row at a time"),
            (21, 0, 30, 9, 2, 0, "g"),
            (3, 11, 30, 9, 0, 3, "no margin (a click): the cursor may sit on the edge"),
            (0, 1, 30, 3, 2, 0, "3 rows: the margin is cut to one"),
            (0, 2, 30, 3, 2, 1, "3 rows: one past the cursor"),
            (5, 5, 30, 3, 2, 4, "3 rows: one above it"),
            (0, 1, 30, 2, 2, 0, "2 rows: no margin"),
            (0, 2, 30, 2, 2, 1, "2 rows: the cursor on the edge"),
            (0, 5, 30, 1, 2, 5, "1 row: the cursor's"),
            (4, 9, 10, 20, 2, 0, "all in view"),
            (21, 20, 25, 9, 2, 16, "five pods went: the view ends at the last pod"),
            (21, 25, 30, 14, 2, 16, "a taller view: no empty rows"),
            (0, 8, 30, 5, 2, 6, "a shorter view: the margin still shows"),
            (0, 99, 30, 9, 2, 21, "a cursor past the end is the last pod"),
            (7, 0, 0, 9, 2, 0, "no pods"),
            (7, 3, 30, 0, 2, 7, "no rows to draw: the view stays"),
        ] {
            assert_eq!(scroll_offset(prev, sel, len, visible, margin), want, "{why}: {prev} {sel} {len} {visible} {margin}");
        }
    }

    /// The human, on the dashboard: with the cursor on the bottom row "its kinda hard to know if
    /// you are at the end or not". On a short terminal (the demo's 30 pods, 9 rows in view) ↓
    /// shows the next two pods before the cursor gets to the edge; ↑ from the bottom keeps the
    /// view still until the cursor is two from the top; a click (and a shift-click) moves the
    /// cursor, never the view, so the row stays under the pointer.
    #[test]
    fn the_list_keeps_two_pods_in_view_past_the_cursor() {
        use crate::input::list_key;
        use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let conf = cfg("");
        let shared = Arc::new(std::sync::Mutex::new(Shared::default()));
        crate::demo::publish(&shared, &conf, 30, 0);
        let mut ui = ui(conf, Arc::new(FakeRemote::new()));
        let names: Vec<String> = shared.lock().unwrap().pods.iter().map(|p| p.name.trim_start_matches("devtest-").to_string()).collect();
        assert_eq!(names.len(), 30);
        // The pods the rows show, in order, and the cursor's row.
        let frame = |ui: &Ui, height: u16| -> (Vec<usize>, usize) {
            let lines = draw(160, height, &shared.lock().unwrap(), ui);
            let (area, _) = ui.last_table.at.get();
            let rows = &lines[area.y as usize + 2..(area.y + area.height) as usize - 1];
            let shown = rows.iter().map(|l| names.iter().position(|n| l.contains(&format!(" {n} "))).unwrap_or_else(|| panic!("no pod in {l:?}"))).collect();
            (shown, rows.iter().position(|l| l.contains('▶')).expect("the cursor's row"))
        };
        let (shown, cursor) = frame(&ui, 14);
        assert_eq!((shown, cursor), ((0..9).collect(), 0));
        for sel in 1..30 {
            list_key(&mut ui, KeyCode::Down, 30);
            let (shown, cursor) = frame(&ui, 14);
            assert_eq!(shown[cursor], sel);
            assert_eq!(shown[0], (sel + 3).saturating_sub(9).min(21), "↓ to {sel}: {shown:?}");
            assert!(shown.contains(&(sel + 2).min(29)), "↓ to {sel}: the next two show: {shown:?}");
        }
        assert_eq!(frame(&ui, 14).1, 8, "the last pod: on the bottom edge, nothing below it");
        for sel in (0..29).rev() {
            list_key(&mut ui, KeyCode::Up, 30);
            let (shown, cursor) = frame(&ui, 14);
            assert_eq!(shown[cursor], sel);
            assert_eq!(shown[0], sel.saturating_sub(2).min(21), "↑ to {sel}: still until two from the top: {shown:?}");
        }
        // A click on the bottom row: the cursor goes there, the view stays; a shift-click marks it.
        let (area, _) = ui.last_table.at.get();
        let bottom = area.y + area.height - 2;
        let click = |mods| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: area.x + 30, row: bottom, modifiers: mods };
        assert!(handle_mouse(click(KeyModifiers::NONE), &mut ui, &shared));
        assert_eq!(ui.selected, 8);
        assert_eq!(frame(&ui, 14), ((0..9).collect(), 8), "the click didn't scroll");
        assert!(handle_mouse(click(KeyModifiers::SHIFT), &mut ui, &shared));
        assert_eq!(ui.marked.len(), 1);
        assert_eq!(frame(&ui, 14), ((0..9).collect(), 8), "nor did the shift-click");
        // The next ↓ scrolls again, two past the cursor.
        list_key(&mut ui, KeyCode::Down, 30);
        assert_eq!(frame(&ui, 14), ((3..12).collect(), 6));
        // A shorter terminal keeps the cursor two from its new edge.
        assert_eq!(frame(&ui, 10), ((7..12).collect(), 2));
    }
}
