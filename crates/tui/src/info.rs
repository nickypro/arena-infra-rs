//! The info panel (`i`): everything the top line and the footer leave out, and its keys.

use arena_core::balance::{self, AccountView};
use arena_core::{fleet, snapshot};
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::modals::{centered_rect, scroll_by};
use crate::state::{self, count_of, summary_text, Tone};
use crate::table::tone_style;
use crate::{Mode, Shared, Ui};

/// The info panel's keys ([`Mode::Info`]): Esc, `i` or `q` go back to the view it was opened
/// from; ↑↓ (j k), PgUp/PgDn (space), Home/End scroll it. `None`: the panel isn't open;
/// `Some(false)`: a key it ignores.
pub(crate) fn info_key(ui: &mut Ui, code: KeyCode) -> Option<bool> {
    let Mode::Info { scroll, detail } = ui.mode else { return None };
    let max = ui.result_max.get();
    let to = |delta: i32| Mode::Info { scroll: scroll_by(scroll, max, delta), detail };
    ui.mode = match code {
        KeyCode::Esc | KeyCode::Char('i' | 'q') => if detail { Mode::Detail } else { Mode::List },
        KeyCode::Up | KeyCode::Char('k') => to(-1),
        KeyCode::Down | KeyCode::Char('j') => to(1),
        KeyCode::PageUp => to(-10),
        KeyCode::PageDown | KeyCode::Char(' ') => to(10),
        KeyCode::Home => Mode::Info { scroll: 0, detail },
        KeyCode::End => Mode::Info { scroll: max, detail },
        _ => return Some(false),
    };
    Some(true)
}

/// Every key of the list and detail views, and the mouse, for the info panel: `(keys, what)`.
const ALL_KEYS: &[(&str, &str)] = &[
    ("q / ctrl-c", "quit (asks first while a run that changes pods is going)"),
    ("esc", "back to the list from the detail view; on the list, clear the marks"),
    ("↑↓ / j k", "move the cursor"),
    ("enter", "the detail view of the cursor pod"),
    ("i  (or click [i] info)", "this panel"),
    ("space", "mark / unmark the cursor pod"),
    ("x", "clear the marks"),
    ("/", "mark pods by selector (the CLI's syntax)"),
    ("a", "act on the marked pods, else the cursor pod (menu)"),
    ("a a / a o", "the cursor pod's AI key: raise its cap / pause or resume it (with a provisioning key)"),
    ("A", "act on every pod (menu)"),
    ("d", "deep-check the marked pods, else the cursor pod"),
    ("D", "deep-check every running pod nobody is using (press twice)"),
    ("c", "ssh into the cursor pod"),
    ("?", "ask an AI about the cursor pod or the marked ones (tab: the other AI)"),
    ("n", "add pods (runs `arena pods up`)"),
    ("p", "the runs panel (pods up, start, rename, jobs, logs, copy home, the AI key's)"),
    ("b  (or click 💾)", "the backups screen: each backup job (how often, last run, next), what's running, every pod's last git save, home copy and archive copy (enter: its history, back up now)"),
    ("r", "refresh now (and re-read the providers' details)"),
    ("f", "refresh cadence: 2 → 5 → 10 → 20 → 60 s"),
    ("s", "short / full pod names (kept)"),
    ("mouse", "click a row: cursor · click its far left or shift-click: mark · wheel: move · [i] info: this panel · 💾: backups"),
];

/// What the table's glyphs mean, for the info panel: `(column, legend)`.
const LEGEND: &[(&str, &str)] = &[
    ("P", "provider: R runpod · V vast · H hetzner · S shadeform"),
    ("M", "M host maintenance on record · L locked (stop/restart/terminate refused)"),
    ("SET", ".name · deploy key · origin→GitHub · API key: ✓ yes  ✗ no  · unknown"),
    ("HLTH", "last deep check and its age: ✓ pass (grey after 26h)  ! warn  ✗ fail  · skipped  … checking  - never"),
    ("PROXY", "the forward's port: green live · `!` yellow stale (`arena proxy apply` re-points it) · - none · ? unreadable"),
    ("💾", "when the repo was last committed: ↑ unpushed  ↓ behind  ⇕ diverged · yellow: uncommitted work · grey: quiet a day+"),
    ("AI$", "the pod's OpenRouter key: spent/cap this period · yellow from 80% · red at the cap · paused: a $0 cap"),
    ("RTT", "round trip from this box to the pod's SSH port: green < 60 ms · yellow < 120 ms · red beyond"),
    ("↻ 7s ago · every 5s", "refreshed 7 s ago, every 5 s (`refreshing` while it does, the ↻ in cyan)"),
];

/// The info panel's text ([`Mode::Info`]): every word the top line and the footer leave out —
/// under `dashboard` the provider, config, whole refresh status, cadence and deep checks; under
/// `fleet` the summary bar as it used to read ([`summary_text`]); each prepaid account; the
/// OpenRouter credit and keys; every key of the list and detail views; what the glyphs mean.
/// `detail`: opened from the detail view (its keys are the same but for `esc`).
fn info_lines(shared: &Shared, ui: &Ui, secs: u64) -> Vec<Line<'static>> {
    let head = |t: &str| Line::styled(t.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let dim = Style::default().fg(Color::DarkGray);
    let row = |label: &str, value: String, style: Style| Line::from(vec![Span::styled(format!("  {label:<13}"), dim), Span::styled(value, style)]);
    let toned = |t: Option<Tone>| t.map(tone_style).unwrap_or_default();
    let mut out = vec![head("dashboard")];
    out.push(row("provider", format!("{} (where the add-pod form starts; the table lists every configured provider)", ui.provider_name), Style::default()));
    out.push(row("config", ui.config_path.clone(), Style::default()));
    out.push(row("status", shared.status.full.clone(), toned(shared.status.tone)));
    let age = match shared.last_refresh {
        Some(t) => format!("last {}s ago", t.elapsed().as_secs()),
        None => "not refreshed yet".into(),
    };
    let now = if shared.refreshing { " · refreshing now" } else { "" };
    out.push(row("refresh", format!("every {secs}s ([f] changes it, [r] refreshes now) · {age}{now}"), Style::default()));
    let deep = match shared.deep.running() {
        0 => ("none running ([d] this pod or the marked ones, [D] every idle one)".to_string(), Style::default()),
        n => (format!("{n} running"), tone_style(Tone::Busy)),
    };
    out.push(row("deep checks", deep.0, deep.1));
    out.push(Line::raw(""));

    out.push(head("fleet"));
    // The bar as it used to read: its warnings each on a line in their colours, then the rest.
    let fleet = summary_text(&shared.summary, &shared.snap.cost, &shared.snap.partial, &shared.balance, shared.ai_cohort.as_ref());
    let mut rest = fleet.as_str();
    for (lead, tone) in [
        (state::partial_notice(&shared.snap.partial), Some(Tone::Warn)),
        (balance::tui_parts(&shared.balance).0, state::balance_tone(&shared.balance)),
        (shared.ai_cohort.as_ref().and_then(|a| a.tui_parts().0), state::ai_tone(shared.ai_cohort.as_ref())),
    ] {
        if let Some(lead) = lead.filter(|l| rest.starts_with(l.as_str())) {
            rest = &rest[lead.len()..];
            out.push(Line::styled(format!("  {}", lead.trim().trim_end_matches('·').trim_end()), toned(tone)));
        }
    }
    for part in rest.split("  ·  ").map(str::trim).filter(|p| !p.is_empty()) {
        out.push(Line::raw(format!("  {part}")));
    }
    out.push(Line::raw(""));

    out.push(head("balances (prepaid accounts)"));
    let stamp = snapshot::unix_now();
    let views: Vec<&AccountView> = shared.balance.iter().collect();
    if views.is_empty() {
        out.push(Line::styled("  none read yet — read every few minutes when a prepaid account's key is configured (`arena balance` has them all)", dim));
    }
    for v in views {
        let style = match v.level {
            balance::Level::Critical => tone_style(Tone::Bad),
            balance::Level::Low => tone_style(Tone::Warn),
            balance::Level::Unknown => tone_style(Tone::Dim),
            _ => Style::default(),
        };
        let symbol = if v.currency == "EUR" { "€" } else { "$" };
        let burn = v.burn.as_ref().map(|b| format!(" · burn {}/h", fleet::fmt_money(symbol, b.per_hr))).unwrap_or_default();
        out.push(row(
            &v.account,
            format!("{} {} · runway {}{burn}", balance::balance_label(v), balance::source_label(v, stamp), balance::fmt_runway(v)),
            style,
        ));
    }
    out.push(Line::raw(""));

    if let Some(ai) = shared.ai.as_ref() {
        out.push(head("AI (OpenRouter)"));
        let money = |x: f64| fleet::fmt_money("$", x);
        let c = shared.ai_cohort.as_ref();
        let credit = match (ai.credits, &ai.credits_error) {
            (Some(cr), None) => format!("{} left", money(cr.left())),
            (Some(cr), Some(e)) => format!("{} left as last read (the latest read failed: {e})", money(cr.left())),
            (None, Some(e)) => format!("? ({e})"),
            (None, None) => "… (not read yet)".into(),
        };
        out.push(row("credit", credit, toned(state::ai_tone(c))));
        if let Some(c) = c {
            let today = c.today.map(money).unwrap_or_else(|| "?".into());
            let uncapped = if c.uncapped > 0 { format!(" · {} with no cap", c.uncapped) } else { String::new() };
            out.push(row("cohort", format!("{} · {today} today · {} more allowed under their caps{uncapped}", count_of(c.keys, "key", "keys"), money(c.headroom)), Style::default()));
            if !c.capped.is_empty() {
                let names: Vec<String> = c.capped.iter().map(|k| if k.pod { k.name.clone() } else { format!("{} (no pod)", k.name) }).collect();
                out.push(row("at cap", format!("{} — `arena keys list` has each one's cap and spend", names.join(", ")), tone_style(Tone::Warn)));
            }
        }
        if let Some(e) = &ai.keys_error {
            out.push(row("keys", format!("⚠ the latest read failed: {e} (the numbers are the last good ones)"), tone_style(Tone::Warn)));
        }
        out.push(Line::raw(""));
    }

    out.push(head("keys"));
    for (k, what) in ALL_KEYS {
        out.push(Line::from(vec![Span::styled(format!("  {k:<17}"), Style::default().add_modifier(Modifier::BOLD)), Span::raw(*what)]));
    }
    out.push(Line::raw(""));
    out.push(head("columns"));
    for (col, what) in LEGEND {
        out.push(Line::from(vec![Span::styled(format!("  {col:<10}"), Style::default().add_modifier(Modifier::BOLD)), Span::raw(*what)]));
    }
    out
}

/// The info panel ([`info_lines`]), scrolled by `ui.mode`'s `scroll` — clamped, and where the end is
/// told back through `result_max` (`scroll_by`).
pub(crate) fn render_info(f: &mut Frame, shared: &Shared, ui: &Ui, _detail: bool, secs: u64) {
    let Mode::Info { scroll, .. } = ui.mode else { return };
    let area = centered_rect(90, 86, f.area());
    f.render_widget(Clear, area);
    let text = Paragraph::new(info_lines(shared, ui, secs)).wrap(Wrap { trim: false });
    let inner = area.height.saturating_sub(2);
    let total = u16::try_from(text.line_count(area.width.saturating_sub(2))).unwrap_or(u16::MAX);
    ui.result_max.set(total.saturating_sub(inner));
    let scroll = scroll.min(ui.result_max.get());
    f.render_widget(
        text.scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" info — [esc/i] close · [↑↓ PgUp PgDn] scroll "),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use arena_core::remote::FakeRemote;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use crate::input::{handle_mouse, list_key, ListKey};
    use crate::tests::{cfg, draw, fixture_shared, ui};

    /// `i` — or a click on the footer's `[i] info` — opens the info panel over the list or the detail
    /// view; it holds every word the bars left out; Esc (or `i`, or another click) goes back to
    /// where it was opened; ↑↓ scroll it, no further than its end.
    #[test]
    fn i_or_the_menu_glyph_opens_the_info_panel_and_esc_goes_back() {
        let shared = Arc::new(Mutex::new(fixture_shared()));
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        assert_eq!(list_key(&mut ui, KeyCode::Char('i'), 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::Info { scroll: 0, detail: false }), "{:?}", ui.mode);
        let all = draw(160, 70, &shared.lock().unwrap(), &ui).join("\n");
        for want in [
            "provider     runpod (where the add-pod form starts",
            "config       /test/config.env",
            "refresh      every 5s ([f] changes it, [r] refreshes now) · not refreshed yet",
            "deep checks  1 running",
            "4 pods · 0 GPUs · mean util - · fleet: $0.42/h across 3 billing pod(s)",
            "none read yet",
            "D                deep-check every running pod nobody is using (press twice)",
            "HLTH      last deep check and its age: ✓ pass",
            "PROXY     the forward's port",
        ] {
            assert!(all.contains(want), "{want:?} in\n{all}");
        }
        assert!(all.lines().last().unwrap().starts_with("[i] info · ") && all.contains("[esc/i] close"), "{all}");
        // Scrolled, it stops at its end.
        let before = draw(160, 20, &shared.lock().unwrap(), &ui);
        for _ in 0..500 {
            info_key(&mut ui, KeyCode::Down);
        }
        let Mode::Info { scroll, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert!(scroll > 0 && scroll == ui.result_max.get(), "{scroll} {}", ui.result_max.get());
        assert_ne!(draw(160, 20, &shared.lock().unwrap(), &ui), before);
        assert_eq!(info_key(&mut ui, KeyCode::Char('z')), Some(false), "not the panel's: nothing");
        assert_eq!(info_key(&mut ui, KeyCode::Esc), Some(true));
        assert!(matches!(ui.mode, Mode::List));
        assert_eq!(info_key(&mut ui, KeyCode::Esc), None, "closed: not its key any more");
        // From the detail view, back to it.
        ui.mode = Mode::Detail;
        list_key(&mut ui, KeyCode::Char('i'), 4);
        assert!(matches!(ui.mode, Mode::Info { detail: true, .. }));
        info_key(&mut ui, KeyCode::Char('i'));
        assert!(matches!(ui.mode, Mode::Detail));
        // A click on `[i] info` (bottom-left, cells 0–7) opens it and closes it; a click beside it doesn't.
        ui.mode = Mode::List;
        let footer_y = draw(120, 20, &shared.lock().unwrap(), &ui).len() as u16 - 1;
        let click = |column: u16, row: u16| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
        assert!(!handle_mouse(click(9, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List));
        assert!(handle_mouse(click(0, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: false, .. }));
        assert!(handle_mouse(click(7, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List));
    }
}
