//! The popups: the action menus, the confirms (and a set's confirm token), the input and `/` boxes, the
//! result; their sizes and scrolling.

use std::sync::{Arc, Mutex};

use arena_core::provider::ContainerReset;
use arena_core::Pod;
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::state::{action_summary, marked_pods, marked_set_token, Action, Confirm};
use crate::{aikey, InputScope, Scope, Shared, Ui};

/// The token the operator must type to confirm a multi-pod action: `ALL` for the whole
/// fleet (a deliberate high bar), and for terminate/restart on a marked set that is big or
/// is every listed pod (`state::marked_set_token` — `/first..last` marks a cohort in one
/// line); otherwise the pod count of the marked set (which they chose).
pub(crate) fn fleet_confirm_token(
    shared: &Arc<Mutex<Shared>>,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
    action: Action,
) -> String {
    fleet_confirm_token_str(&shared.lock().unwrap(), marked, scope, action)
}

/// A popup's scroll offset moved by `delta` lines, kept within `0..=max` (`max`: how far the
/// popup last drawn can scroll, [`Ui::result_max`]). An offset already past `max` — the window
/// grew since — counts as `max`, so the first key back moves the text at once.
pub(crate) fn scroll_by(scroll: u16, max: u16, delta: i32) -> u16 {
    let s = (i32::from(scroll.min(max)) + delta).clamp(0, i32::from(max));
    u16::try_from(s).unwrap_or(max)
}

/// The result modal's keys: ↑↓/jk a line, PgUp/PgDn ten, never past the end of the text.
/// `false`: any other key, which dismisses it.
pub(crate) fn result_key(ui: &mut Ui, code: KeyCode) -> bool {
    let delta = match code {
        KeyCode::Up | KeyCode::Char('k') => -1,
        KeyCode::Down | KeyCode::Char('j') => 1,
        KeyCode::PageUp => -10,
        KeyCode::PageDown => 10,
        _ => return false,
    };
    ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), delta);
    true
}

/// The free-text input modal for `run` / `set-branch` (one pod or a multi-pod scope) — also
/// their confirm: Enter runs it. On top, what Enter will do as typed so far ([`action_summary`]:
/// `alpha: switch to branch w3d2 (fast-forward only)`, the command itself for `run`; several
/// pods named once), wrapped by the widget.
pub(crate) fn render_input(f: &mut Frame, shared: &Shared, ui: &Ui, action: Action, scope: &InputScope, value: &str) {
    let names = match scope {
        InputScope::Pod { name, .. } => vec![name.clone()],
        InputScope::Set(sc) => scope_names(shared, ui, *sc),
    };
    // The raise-cap box: what the key spent of its cap and the new cap as typed — or why Enter
    // won't run it.
    let preview = match action {
        Action::KeyLimit => match aikey::limit_preview(&ui.ai_set, value) {
            Ok(lines) => format!("{}\n\n", lines.join("\n")),
            Err(why) => format!("✗ {}\n\n", why.join("\n")),
        },
        _ => String::new(),
    };
    let text = format!(
        "{}\n\n{preview}{}\n\n  > {value}\n\n[enter] run  [esc] cancel",
        action_summary(action, &names, Some(value), CONFIRM_NAMES_MAX).join("\n"),
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

/// The `/` modal: type a selection in the CLI's selector syntax.
pub(crate) fn render_select(f: &mut Frame, value: &str) {
    let text = format!(
        "mark pods by selector — the CLI's syntax:\n\n  names or ids     apple bloom   (or apple,bloom)\n  a range          apple..delta   (MACHINE_NAME_LIST order)\n  leave out        -x cloud   or   !cloud\n  narrow           --on runpod   --gpus 2\n\nReplaces the current marks; a typo marks nothing.\n\n  > {value}\n\n[enter] mark  [esc] cancel"
    );
    let area = centered_rect(64, 50, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" select "),
        ),
        area,
    );
}

/// A centered popup rect `pct_x` × `pct_y` percent of the screen.
pub(crate) fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
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

/// Render a navigable action menu: a title, one highlighted row per action (with its
/// key + description; destructive ones in red), and a key hint. `sel` is the cursor; `restart`
/// what a restart does to the disk of the pod(s) the menu is for (its label).
/// `ai_off`: why the AI key's rows can't act now (`aikey::pod_rows`) — they're greyed, with it.
/// An action menu: one row per action, `off[i]` (when set) greying row `i` out with why — what
/// it can't do there (the capability table's answer, [`menu_offs`]; the AI key's).
fn render_action_menu(f: &mut Frame, title: String, actions: &[(char, Action)], sel: usize, restart: ContainerReset, off: &[Option<String>]) {
    let mut lines: Vec<Line> = vec![Line::from(title), Line::from("")];
    for (i, (k, a)) in actions.iter().enumerate() {
        let selected = i == sel;
        let off = off.get(i).and_then(|o| o.as_deref());
        let base = if off.is_some() {
            Style::default().fg(Color::DarkGray)
        } else if a.is_risky() {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        let style = if selected {
            base.add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            base
        };
        let marker = if selected { "▶ " } else { "  " };
        let desc = off.map_or_else(|| a.desc(restart).to_string(), |why| format!("({})", why.split(" — ").next().unwrap_or(why)));
        lines.push(Line::styled(format!("{marker}[{k}] {:<10} {desc}", a.label()), style));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[↑↓] move   [enter] choose   [letter] pick   [esc] cancel",
        Style::default().fg(Color::DarkGray),
    ));
    // As tall as its lines (twelve actions), within the screen.
    let pct_y = ((lines.len() as u16 + 2) * 100 / f.area().height.max(1) + 2).clamp(40, 96);
    let area = centered_rect(58, pct_y, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" action ")),
        area,
    );
}

pub(crate) fn render_menu(f: &mut Frame, shared: &Shared, ui: &Ui, sel: usize) {
    let pod = shared.pods.get(ui.selected);
    let name = pod.map(|p| ui.shown_name(&p.name)).unwrap_or_else(|| "?".into());
    let (rows, ai_off) = aikey::pod_rows(shared, ui, pod);
    let off = menu_offs(shared, &rows, pod.into_iter().collect(), ai_off);
    render_action_menu(f, format!("Actions for {name}:"), &rows, sel, shared.restart_reset(pod), &off);
}

/// Why each of `rows` can't act on `pods` (`None`: it can): an AI key row, `ai_off`; an
/// operation's ([`Action::op`]), the capability table's refusal when NONE of the pods' providers
/// can do it (a set of some that can and some that can't is the pick's to sort: a stop skips
/// and names them, a restart or terminate refuses). Pure.
fn menu_offs(shared: &Shared, rows: &[(char, Action)], pods: Vec<&Pod>, ai_off: Option<String>) -> Vec<Option<String>> {
    rows.iter()
        .map(|(_, a)| {
            if a.ai_key() {
                return ai_off.clone();
            }
            let op = a.op()?;
            let whys: Vec<Option<&str>> = pods.iter().map(|p| shared.refusal(p, op)).collect();
            if whys.is_empty() || whys.iter().any(Option::is_none) {
                None
            } else {
                whys[0].map(String::from)
            }
        })
        .collect()
}

/// The single-pod confirm. What is about to happen — a plain summary of the action on the pod,
/// its warning, and (`v`) the full command — fills the top of the popup; the prompt (`[y]
/// apply`, or the typed-name field) sits in its own rows at the bottom, so it is always on
/// screen. Live (wave 6): Setup's preview — several long commands — wrapped past the popup's
/// bottom and took the `[y] apply` line with it; the body is now bounded by its rows.
pub(crate) fn render_confirm(f: &mut Frame, c: &Confirm) {
    let border = if c.action.is_destructive() { Color::Red } else { Color::Yellow };
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" confirm {} ", c.action.label()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let prompt = confirm_prompt(c);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(prompt.len() as u16)])
        .split(inner);
    let body = confirm_body(c, rows[0].width as usize, rows[0].height as usize);
    f.render_widget(Paragraph::new(body.join("\n")).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(Paragraph::new(prompt.join("\n")), rows[1]);
}

/// The confirm's bottom rows: the typed-name field, or the `[y]` keys (and `[v]` when there is
/// a full command to show). Pure.
fn confirm_prompt(c: &Confirm) -> Vec<String> {
    if c.action.requires_typed_name() {
        vec![
            "Type the pod name to confirm:".to_string(),
            format!("  > {}", c.typed),
            String::new(),
            "[enter] apply  [esc] cancel".to_string(),
        ]
    } else {
        let v = match (&c.preview, c.full) {
            (None, _) => "",
            (Some(_), false) => "   [v] full command",
            (Some(_), true) => "   [v] hide command",
        };
        vec![String::new(), format!("[y] apply{v}   [n/esc] cancel")]
    }
}

/// The confirm's body for an area `width` × `rows` (pure, so tested): what the action does to
/// the pod in plain words (`alpha: print the torch version (read-only)`, [`action_summary`])
/// and its warning, word-wrapped. The generated command is shown only on `v`
/// ([`Confirm::full`]): under `Will run:`, wrapped rather than clipped, and when it doesn't
/// fit in the rows left the last row says how many were left out. Live: the confirm opened
/// on `ssh -p 22192 -o BatchMode=yes -o StrictHostKeyChecking=no … -i /…`, clipped at the
/// width — the remote command never showed.
fn confirm_body(c: &Confirm, width: usize, rows: usize) -> Vec<String> {
    let width = width.max(2);
    let summary = action_summary(c.action, std::slice::from_ref(&c.pod_name), None, CONFIRM_NAMES_MAX);
    let mut out: Vec<String> = summary.iter().flat_map(|l| wrap_words(l, width)).collect();
    if let Some(note) = &c.note {
        out.push(String::new());
        out.extend(note.lines().flat_map(|l| wrap_words(l, width)));
    }
    if let Some(w) = c.warning() {
        out.push(String::new());
        out.extend(wrap_words(w, width));
    }
    let Some(p) = c.preview.as_ref().filter(|_| c.full) else { return out };
    out.push(String::new());
    out.push("Will run:".to_string());
    let room = rows.saturating_sub(out.len());
    let lines: Vec<String> = p.lines().flat_map(|l| wrap_command(l, width)).collect();
    let fits = if lines.len() <= room { lines.len() } else { room.saturating_sub(1) };
    out.extend(lines[..fits].iter().cloned());
    if fits < lines.len() {
        out.push(format!("… {} more line(s) not shown", lines.len() - fits));
    }
    out
}

/// `text` word-wrapped into rows of at most `width` characters (a word longer than a row is
/// cut), so the confirm knows exactly how many rows its text takes. Pure.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row: Vec<char> = Vec::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let mut word: Vec<char> = word.chars().collect();
        if !row.is_empty() && row.len() + 1 + word.len() > width {
            rows.push(row.drain(..).collect::<String>());
        }
        if !row.is_empty() {
            row.push(' ');
        }
        while row.len() + word.len() > width {
            let take = width - row.len();
            row.extend(word.drain(..take));
            rows.push(row.drain(..).collect::<String>());
        }
        row.extend(word);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row.into_iter().collect());
    }
    rows
}

/// One command line as rows of at most `width` characters, cut wherever the width runs out (a
/// command has no word breaks worth keeping), each continuation indented two spaces so one
/// command doesn't read as two. Pure.
fn wrap_command(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let indent = if width > 4 { 2 } else { 0 };
    let mut rest: Vec<char> = line.chars().collect();
    let mut rows = Vec::new();
    loop {
        let pad = if rows.is_empty() { 0 } else { indent };
        let take = rest.len().min(width - pad);
        let row: String = rest.drain(..take).collect();
        rows.push(format!("{}{row}", " ".repeat(pad)));
        if rest.is_empty() {
            return rows;
        }
    }
}

/// `(count, human label)` for a scope, e.g. `(19, "all 19 pods")` / `(3, "3 marked pods")`.
fn scope_label(shared: &Shared, marked: &std::collections::HashSet<String>, scope: Scope) -> (usize, String) {
    match scope {
        Scope::All => {
            let n = shared.pods.len();
            (n, format!("all {n} pods"))
        }
        Scope::Marked => {
            let n = marked_pods(&shared.pods, marked).len();
            (n, format!("{n} marked pod{}", if n == 1 { "" } else { "s" }))
        }
    }
}

/// The shown names of the pods a scope targets, in row order.
fn scope_names(shared: &Shared, ui: &Ui, scope: Scope) -> Vec<String> {
    scope_pods_of(shared, ui, scope).iter().map(|p| ui.shown_name(&p.name)).collect()
}

/// How many marked pods a marked-set confirm names before `… and N more`: enough to see
/// what a `/` selection caught (it can mark pods scrolled off screen) without pushing the
/// token prompt out of the fixed-size popup.
const CONFIRM_NAMES_MAX: usize = 10;

pub(crate) fn render_fleet_menu(f: &mut Frame, shared: &Shared, ui: &Ui, scope: Scope, sel: usize) {
    let (_, who) = scope_label(shared, &ui.marked, scope);
    let actions = Action::fleet_menu(scope == Scope::Marked, ui.ai_keys);
    let pods = scope_pods_of(shared, ui, scope);
    let ai_off = aikey::set_why(shared, &pods);
    let off = menu_offs(shared, &actions, pods.clone(), ai_off);
    render_action_menu(f, format!("Actions — {who}:"), &actions, sel, shared.restart_reset(pods), &off);
}

/// The listed pods `scope` covers, render-side (as [`scope_names`]).
pub(crate) fn scope_pods_of<'a>(shared: &'a Shared, ui: &Ui, scope: Scope) -> Vec<&'a Pod> {
    match scope {
        Scope::All => shared.pods.iter().collect(),
        Scope::Marked => marked_pods(&shared.pods, &ui.marked),
    }
}

/// The multi-pod confirm. What is about to happen — to which pods, named once (`/` can mark
/// pods scrolled off screen; up to [`CONFIRM_NAMES_MAX`] names), then what it does in plain
/// words ([`action_summary`]), its warning, and `note` (a stop's pods it will skip) — wraps in
/// the top of the popup and is clipped if it must be; the token prompt and what has been typed
/// sit in their own rows at the bottom, so they are always on screen.
pub(crate) fn render_fleet_confirm(f: &mut Frame, shared: &Shared, ui: &Ui, action: Action, typed: &str, scope: Scope, note: &str) {
    let n = scope_label(shared, &ui.marked, scope).0;
    let token = fleet_confirm_token_str(shared, &ui.marked, scope, action);
    // Warned as if each pod's provider wipes (the safe side); worded by the worst answer of
    // the set's backends, as its menu label.
    let reset = shared.restart_reset(scope_pods_of(shared, ui, scope));
    let warn = action.warning(true, reset).map(|w| format!("\n\n{w}")).unwrap_or_default();
    let summary = action_summary(action, &scope_names(shared, ui, scope), None, CONFIRM_NAMES_MAX).join("\n");
    // Why a marked set asks for ALL rather than its count (`state::marked_set_token`).
    let why = if scope == Scope::Marked && token == "ALL" {
        let what = if n >= shared.pods.len() { "every listed pod" } else { "a big set" };
        format!("\n\nThat is {what}: {} on it takes ALL, like the whole fleet.", action.label())
    } else {
        String::new()
    };
    let note = if note.is_empty() { String::new() } else { format!("\n\n{note}") };
    let border = if action.is_destructive() { Color::Red } else { Color::Yellow };
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" confirm {} ", action.label()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(5)])
        .split(inner);
    f.render_widget(
        Paragraph::new(format!("{summary}{warn}{why}{note}")).wrap(Wrap { trim: false }),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(format!("Type {token} to confirm:\n\n  > {typed}\n\n[enter] apply  [esc] cancel")),
        rows[1],
    );
}

/// Render-side confirm token (works off `&Shared`; `fleet_confirm_token` locks and calls it).
fn fleet_confirm_token_str(
    shared: &Shared,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
    action: Action,
) -> String {
    match scope {
        Scope::All => "ALL".to_string(),
        Scope::Marked => marked_set_token(action, scope_label(shared, marked, scope).0, shared.pods.len()),
    }
}

pub(crate) fn render_result(f: &mut Frame, msg: &str, scroll: u16, max: &std::cell::Cell<u16>) {
    // Grow the box for multi-line results (e.g. a per-pod fleet readout); it scrolls if
    // taller than the screen.
    let n = msg.lines().count() as u16;
    let pct_y = ((n + 4) * 100 / f.area().height.max(1) + 6).clamp(25, 85);
    let area = centered_rect(70, pct_y, f.area());
    f.render_widget(Clear, area);
    let color = if msg.contains('✗') { Color::Red } else { Color::Green };
    // Clamp scroll so you can't page past the end, and tell the keys where that is.
    let visible = area.height.saturating_sub(2);
    let max_scroll = n.saturating_sub(visible);
    max.set(max_scroll);
    let scroll = scroll.min(max_scroll);
    let footer = if n + 2 > visible { "  [↑↓] scroll · [esc] dismiss" } else { "" };
    f.render_widget(
        Paragraph::new(msg.to_string())
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .title(format!(" result {footer}")),
            ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::remote::FakeRemote;
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use crossterm::event::KeyModifiers;
    use crate::input::apply_select;
    use crate::ssh_actions::build_preview;
    use crate::state::{dashboard_snapshot, mark_key};
    use crate::tests::{cfg, draw, fixture_shared, pod, ui, NOW};
    use crate::{ask, Mode};

    /// The rows read the core snapshot: $/h in each provider's currency (`-` when not
    /// billing), the maintenance badge, the last deep check + its age (or `checking`), the
    /// proxy port + state; the summary bar the fleet total with € kept apart.
    /// Live (wave 6): the Setup confirm's preview — several long commands — wrapped past the
    /// popup and clipped its `[y] apply` line, and the footer said "type to confirm" for a
    /// `[y]` prompt. The prompt has its own rows now; the full command (shown on `v`) is
    /// wrapped within the rows left, the rest counted; and the footer names the keys the
    /// prompt takes.
    #[test]
    fn the_setup_confirm_always_shows_its_prompt() {
        let shared = Shared::default();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let long: Vec<String> = (0..30).map(|i| format!("ssh -p 22001 root@10.0.0.1 'step {i}: {}'", "x".repeat(200))).collect();
        let mut c = Confirm::new(Action::Setup, "devtest-alpha".into(), "id".into(), Some(long.join("\n")));
        c.full = true;
        ui.mode = Mode::Confirm(c);
        for (w, h) in [(120, 40), (80, 24), (60, 16)] {
            let all = draw(w, h, &shared, &ui).join("\n");
            assert!(all.contains("[y] apply") && all.contains("[n/esc] cancel"), "{w}x{h}:\n{all}");
            assert!(all.contains("more line(s) not shown") && all.contains("step 0:"), "{w}x{h}:\n{all}");
            assert!(!all.contains("type to confirm"), "{w}x{h}: the footer offers [y]:\n{all}");
        }
        // A short preview is shown whole.
        let mut c = Confirm::new(Action::Setup, "devtest-alpha".into(), "id".into(), Some("echo one\necho two".into()));
        c.full = true;
        ui.mode = Mode::Confirm(c);
        let all = draw(120, 40, &shared, &ui).join("\n");
        assert!(all.contains("echo two") && !all.contains("not shown") && all.contains("[y] apply"), "{all}");
        // A typed-name action keeps its field at the bottom, and its footer says so.
        ui.mode = Mode::Confirm(Confirm::new(Action::Terminate, "devtest-alpha".into(), "id".into(), None));
        let all = draw(100, 30, &shared, &ui).join("\n");
        assert!(all.contains("Type the pod name to confirm") && all.contains("type the pod's name"), "{all}");
        // The body (pure): the summary (2 rows at 40), then the commands wrapped to the width
        // (each 237–238 characters: 7 rows, 210 in all), the rows that don't fit counted.
        let mut c = Confirm::new(Action::Setup, "p".into(), "id".into(), Some(long.join("\n")));
        c.full = true;
        let body = confirm_body(&c, 40, 10);
        assert!(body.len() <= 10 && body.iter().all(|l| l.chars().count() <= 40), "{body:?}");
        assert_eq!(body[..2], ["p: provision (deploy key, SSH keys,", "repo, tools)"]);
        assert_eq!(body.last().unwrap(), "… 205 more line(s) not shown");
    }

    /// A marked set's confirm names the pods — `/` can mark ones scrolled off screen.
    #[test]
    fn a_marked_set_confirm_names_its_pods_and_the_footer_shows_a_notice() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.marked = ["runpod:rp1", "runpod:rp2"].map(String::from).into_iter().collect();
        ui.mode = Mode::FleetConfirm { action: Action::Terminate, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("2 pods: alpha, bravo") && all.contains("terminate (delete)"), "{all}");
        assert!(all.contains("Type 2 to confirm"), "{all}");

        ui.mode = Mode::List;
        shared.notify("✗ a target matched no pod — nothing was done: `alpah` …", true);
        let footer = draw(180, 12, &shared, &ui).pop().unwrap();
        assert!(footer.starts_with("[i] info · a target matched no pod"), "{footer}");
    }

    /// `n` pods `devtest-m00…`, RunPod, published as the dashboard shows them.
    fn cohort(n: usize) -> Shared {
        let pods: Vec<Pod> = (0..n).map(|i| pod(&format!("m{i:02}"), "runpod", &format!("rp{i}"), 1, None)).collect();
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        let mut shared = Shared::default();
        shared.publish(snap);
        shared
    }

    /// However many pods are marked and however long the warning, the token prompt and
    /// what has been typed stay on screen: the names are capped (`… and N more`) and the
    /// prompt has its own rows at the bottom of the popup.
    #[test]
    fn a_big_marked_set_confirm_keeps_the_token_prompt_on_screen() {
        let shared = cohort(20);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.short_names = false;
        ui.marked = shared.pods.iter().map(mark_key).collect();
        for (action, typed) in [(Action::Restart, "AL"), (Action::Terminate, "ALL"), (Action::Setup, "2")] {
            ui.mode = Mode::FleetConfirm { action, typed: typed.into(), scope: Scope::Marked, note: String::new() };
            for (w, h) in [(100, 30), (80, 24)] {
                let all = draw(w, h, &shared, &ui).join("\n");
                let token = if action.is_destructive() { "ALL" } else { "20" };
                assert!(all.contains(&format!("Type {token} to confirm:")), "{action:?} {w}x{h}:\n{all}");
                assert!(all.contains(&format!("> {typed}")), "{action:?} {w}x{h}: the typed echo:\n{all}");
                assert!(all.contains("[enter] apply"), "{action:?} {w}x{h}:\n{all}");
            }
            let wide = draw(160, 40, &shared, &ui).join("\n");
            assert!(wide.contains("devtest-m00, devtest-m01") && wide.contains("… and 10 more"), "{wide}");
        }
    }

    /// `/first..last` marks the whole cohort in one line, so the marked-set bar follows what
    /// is marked: terminate/restart on it ask for `ALL` (as `A` would), not the count; a
    /// small partial set keeps the count, and so do the safe actions.
    #[test]
    fn a_selector_marking_the_whole_fleet_needs_all_to_terminate() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        apply_select(&mut ui, &mut shared, "alpha..delta");
        assert_eq!(ui.marked.len(), 4);
        // The token is read off the shared state, as the key loop reads it.
        let shared = Arc::new(Mutex::new(shared));
        let token = |ui: &Ui, action| fleet_confirm_token(&shared, &ui.marked, Scope::Marked, action);
        assert_eq!(token(&ui, Action::Terminate), "ALL");
        assert_eq!(token(&ui, Action::Restart), "ALL");
        assert_eq!(token(&ui, Action::Setup), "4");
        assert_eq!(fleet_confirm_token(&shared, &ui.marked, Scope::All, Action::Setup), "ALL");
        apply_select(&mut ui, &mut shared.lock().unwrap(), "alpha,bravo");
        assert_eq!(token(&ui, Action::Terminate), "2");
        // The popup says why it wants ALL.
        apply_select(&mut ui, &mut shared.lock().unwrap(), "alpha..delta");
        ui.mode = Mode::FleetConfirm { action: Action::Terminate, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared.lock().unwrap(), &ui).join("\n");
        assert!(all.contains("That is every listed pod: terminate on it takes ALL") && all.contains("Type ALL"), "{all}");
    }

    /// Live ASK F3: both popups — the `?` answer and the older result modal — scroll no further
    /// than their text's end, so after PgDn past it one PgUp moves the text at once (it took
    /// 11 after 10 extra PgDn). The offset is clamped by `scroll_by` to what the popup last
    /// drawn can scroll; one that fits doesn't scroll at all.
    #[test]
    fn a_popup_scrolls_no_further_than_its_text() {
        assert_eq!([scroll_by(0, 5, -1), scroll_by(3, 5, 10), scroll_by(5, 5, -10), scroll_by(0, 0, 10), scroll_by(2, 5, 1)], [0, 5, 0, 0, 3]);
        assert_eq!(scroll_by(40, 5, -1), 4, "past the end (the window grew): the first key back moves");
        assert_eq!(scroll_by(u16::MAX, u16::MAX, 10), u16::MAX);
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let none = KeyModifiers::NONE;
        let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let shared_arc = Arc::new(Mutex::new(fixture_shared()));
        let answer = || Mode::Answer(ask::Answer { title: "t".into(), text: long.clone() });
        for (popup, mode) in [("answer", answer()), ("result", Mode::Result(long.clone()))] {
            ui.mode = mode;
            ui.result_scroll = 0;
            let press = |ui: &mut Ui, code: KeyCode| {
                if popup == "answer" {
                    assert!(ask::key(ui, &shared_arc, code, none));
                } else {
                    assert!(result_key(ui, code));
                }
            };
            draw(120, 40, &shared, &ui);
            let max = ui.result_max.get();
            assert!(max > 50 && max < 100, "{popup}: {max}");
            for _ in 0..30 {
                press(&mut ui, KeyCode::PageDown);
            }
            assert_eq!(ui.result_scroll, max, "{popup}: PgDn stops at the end");
            assert!(draw(120, 40, &shared, &ui).join("\n").contains("line 99"), "{popup}");
            press(&mut ui, KeyCode::Up);
            assert_eq!(ui.result_scroll, max - 1, "{popup}: one Up moves at once");
            press(&mut ui, KeyCode::PageUp);
            assert_eq!(ui.result_scroll, max - 11, "{popup}");
            for _ in 0..30 {
                press(&mut ui, KeyCode::PageUp);
            }
            assert_eq!(ui.result_scroll, 0, "{popup}");
            press(&mut ui, KeyCode::Down);
            assert_eq!(ui.result_scroll, 1, "{popup}");
        }
        // A result that fits: no scrolling, and any other key dismisses it (left to the loop).
        ui.mode = Mode::Result("✓ done".into());
        ui.result_scroll = 0;
        draw(120, 40, &shared, &ui);
        assert!(result_key(&mut ui, KeyCode::PageDown) && ui.result_scroll == 0);
        assert!(!result_key(&mut ui, KeyCode::Esc));
    }

    /// The confirm says what the action does in plain words — never the generated `ssh -p …
    /// -o … -i …` line (live: clipped at the width, it hid the remote command) — for every
    /// action that has a confirm; the full command is there on `v`, wrapped, not clipped. The
    /// input modal (run/set-branch's confirm) and a multi-pod confirm summarise the same way,
    /// naming several pods once.
    #[test]
    fn confirms_summarise_the_action_instead_of_the_ssh_line() {
        let shared = fixture_shared();
        let conf = cfg("ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\n");
        let mut ui = ui(conf, Arc::new(FakeRemote::new()));
        let alpha = shared.pods.iter().find(|p| p.id == "rp1").unwrap().clone();
        // (action, its summary)
        for (action, want) in [
            (Action::Test, "alpha: print the torch version (read-only)"),
            (Action::Backup, "alpha: commit + push the current branch"),
            (Action::Setup, "alpha: provision (deploy key, SSH keys, repo, tools)"),
            (Action::Restart, "alpha: restart in place (a hard reset)"),
            (Action::Stop, "alpha: stop (it can be started again)"),
            (Action::Terminate, "alpha: terminate (delete)"),
        ] {
            let preview = build_preview(&ui.cfg, action, &alpha);
            assert_eq!(preview.is_some(), !action.requires_typed_name(), "{action:?}");
            ui.mode = Mode::Confirm(Confirm::new(action, "alpha".into(), alpha.id.clone(), preview));
            let all = draw(100, 30, &shared, &ui).join("\n");
            assert!(all.contains(want), "{action:?}:\n{all}");
            assert!(!all.contains("ssh -p") && !all.contains("BatchMode") && !all.contains("Will run"), "{action:?}:\n{all}");
            if action.requires_typed_name() {
                assert!(all.contains("Type the pod name to confirm"), "{action:?}:\n{all}");
            } else {
                assert!(all.contains("[v] full command"), "{action:?}:\n{all}");
            }
        }
        // The warnings stay.
        ui.mode = Mode::Confirm(Confirm::new(Action::Terminate, "alpha".into(), alpha.id.clone(), None));
        assert!(draw(100, 30, &shared, &ui).join("\n").contains("⚠ IRREVERSIBLE"));
        // `v`: the full command, wrapped whole — its end (the python3 fallback) included.
        let mut c = Confirm::new(Action::Test, "alpha".into(), alpha.id.clone(), build_preview(&ui.cfg, Action::Test, &alpha));
        c.full = true;
        let body = confirm_body(&c, 50, 40);
        let at = body.iter().position(|l| l == "Will run:").unwrap();
        let rebuilt: String =
            body[at + 1..].iter().enumerate().map(|(i, l)| if i == 0 { l.as_str() } else { l.strip_prefix("  ").unwrap() }).collect();
        assert_eq!(Some(rebuilt), c.preview.clone());
        assert!(body.iter().all(|l| l.chars().count() <= 50), "{body:?}");
        ui.mode = Mode::Confirm(c);
        let all = draw(100, 30, &shared, &ui).join("\n");
        assert!(all.contains("Will run:") && all.contains("[v] hide command"), "{all}");
        // Several pods: named once, then what.
        ui.marked = ["runpod:rp1", "runpod:rp2", "hetzner:88"].map(String::from).into();
        ui.mode = Mode::FleetConfirm { action: Action::Backup, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("3 pods: alpha, bravo, charlie") && all.contains("commit + push the current branch"), "{all}");
        // run / set-branch: the input modal says what Enter does, the command verbatim.
        ui.mode = Mode::Input { action: Action::Run, scope: InputScope::Set(Scope::Marked), value: "nvidia-smi -L; hostname".into() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("3 pods: alpha, bravo, charlie") && all.contains("nvidia-smi -L; hostname"), "{all}");
        ui.mode = Mode::Input { action: Action::SetBranch, scope: InputScope::Pod { name: "alpha".into(), id: "rp1".into() }, value: "w3d2".into() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("alpha: switch to branch w3d2 (fast-forward only)"), "{all}");
    }

    /// The text wrappers the confirm counts its rows with (pure).
    #[test]
    fn wrap_words_and_wrap_command_table() {
        // (text, width, rows)
        let words: &[(&str, usize, &[&str])] = &[
            ("a bb ccc", 4, &["a bb", "ccc"]),
            ("a bb ccc", 8, &["a bb ccc"]),
            ("abcdefghij k", 4, &["abcd", "efgh", "ij k"]),
            ("", 4, &[""]),
        ];
        for (text, width, want) in words {
            assert_eq!(wrap_words(text, *width), *want, "{text:?} at {width}");
        }
        let cmds: &[(&str, usize, &[&str])] = &[
            ("echo hi", 10, &["echo hi"]),
            ("abcdefghijkl", 6, &["abcdef", "  ghij", "  kl"]),
            ("", 6, &[""]),
        ];
        for (line, width, want) in cmds {
            assert_eq!(wrap_command(line, *width), *want, "{line:?} at {width}");
        }
    }
}
