//! A frame: the top line, the pods table (or the split view with the detail pane), the footer, and the
//! popup that's open on top.

use arena_core::config::LEGACY_CONFIG as DEFAULT_CONFIG;
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::detail::detail_pane;
use crate::modals::{render_confirm, render_fleet_confirm, render_fleet_menu, render_input, render_menu, render_result, render_select};
use crate::state::{top_parts, Tone};
use crate::table::{pods_table, tone_style};
use crate::{ask, backups, newpod, render_info, runs, Mode, Shared, Ui, NOTICE_FOR};

pub(crate) fn view(f: &mut Frame, shared: &Shared, ui: &Ui, secs: u64) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // the top line: fleet summary (and a non-default provider/config)
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer: [i] info · status · age · cadence, keys
        ])
        .split(f.area());

    top_line(f, shared, ui, chunks[0]);
    ui.top_at.set(Some(chunks[0].y));

    if let Mode::Backups(v) = &ui.mode {
        backups::render(f, shared, ui, v, chunks[1]);
    } else if matches!(ui.mode, Mode::Detail) || matches!(ui.mode, Mode::Info { detail: true, .. }) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[1]);
        // No room for the history sparklines in the split view.
        pods_table(f, shared, ui, cols[0], false);
        detail_pane(f, shared, ui, cols[1]);
    } else {
        pods_table(f, shared, ui, chunks[1], true);
    }

    f.render_widget(Paragraph::new(footer_hint(shared, ui, secs)), chunks[2]);
    // Where the footer's `[i] info` is (its first cell): a click there opens the info panel.
    ui.menu_at.set(Some((chunks[2].x, chunks[2].y)));

    match &ui.mode {
        Mode::Menu { sel } => render_menu(f, shared, ui, *sel),
        Mode::Confirm(c) => render_confirm(f, c),
        Mode::FleetMenu { scope, sel } => render_fleet_menu(f, shared, ui, *scope, *sel),
        Mode::FleetConfirm { action, typed, scope, note } => render_fleet_confirm(f, shared, ui, *action, typed, *scope, note),
        Mode::NewPod(form) => newpod::render(f, form, &ui.offers),
        Mode::UpConfirm(form) => newpod::render_confirm(f, form, &ui.offers),
        Mode::Runs { id, scroll, stop } => runs::render(f, shared, *id, *scroll, *stop, &ui.result_max),
        Mode::Quit => runs::render_quit(f, shared),
        Mode::Input { action, scope, value } => render_input(f, shared, ui, *action, scope, value),
        Mode::Select { value } => render_select(f, value),
        Mode::Working(msg) => render_result(f, msg, 0, &ui.result_max),
        Mode::Result(msg) => render_result(f, msg, ui.result_scroll, &ui.result_max),
        Mode::Ask { scope, value, pick } => ask::render_input(f, shared, ui, scope, value, pick),
        Mode::Answer(answer) => ask::render_answer(f, answer, ui.result_scroll, &ui.result_max),
        Mode::Info { detail, .. } => render_info(f, shared, ui, *detail, secs),
        _ => {}
    }
}

/// The top line ([`state::top_parts`]): the fleet in short, a group per provider and
/// OpenRouter's, laid out for the terminal's width — the least needed words left out, never a
/// warning. A provider other than RunPod (where the add-pod form starts) or a config other than
/// the default goes at the right while there's room (always in the info panel): a dashboard on
/// a second fleet should say so. A click on it opens the info panel ([`handle_mouse`]).
fn top_line(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let tag = where_tag(ui);
    let top = top_parts(&shared.summary, &shared.pods, &shared.snap.partial, &shared.balance, shared.ai_cohort.as_ref(), tag.as_deref(), area.width as usize);
    let spans: Vec<Span> = top
        .parts
        .into_iter()
        .map(|p| match p.tone {
            Some(t) => Span::styled(p.text, tone_style(t)),
            None => Span::raw(p.text),
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD))), area);
    if let Some(tag) = top.tag {
        let w = Span::raw(&tag).width() as u16 + 1;
        let at = Rect { x: area.x + area.width.saturating_sub(w), width: w.min(area.width), ..area };
        f.render_widget(Paragraph::new(Span::styled(tag, Style::default().fg(Color::Cyan))), at);
    }
}

/// The provider (when not RunPod) and the config (when not the default), for the top line's
/// right end: `vast · ~/sandbox/config.env`; `None` when both are the defaults.
fn where_tag(ui: &Ui) -> Option<String> {
    let mut out = Vec::new();
    if ui.provider_name != "runpod" {
        out.push(ui.provider_name.clone());
    }
    if ui.config_path != DEFAULT_CONFIG {
        out.push(home_short(&ui.config_path));
    }
    (!out.is_empty()).then(|| out.join(" · "))
}

/// A path with the home directory as `~`.
fn home_short(path: &str) -> String {
    match std::env::var("HOME").ok().filter(|h| !h.is_empty() && h != "/") {
        Some(h) => match path.strip_prefix(h.trim_end_matches('/')) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// The footer's first words, always in the same cells: a click on them, or `i`, opens the info
/// panel. Words, not a `☰`: the human (2026-10-30) would "slightly prefer if it was all normal text"
/// — some terminals draw those symbols as pictures.
pub(crate) const MENU_LABEL: &str = "[i] info";

/// The footer's one symbol, before the refresh's age and cadence (`↻ 7s ago · every 5s`), cyan
/// while a refresh runs. The human (2026-10-30), after asking for plain words: "no i think i like
/// them :) maybe even a small refresh icon or smth at the bottom right is good. its just if you
/// have too many then the text sizing becomes pretty unreliable" — so one, a single cell on
/// every terminal (`↻` has no emoji form), and the rest of the footer stays words.
const REFRESH_MARK: &str = "↻";

/// The footer, one line, cut at the terminal's edge, in plain words: [`MENU_LABEL`] first (a click
/// on it, or `i`, opens the info panel with everything that isn't here), then what must be read
/// before the next key — a live [`Notice`], a run going (`running: pods up …`), a `?` question out
/// with its seconds — then the refresh's short status (`ok · API 13/min`, `warn: …` in yellow), how
/// long ago it refreshed (`↻ 7s ago`, [`REFRESH_MARK`]; `refreshing` while it does) and its cadence
/// (`every 5s`), then the keys for the current mode: the way out first (`[q] quit`, `[esc] back`),
/// pairs merged.
fn footer_hint(shared: &Shared, ui: &Ui, secs: u64) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(MENU_LABEL, Style::default().add_modifier(Modifier::BOLD)), Span::raw(" · ")];
    // A notice (`D`'s prompt and what it skips, a finished action) comes first: the question's
    // line can be long, and the notice is what must be read before the next key.
    if let Some(n) = shared.notice.as_ref().filter(|n| n.at.elapsed() < NOTICE_FOR) {
        let style = if n.error { tone_style(Tone::Bad) } else { tone_style(Tone::Busy) };
        spans.extend([Span::styled(without_symbol(&n.text), style), Span::raw(" · ")]);
    }
    // A run going (`pods up`…) leads too: it bills, and `p` shows it.
    if let Some(r) = runs::footer(shared) {
        spans.push(Span::styled(r, tone_style(Tone::Busy)));
    }
    // A question out, counting, so it reads as alive: an answer can take a minute, and a line that
    // never changed looked like a hang.
    if let Some(asking) = shared.ask_running.as_ref() {
        let secs = shared.ask_since.map(|t| t.elapsed()).unwrap_or_default();
        spans.push(Span::styled(format!("{asking} {}s · ", secs.as_secs()), tone_style(Tone::Busy)));
    }
    let st = &shared.status;
    if !st.short.is_empty() {
        spans.push(match st.tone {
            Some(t) => Span::styled(st.short.clone(), tone_style(t)),
            None => Span::raw(st.short.clone()),
        });
        spans.push(Span::raw(" · "));
    }
    // While a refresh runs it says so and keeps the age: a slow refresh at a short cadence would
    // otherwise hide it most of the time (pre-merge review of #44).
    let refreshed = match (shared.last_refresh, shared.refreshing) {
        (Some(t), true) => format!("refreshing, last {}s ago", t.elapsed().as_secs()),
        (None, true) => "refreshing".into(),
        (Some(t), false) => format!("{}s ago", t.elapsed().as_secs()),
        (None, false) => "-".into(),
    };
    let mark = if shared.refreshing { tone_style(Tone::Busy) } else { Style::default() };
    spans.push(Span::styled(format!("{REFRESH_MARK} "), mark));
    spans.push(Span::raw(format!("{refreshed} · every {secs}s  ")));
    spans.push(Span::styled(mode_keys(&ui.mode).to_string(), Style::default().fg(Color::Gray)));
    Line::from(spans)
}

/// A notice's text without a leading `✗`/`✓`/`⚠`: its colour already says it, and the footer is
/// plain words ([`MENU_LABEL`]). Notices are written all over with those marks; the footer drops them.
fn without_symbol(text: &str) -> String {
    let t = text.trim_start();
    ["✗ ", "✓ ", "⚠️ ", "⚠ "].iter().find_map(|m| t.strip_prefix(m)).unwrap_or(t).to_string()
}

/// The footer's keys for a mode: the few most used, pairs merged — the info panel has them all
/// ([`ALL_KEYS`]). Modal popups keep their own.
pub(crate) fn mode_keys(mode: &Mode) -> &'static str {
    match mode {
        Mode::List => "[q] quit  [enter] detail  [a/A] act/all  [space/x] mark/clear  [d/D] deep  [c] ssh  [?] ask  [n] new  [p] runs  [b] backups",
        Mode::Detail => "[esc] back  [q] quit  [a/A] act/all  [space/x] mark/clear  [d/D] deep  [c] ssh  [?] ask  [n] new  [p] runs  [b] backups",
        Mode::Info { .. } => "[esc/i] close  [↑↓ PgUp PgDn] scroll",
        Mode::Menu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::Confirm(c) if c.action.requires_typed_name() => "type the pod's name  [enter] apply  [esc] cancel",
        Mode::Confirm(c) if c.preview.is_some() => "[y] apply  [v] full command  [n/esc] cancel",
        Mode::Confirm(_) => "[y] apply  [n/esc] cancel",
        Mode::FleetMenu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::FleetConfirm { .. } => "type the token to confirm  [enter] apply  [esc] cancel",
        Mode::NewPod { .. } => "[↑↓] field  [←→] change  [o] availability  [enter] review  [esc] cancel",
        Mode::UpConfirm(_) => "[y] create  [n/esc] back to the form",
        Mode::Runs { .. } => "[esc] hide (a run keeps going)  [←→] other runs  [↑↓ PgUp PgDn] scroll  [end] follow  [s] stop",
        Mode::Quit => "[k] quit, leave it running  [s] quit, send it Ctrl+C  [esc] stay",
        Mode::Input { .. } => "type the value  [enter] run  [esc] cancel",
        Mode::Select { .. } => "type a selection  [enter] mark  [esc] cancel",
        Mode::Working(_) => "working… (background) — please wait",
        Mode::Result(_) => "[↑↓] scroll  ·  [any other key] dismiss",
        Mode::Ask { .. } => "type the question  [enter] ask  [tab] the other AI  [ctrl-u] clear  [esc] cancel",
        Mode::Answer(_) => "[↑↓ PgUp PgDn] scroll  [esc] close",
        Mode::Backups(v) => backups::keys(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use arena_core::health::Status;
    use arena_core::metrics::PodMetrics;
    use arena_core::remote::FakeRemote;
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use arena_core::{balance, Pod};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use crate::state::{self, dashboard_snapshot, summarize, RefreshStatus};
    use crate::table::now_unix;
    use crate::tests::{cfg, draw, fixture_shared, live_views, pod, screen, ui, verdict, NOW};
    use crate::{demo, handle_mouse};

    /// The footer, in plain words: `[i] info` in its first cells, then a notice (`D`'s prompt),
    /// then a question's line with its seconds, then the short status, the refresh's age and
    /// cadence behind its one symbol (`↻`, cyan while refreshing), then the keys; one separator
    /// between each part, and no ☰ ✓ ⚠ or spinner.
    #[test]
    fn a_notice_leads_the_footer_before_the_asking_line() {
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // No start time: 0s and the first frame, whatever the clock does.
        let mut shared = Shared {
            ask_running: Some("asking claude about devtest-alpha…".into()),
            status: RefreshStatus::of("3 reporting · RunPod API 13/min".into(), &[], Some(13)),
            ..Default::default()
        };
        let text = |s: &Shared| footer_hint(s, &ui, 10).spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let quiet = text(&shared);
        assert!(quiet.starts_with("[i] info · asking claude about devtest-alpha… 0s · ok · API 13/min · ↻ - · every 10s  [q] quit  [enter] detail"), "{quiet}");
        shared.notify("D again to deep-check 2 running pods", false);
        let t = text(&shared);
        let rest = t.strip_prefix("[i] info · D again to deep-check 2 running pods · ").unwrap_or_else(|| panic!("{t}"));
        assert!(rest.starts_with("asking claude about devtest-alpha… 0s · ok · API 13/min"), "{t}");
        assert!(!t.contains("·  ·") && !t.contains("· ·"), "{t}");
        // A refresh that warned: its first words, yellow, and how many more.
        shared.status = RefreshStatus::of("3 reporting · ⚠ pod details incomplete: x · ⚠ ssh config not rewritten: y".into(), &["pod details incomplete: x", "ssh config not rewritten: y"], None);
        let line = footer_hint(&shared, &ui, 5);
        let warn = line.spans.iter().find(|s| s.content.starts_with("warn:")).unwrap_or_else(|| panic!("{line:?}"));
        assert_eq!((warn.content.as_ref(), warn.style.fg), ("warn: pod details incomplete +1", Some(Color::Yellow)));
        // A refresh under way says so, in words.
        shared.refreshing = true;
        let t = text(&shared);
        assert!(t.contains(" · ↻ refreshing · every 10s  "), "{t}");
        let mark = footer_hint(&shared, &ui, 10).spans.into_iter().find(|s| s.content.starts_with(REFRESH_MARK)).unwrap();
        assert_eq!(mark.style.fg, Some(Color::Cyan), "the mark says a refresh runs");
        shared.last_refresh = Some(Instant::now());
        assert!(text(&shared).contains(" · ↻ refreshing, last 0s ago · every 10s  "), "the age stays while it refreshes");
        let all = t.chars().filter(|c| !c.is_ascii()).collect::<String>();
        assert!(!all.chars().any(|c| "☰⟳✓✗⚠⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)), "only words: {all:?} in {t}");
        assert_eq!(t.matches(REFRESH_MARK).count(), 1, "one symbol: {t}");
    }

    /// Not a test: how long the view takes over the 60-pod demo fleet, whole and in parts (µs per
    /// frame, the median of 200) — what to cache. `cargo test --release -p arena-tui -- --ignored
    /// --nocapture view_cost`.
    #[test]
    #[ignore = "a measurement"]
    fn view_cost() {
        let shared = Mutex::new(Shared::default());
        let conf = cfg("");
        for round in 0..60 {
            demo::publish(&shared, &conf, 60, round);
        }
        let shared = shared.into_inner().unwrap();
        let ui = ui(conf, Arc::new(FakeRemote::new()));
        let median = |f: &mut dyn FnMut()| {
            let mut t: Vec<u128> = (0..200)
                .map(|_| {
                    let s = Instant::now();
                    f();
                    s.elapsed().as_micros()
                })
                .collect();
            t.sort_unstable();
            t[100]
        };
        for (w, h) in [(200u16, 50u16), (120, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let whole = median(&mut || {
                term.draw(|f| view(f, &shared, &ui, 5)).unwrap();
            });
            let table = median(&mut || {
                term.draw(|f| pods_table(f, &shared, &ui, f.area(), true)).unwrap();
            });
            let rest = median(&mut || {
                term.draw(|f| {
                    top_line(f, &shared, &ui, f.area());
                    f.render_widget(Paragraph::new(footer_hint(&shared, &ui, 5)), f.area());
                })
                .unwrap();
            });
            println!("{w}x{h}: view {whole}µs, pods_table {table}µs, summary+footer {rest}µs");
        }
    }

    /// Not a test: the README's picture of the dashboard — a made-up fleet of five, drawn by
    /// the renderer. `cargo test -p arena-tui -- --ignored --nocapture readme_picture`.
    #[test]
    #[ignore = "prints the README's picture"]
    fn readme_picture() {
        use arena_core::metrics::GpuStat;
        let gpu = |name: &str, util: u32, used: Option<u32>, total: Option<u32>, temp: u32| GpuStat {
            name: Some(name.into()),
            util_pct: Some(util),
            mem_used_mb: used,
            mem_total_mb: total,
            temp_c: Some(temp),
        };
        // name, provider, id, GPU type, $/h, nvidia-smi's GPUs, branch, saved (s ago), verdict,
        // checked (s ago), proxy port.
        type Row = (&'static str, &'static str, &'static str, &'static str, f64, Vec<GpuStat>, &'static str, i64, Status, u64, u16);
        let rows: [Row; 5] = [
            ("apple", "runpod", "rp1", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 87, Some(10_240), Some(16_376), 71)], "w1d2", 240, Status::Pass, 720, 22001),
            ("bloom", "runpod", "rp2", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 0, Some(900), Some(16_376), 34)], "w1d2", 780, Status::Pass, 3_000, 22002),
            ("cedar", "vast", "v3", "RTX 3090", 0.22, vec![gpu("RTX 3090", 0, Some(300), Some(24_576), 30)], "w1d2", 10_800, Status::Fail, 7_300, 22003),
            ("delta", "runpod", "rp4", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 42, Some(7_100), Some(16_376), 63)], "main", 480, Status::Pass, 900, 22004),
            ("ember", "runpod", "rp5", "NVIDIA B300 SXM6 AC MIG 1g.34gb", 0.89, vec![gpu("B300 SXM6 AC", 98, None, None, 78)], "w1d2", 120, Status::Pass, 600, 22005),
        ];
        let mut pods = Vec::new();
        let mut shared = Shared::default();
        let mut cache = HealthCache::new();
        for (i, (name, provider, id, ty, cost, gpus, branch, saved, verdict_status, checked, port)) in rows.into_iter().enumerate() {
            let mut p = pod(name, provider, id, i as u8 + 1, Some(port));
            p.gpu_type = Some(ty.into());
            p.gpu_count = Some(1);
            p.cost_per_hr = Some(cost);
            p.locked = Some(name == "delta");
            let branch = if branch == "main" { "main".to_string() } else { format!("autocommit-devtest-{branch}-{name}") };
            shared.metrics.insert(
                p.name.clone(),
                PodMetrics {
                    gpus,
                    branch: Some(branch),
                    origin: Some("git@github.com:example/ARENA_3.0.git".into()),
                    has_name: Some(true),
                    has_key: Some(true),
                    has_api_key: Some(true),
                    disk_used_mb: Some(40_960),
                    disk_total_mb: Some(100_352),
                    last_commit: Some(now_unix() - saved),
                    dirty_files: Some(0),
                    ..Default::default()
                },
            );
            cache.merge(&[verdict(&p, verdict_status, (verdict_status == Status::Fail).then_some(("cuda", "RuntimeError: Error 999")))], NOW - checked);
            pods.push(p);
        }
        use arena_core::proxy::{render_nginx, Forward};
        let fwd = |i: usize, p: &Pod| Forward {
            name: p.name.clone(),
            public_port: 9500 + i as u16,
            target_ip: p.ssh_ip.clone().unwrap(),
            target_port: p.ssh_port.unwrap() + u16::from(i == 2),
            provider: Some(p.provider.clone()),
            pod_id: None,
        };
        let proxy = render_nginx(&pods.iter().enumerate().map(|(i, p)| fwd(i, p)).collect::<Vec<_>>());
        let snap = dashboard_snapshot(&pods, &[], Some(&proxy), &cache, &Naming::from_config(&cfg("")), NOW);
        shared.summary = summarize(&pods, &shared.metrics);
        shared.publish(snap);
        shared.balance = live_views(&[(balance::Account::Runpod, 214.30, 1.17)]);
        shared.status = RefreshStatus::of("5 reporting · RunPod API 6/min".into(), &[], Some(6));
        shared.last_refresh = Some(Instant::now());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.config_path = "./config.env".into();
        println!("{}", draw(120, 11, &shared, &ui).join("\n"));
        ui.mode = Mode::Detail;
        ui.selected = 4;
        println!("{}", draw(120, 30, &shared, &ui).join("\n"));
    }

    /// The top line is one line — no title box — with the fleet in short words and, at its right
    /// while there's room, a provider or config that isn't the default (a second fleet's
    /// dashboard says so). The table starts right under it. A click anywhere on it opens the info
    /// panel (every word it leaves out), and another closes it.
    #[test]
    fn the_top_line_is_one_short_line_and_names_a_second_fleet() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(120, 12, &shared, &ui);
        assert!(lines[0].starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h "), "{}", lines[0]);
        assert!(lines[0].trim_end().ends_with(" /test/config.env"), "{}", lines[0]);
        assert!(lines[1].starts_with("┌Pods"), "{}", lines[1]);
        ui.provider_name = "vast".into();
        assert!(draw(120, 12, &shared, &ui)[0].trim_end().ends_with(" vast · /test/config.env"));
        let narrow = &draw(50, 12, &shared, &ui)[0];
        assert_eq!(narrow.trim_end(), " 4 pods │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h", "no room: the tag first, then the GPUs (the info panel has them)");
        ui.provider_name = "runpod".into();
        ui.config_path = DEFAULT_CONFIG.into();
        assert_eq!(draw(120, 12, &shared, &ui)[0].trim_end(), " 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h", "the defaults: nothing");
        let shared = Arc::new(Mutex::new(shared));
        let click = |column: u16, row: u16| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
        assert!(handle_mouse(click(30, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: false, .. }), "{:?}", ui.mode);
        draw(120, 12, &shared.lock().unwrap(), &ui);
        assert!(handle_mouse(click(100, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List), "{:?}", ui.mode);
        ui.mode = Mode::Detail;
        assert!(handle_mouse(click(3, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: true, .. }), "{:?}", ui.mode);
    }

    /// The top line's colour at the first cell of `text` (on its first row), and the line.
    fn top_colour(shared: &Shared, ui: &Ui, width: u16, text: &str) -> (Color, String) {
        let mut t = Terminal::new(TestBackend::new(width, 12)).unwrap();
        t.draw(|f| view(f, shared, ui, 5)).unwrap();
        let line = screen(t.backend().buffer())[0].clone();
        let x = line[..line.find(text).unwrap_or_else(|| panic!("{text:?} in {line}"))].chars().count() as u16;
        (t.backend().buffer()[(x, 0)].fg, line)
    }

    /// The prepaid balance in its provider's group, beside the burn of its pods there: the name
    /// green while it needs nothing; `⚠` in front and yellow when low, red when critical — at 80
    /// columns as at 120, its words never cut.
    #[test]
    fn the_top_line_shows_each_balance_in_its_providers_group() {
        use balance::Account;
        let mut shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let top = |shared: &Shared, w: u16| draw(w, 12, shared, &ui)[0].trim_end().to_string();
        // Nothing observed yet: no balance at all.
        assert!(top(&shared, 240).starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h "), "{}", top(&shared, 240));
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42)]); // ≈238h
        assert!(top(&shared, 240).starts_with(" 4 pods, 0 GPUs │ 3 RunPod $100.00 ≈9.9d · $0.42/h │ 1 Hetzner "), "{}", top(&shared, 240));
        assert_eq!(top_colour(&shared, &ui, 120, "RunPod").0, Color::Green);
        shared.balance = live_views(&[(Account::Runpod, 20.0, 0.42)]); // ≈47h, under 72
        for w in [80, 120] {
            let (colour, line) = top_colour(&shared, &ui, w, "⚠ 3 RunPod $20.00 ≈47h");
            assert_eq!(colour, Color::Yellow, "{w}: {line}");
        }
        shared.balance = live_views(&[(Account::Runpod, 5.0, 0.42)]); // ≈11h
        assert_eq!(top_colour(&shared, &ui, 80, "⚠ 3 RunPod $5.00 ≈11h").0, Color::Red, "critical is red");
        // RunPod at $0 with nothing billing is still the alarm (at $0 RunPod stops the pods).
        shared.balance = live_views(&[(Account::Runpod, 0.0, 0.0)]);
        assert_eq!(top_colour(&shared, &ui, 80, "⚠ 3 RunPod $0.00 no burn").0, Color::Red);
    }

    /// The demo fleet's top line (`ARENA_TUI_DEMO=51`, the default config) at 160 and 80 columns,
    /// as the terminal shows it: a group per provider and OpenRouter's, its warnings at both.
    #[test]
    fn the_demo_top_line_at_160_and_80_columns() {
        let shared = Mutex::new(Shared::default());
        demo::publish(&shared, &cfg(""), 51, 0);
        let shared = shared.into_inner().unwrap();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.config_path = DEFAULT_CONFIG.into();
        let at = |w: u16| draw(w, 12, &shared, &ui)[0].trim_end().to_string();
        assert_eq!(
            at(160),
            " 51 pods · 2 unreachable │ 43 RunPod $2262.90 · $11.91/h │ ⚠ 5 Vast $47.60 ≈40h · $1.19/h │ 3 Hetzner €0.048/h │ ⚠ OpenRouter key at cap · $422.09 left"
        );
        assert_eq!(at(80), " 2 unreachable │ RunPod │ ⚠ Vast $47.60 ≈40h │ Hetzner │ ⚠ OpenRouter key at cap");
    }

    /// Live (2026-10-15): with no Shadeform pods the bar led, in red, with `‼ shadeform balance no
    /// burn` — the opt-in overflow account, unfunded on purpose. With no pods there and nothing
    /// wrong it has no group on the top line (the info panel has it, `shadeform $0.00
    /// unfunded`); with a billing pod there it is the alarm again.
    #[test]
    fn an_unfunded_overflow_account_stays_off_the_top_line() {
        use balance::Account;
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42), (Account::Shadeform, 0.0, 0.0)]);
        let wide = draw(240, 12, &shared, &ui)[0].clone();
        assert!(!wide.contains("Shadeform") && !wide.contains('⚠'), "{wide}");
        assert_eq!(state::balance_tone(&shared.balance), None);
        ui.mode = Mode::Info { scroll: 0, detail: false };
        let info = draw(200, 60, &shared, &ui).join("\n");
        assert!(info.contains("balance: runpod $100.00 ≈9.9d · shadeform $0.00 unfunded"), "{info}");
        ui.mode = Mode::List;
        shared.balance = live_views(&[(Account::Shadeform, 0.0, 0.57)]);
        let (colour, line) = top_colour(&shared, &ui, 80, "⚠ Shadeform $0.00 <1h");
        assert_eq!(colour, Color::Red, "{line}");
    }

    /// A provider that failed to list has its group on the top line, in red, at any common width
    /// — the line is built to fit, and a warning is never what it leaves out.
    #[test]
    fn a_failed_listing_is_visible_on_a_narrow_terminal() {
        let mut shared = fixture_shared();
        shared.snap.partial = vec!["vast".into()];
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        for w in [80, 100, 120] {
            let (colour, line) = top_colour(&shared, &ui, w, "⚠ Vast: list failed");
            assert_eq!(colour, Color::Red, "{w}: {line}");
            assert!(line.starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ ⚠ Vast: list failed │ 1 Hetzner €0.006/h"), "{w}: {line}");
        }
    }

    /// The add-pod form's footer names the form's own keys (it said `[↑↓] pods  [+-] gpus  [←→]
    /// type` — keys the form doesn't have — under a form saying `[↑↓] field  [←→] change`).
    #[test]
    fn the_new_pod_footer_matches_the_form() {
        let shared = fixture_shared();
        let mut ui = ui(cfg("RUNPOD_API_KEY=x\n"), Arc::new(FakeRemote::new()));
        ui.mode = Mode::NewPod(newpod::build_form(&ui.cfg, "runpod", &[], true));
        let lines = draw(120, 40, &shared, &ui);
        let footer = lines.last().unwrap();
        assert!(footer.contains("[↑↓] field  [←→] change  [o] availability  [enter] review  [esc] cancel"), "{footer}");
        assert!(!footer.contains("[+-]") && !footer.contains("[↑↓] pods"), "{footer}");
        assert!(lines.join("\n").contains("[↑↓] field   [←→] change   [o] availability   [enter] review   [esc] cancel"));
    }
}
