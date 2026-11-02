//! Keys and the mouse: what each does in each view and popup.

use std::io::Stdout;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use arena_core::provider::Provider;
use arena_core::selector::Naming;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use tokio::sync::Notify;

use crate::actions::{apply_action, choose_fleet_action, choose_pod_action, execute, execute_fleet, scope_pods, start_action, start_cli_action};
use crate::deep_check::{deep_all_confirm, deep_all_plan, skipped_words, start_deep_check, DEEP_ALL_CONFIRM};
use crate::info::info_key;
use crate::modals::{fleet_confirm_token, result_key, scroll_by};
use crate::prefs::Prefs;
use crate::state::{mark_key, select_marks, Action};
use crate::terminal::connect_ssh;
use crate::view::MENU_LABEL;
use crate::{aikey, ask, backups, newpod, runs, selected_pod, InputScope, Mode, Scope, Shared, Ui};

/// The cadences `f` cycles through (seconds).
const REFRESH_STEPS: &[u64] = &[2, 5, 10, 20, 60];

/// Mouse handling: wheel-scroll the pod list (or the result modal, or the info panel), and in
/// the list left-click a row to select it. Clicking the far-left of a row — or shift-clicking
/// anywhere on it — toggles that pod's multi-select mark (same `marked` set as `space`). A click
/// on the footer's `[i] info`, or anywhere on the top line (the info panel has every word it
/// leaves out), opens the info panel from the list or the detail view, and closes it.
/// `false`: nothing changed (a move, a click outside the rows) — no frame for it.
pub(crate) fn handle_mouse(m: MouseEvent, ui: &mut Ui, shared: &Arc<Mutex<Shared>>) -> bool {
    let in_list = matches!(ui.mode, Mode::List);
    let in_result = matches!(ui.mode, Mode::Result(_));
    // The footer's `[i] info` (its cells, [`MENU_LABEL`]) or the top line: opens the info panel
    // from the list or the detail view, and closes it again.
    let on_menu = ui.menu_at.get().is_some_and(|(x, y)| m.row == y && (x..x.saturating_add(MENU_LABEL.chars().count() as u16)).contains(&m.column))
        || ui.top_at.get() == Some(m.row);
    if let (MouseEventKind::Down(MouseButton::Left), true) = (m.kind, on_menu) {
        ui.mode = match ui.mode {
            Mode::List => Mode::Info { scroll: 0, detail: false },
            Mode::Detail => Mode::Info { scroll: 0, detail: true },
            Mode::Info { detail: true, .. } => Mode::Detail,
            Mode::Info { detail: false, .. } => Mode::List,
            _ => return false,
        };
        return true;
    }
    // The 💾 header: the backups screen.
    let on_saved = ui.backups.saved_at.get().is_some_and(|(x, y)| m.row == y && (x..x.saturating_add(2)).contains(&m.column));
    if let (MouseEventKind::Down(MouseButton::Left), true, Mode::List | Mode::Detail) = (m.kind, on_saved, &ui.mode) {
        backups::open(ui, shared);
        return true;
    }
    if let Mode::Backups(_) = ui.mode {
        return backups::mouse(m, ui, shared);
    }
    if let Mode::Info { scroll, detail } = ui.mode {
        let delta = match m.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return false,
        };
        ui.mode = Mode::Info { scroll: scroll_by(scroll, ui.result_max.get(), delta), detail };
        return true;
    }
    match m.kind {
        MouseEventKind::ScrollUp => {
            if in_result {
                ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), -1);
            } else if in_list {
                ui.selected = ui.selected.saturating_sub(1);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_result {
                ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), 1);
            } else if in_list {
                let len = shared.lock().unwrap().pods.len();
                if len > 0 {
                    ui.selected = (ui.selected + 1).min(len - 1);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if !in_list {
                return false;
            }
            let (area, offset) = ui.last_table.get();
            // Data rows sit below the top border (1) + header (1), above the bottom border.
            let rows_top = area.y.saturating_add(2);
            let rows_bot = area.y.saturating_add(area.height).saturating_sub(1);
            let (mx, my) = (m.column, m.row);
            if my < rows_top || my >= rows_bot || mx < area.x || mx >= area.x.saturating_add(area.width)
            {
                return false; // click outside the table rows
            }
            let idx = offset + (my - rows_top) as usize;
            let key = {
                let s = shared.lock().unwrap();
                if idx >= s.pods.len() {
                    return false;
                }
                mark_key(&s.pods[idx])
            };
            ui.selected = idx;
            // Far-left of the row, or shift-click, toggles the multi-select mark.
            let left_zone = mx < area.x.saturating_add(5);
            let shift = m.modifiers.contains(KeyModifiers::SHIFT);
            if (left_zone || shift) && !ui.marked.remove(&key) {
                ui.marked.insert(key);
            }
        }
        _ => return false,
    }
    true
}

/// Advance the cadence to the next step in `REFRESH_STEPS` (wrapping).
fn cycle_interval(interval: &AtomicU64) {
    let cur = interval.load(Ordering::Relaxed);
    let idx = REFRESH_STEPS.iter().position(|&s| s == cur).unwrap_or(usize::MAX);
    let next = REFRESH_STEPS[idx.wrapping_add(1) % REFRESH_STEPS.len()];
    interval.store(next, Ordering::Relaxed);
}

/// Enter in the `/` input: mark what `input` resolves to (`state::select_marks`), replacing
/// the current marks — not adding to them, so the marked set is exactly what was typed —
/// and say so in the footer. A typo marks nothing and leaves the existing marks as they
/// were (the error in red). Either way back to the list.
pub(crate) fn apply_select(ui: &mut Ui, shared: &mut Shared, input: &str) {
    match select_marks(input, &Naming::from_config(&ui.cfg), &shared.pods) {
        Ok((keys, msg)) => {
            ui.marked = keys;
            shared.notify(msg, false);
        }
        Err(e) => shared.notify(format!("✗ {e}"), true),
    }
    ui.mode = Mode::List;
}

/// What a key on the list or detail view leaves for the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListKey {
    /// `q`: leave the dashboard.
    Quit,
    /// Handled: the cursor, the marks, or which view or popup is open.
    Done,
    /// Not one of [`list_key`]'s: the loop's own (it needs the provider, the terminal or the
    /// shared state).
    Other,
}

/// The list and detail views' keys that need nothing but `ui` and the row count `len`. Only
/// `q` quits (and Ctrl+C, from anywhere); Esc steps back and never quits — from the detail view
/// to the list, and on the list it clears the marks if there are any, else does nothing. Live:
/// Esc on the list quit, so the Esc-Esc that backs out of a popup closed the dashboard.
pub(crate) fn list_key(ui: &mut Ui, code: KeyCode, len: usize) -> ListKey {
    match code {
        KeyCode::Char('q') => return ListKey::Quit,
        KeyCode::Esc if matches!(ui.mode, Mode::Detail) => ui.mode = Mode::List,
        KeyCode::Esc | KeyCode::Char('x') => ui.marked.clear(),
        KeyCode::Up | KeyCode::Char('k') => ui.selected = ui.selected.saturating_sub(1),
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
        KeyCode::Char('/') => ui.mode = Mode::Select { value: String::new() },
        KeyCode::Char('i') => ui.mode = Mode::Info { scroll: 0, detail: matches!(ui.mode, Mode::Detail) },
        // Act on the marked set if any are marked, else the cursor pod.
        KeyCode::Char('a') if !ui.marked.is_empty() => ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 },
        KeyCode::Char('a') if len > 0 => ui.mode = Mode::Menu { sel: 0 },
        KeyCode::Char('A') if len > 0 => ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 },
        KeyCode::Char('a' | 'A') => {}
        _ => return ListKey::Other,
    }
    ListKey::Done
}

/// Before `a`, `d` or `?` picks between the marked set and the cursor pod: clear the marks if
/// none of them is a listed pod. A mark outlives its pod (terminated, renamed, replaced) and
/// shows on no row, so a set of only those reads as nothing marked, and the key should act on
/// the cursor pod. Live (2026-11-02): pods marked and unmarked back to none on screen, and `a`
/// offered actions on "0 marked pods". Marks with a listed pod among them are kept as they
/// are.
pub(crate) fn forget_unlisted_marks(ui: &mut Ui, pods: &[arena_core::Pod]) {
    if !ui.marked.is_empty() && crate::state::marked_pods(pods, &ui.marked).is_empty() {
        ui.marked.clear();
    }
}

/// The dashboard's parts the key handling needs besides `ui`.
pub(crate) struct Ctx<'a> {
    pub shared: &'a Arc<Mutex<Shared>>,
    pub provider: &'a Arc<dyn Provider>,
    pub interval: &'a AtomicU64,
    pub nudge: &'a Notify,
    pub details_wanted: &'a AtomicBool,
}

/// What handling an event came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Flow {
    /// Nothing on screen changed (a mouse moving, a key with no use here).
    Same,
    /// Draw again.
    Changed,
    /// Leave the dashboard, saying these lines once the terminal is back (a run left running).
    Quit(Vec<String>),
}

/// One input event: a mouse event (a move changes nothing), a resize (draw again), a key press.
pub(crate) fn handle_event(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, ev: Event) -> Result<Flow> {
    Ok(match ev {
        Event::Mouse(m) => {
            if handle_mouse(m, ui, cx.shared) {
                Flow::Changed
            } else {
                Flow::Same
            }
        }
        Event::Resize(..) => Flow::Changed,
        // Key releases and repeats on terminals that send them: ignored.
        Event::Key(k) if k.kind == KeyEventKind::Press => return handle_key(terminal, cx, ui, k.code, k.modifiers),
        _ => Flow::Same,
    })
}

/// One key press, in whatever view or popup is open. Never awaits: a slow action is started
/// in the background ([`start_action`], `runs::start`, …) and lands through `Shared`.
fn handle_key(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, code: KeyCode, mods: KeyModifiers) -> Result<Flow> {
    let flow = handle_key_in(terminal, cx, ui, code, mods);
    // An action the backups screen started has ended (its result or confirm closed): back to it.
    backups::come_back(ui, cx.shared);
    flow
}

fn handle_key_in(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, code: KeyCode, mods: KeyModifiers) -> Result<Flow> {
    let Ctx { shared, provider, interval, nudge, details_wanted } = *cx;
    // Ctrl+C quits from any mode (raw mode swallows the usual SIGINT) — asking first while a run
    // that changes pods is going; at that question it quits, leaving the run going.
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        if matches!(ui.mode, Mode::Quit) {
            return Ok(match runs::key(ui, shared, KeyCode::Char('k')) {
                runs::Flow::Quit(notes) => Flow::Quit(notes),
                _ => Flow::Quit(Vec::new()),
            });
        }
        return Ok(if runs::quit_or_ask(ui, shared) { Flow::Quit(Vec::new()) } else { Flow::Changed });
    }
    if let Some(why) = demo_refuses(ui, code) {
        shared.lock().unwrap().notify(why, true);
        return Ok(Flow::Changed);
    }
    if let Some(changed) = info_key(ui, code) {
        return Ok(if changed { Flow::Changed } else { Flow::Same });
    }
    if matches!(ui.mode, Mode::List | Mode::Detail) && matches!(code, KeyCode::Char('a' | 'd' | '?')) {
        forget_unlisted_marks(ui, &shared.lock().unwrap().pods);
    }
    // `?` and its two popups (ask.rs); the add-pod form and its confirm (newpod.rs); the runs
    // panel, `p`, and the quit question (runs.rs).
    if ask::key(ui, shared, code, mods) || newpod::key(ui, shared, code) || backups::key(ui, shared, provider, code) {
        return Ok(Flow::Changed);
    }
    match runs::key(ui, shared, code) {
        runs::Flow::Handled => return Ok(Flow::Changed),
        runs::Flow::Quit(notes) => return Ok(Flow::Quit(notes)),
        runs::Flow::NotOurs => {}
    }
    let len = shared.lock().unwrap().pods.len();
    match ui.mode.clone() {
        Mode::List | Mode::Detail => match list_key(ui, code, len) {
            ListKey::Quit => return Ok(if runs::quit_or_ask(ui, shared) { Flow::Quit(Vec::new()) } else { Flow::Changed }),
            ListKey::Done => {}
            ListKey::Other => match code {
                KeyCode::Char('r') => {
                    // A manual refresh also re-reads the provider's details (maintenance
                    // windows, $/h) — rate-limited by `state::details_due`.
                    details_wanted.store(true, Ordering::Relaxed);
                    nudge.notify_one();
                }
                KeyCode::Char('d') => {
                    // Deep-check the marked set if any are marked, else the cursor pod —
                    // in the background; the rows say `checking` until it lands.
                    let pods = if ui.marked.is_empty() {
                        selected_pod(shared, ui.selected).into_iter().collect()
                    } else {
                        scope_pods(shared, &ui.marked, Scope::Marked)
                    };
                    if !pods.is_empty() {
                        start_deep_check(shared, ui, pods);
                    }
                }
                KeyCode::Char('D') => {
                    // Every running pod no one is using, after a second `D`: the first
                    // says how many, and which it leaves out. The second checks only pods
                    // the first named that still qualify — never one a refresh added since.
                    let (mut pods, skipped) = {
                        let s = shared.lock().unwrap();
                        deep_all_plan(&s.pods, &s.metrics)
                    };
                    let armed = ui.deep_all_armed.take().filter(|(t, _)| t.elapsed() < DEEP_ALL_CONFIRM);
                    let skipped = skipped_words(&skipped);
                    pods = deep_all_confirm(pods, armed.as_ref().map(|(_, named)| named));
                    if pods.is_empty() {
                        shared.lock().unwrap().notify(format!("nothing to deep-check{skipped}"), false);
                    } else if armed.is_some() {
                        start_deep_check(shared, ui, pods);
                    } else {
                        ui.deep_all_armed = Some((Instant::now(), pods.iter().map(mark_key).collect()));
                        let n = pods.len();
                        shared.lock().unwrap().notify(
                            format!(
                                "D again to deep-check {n} running pod{} (about a minute of GPU load each){skipped}",
                                if n == 1 { "" } else { "s" }
                            ),
                            false,
                        );
                    }
                }
                KeyCode::Char('s') => {
                    // Toggle short/full names and persist the choice.
                    ui.short_names = !ui.short_names;
                    Prefs { short_names: ui.short_names }.save();
                }
                KeyCode::Char('f') => {
                    cycle_interval(interval);
                    nudge.notify_one(); // apply a shorter cadence immediately
                }
                KeyCode::Char(' ') => {
                    // Toggle multi-select mark on the cursor pod.
                    if let Some(pod) = selected_pod(shared, ui.selected) {
                        let key = mark_key(&pod);
                        if !ui.marked.remove(&key) {
                            ui.marked.insert(key);
                        }
                    }
                }
                KeyCode::Char('c') => {
                    connect_ssh(terminal, shared, ui)?;
                    nudge.notify_one();
                }
                KeyCode::Char('n') => newpod::open(ui, shared),
                _ => return Ok(Flow::Same),
            },
        },
        Mode::Menu { mut sel } => {
            let rows = {
                let s = shared.lock().unwrap();
                aikey::pod_rows(&s, ui, s.pods.get(ui.selected)).0
            };
            let n = rows.len();
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    ui.mode = Mode::Menu { sel };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(n - 1);
                    ui.mode = Mode::Menu { sel };
                }
                KeyCode::Enter => choose_pod_action(shared, ui, provider, rows[sel.min(n - 1)].1),
                KeyCode::Char(ch) => match rows.iter().find(|(k, _)| *k == ch) {
                    Some((_, action)) => choose_pod_action(shared, ui, provider, *action),
                    None => ui.mode = Mode::Menu { sel },
                },
                _ => ui.mode = Mode::Menu { sel },
            }
        }
        Mode::Confirm(mut c) => {
            let typed = c.action.requires_typed_name();
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if c.is_satisfied() => apply_action(shared, ui, provider, &c),
                KeyCode::Char('y') if !typed => apply_action(shared, ui, provider, &c),
                KeyCode::Char('n') if !typed => ui.mode = Mode::List,
                KeyCode::Char('v') if !typed && c.preview.is_some() => {
                    c.full = !c.full;
                    ui.mode = Mode::Confirm(c);
                }
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
        Mode::FleetMenu { scope, mut sel } => {
            let actions = Action::fleet_menu(scope == Scope::Marked, ui.ai_keys);
            let n = actions.len();
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    ui.mode = Mode::FleetMenu { scope, sel };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(n - 1);
                    ui.mode = Mode::FleetMenu { scope, sel };
                }
                KeyCode::Enter => {
                    choose_fleet_action(shared, ui, provider, actions[sel.min(n - 1)].1, scope);
                }
                KeyCode::Char(ch) => match actions.iter().find(|(k, _)| *k == ch) {
                    Some((_, action)) => choose_fleet_action(shared, ui, provider, *action, scope),
                    None => ui.mode = Mode::FleetMenu { scope, sel },
                },
                _ => ui.mode = Mode::FleetMenu { scope, sel },
            }
        }
        Mode::FleetConfirm { action, mut typed, scope, note } => {
            let token = fleet_confirm_token(shared, &ui.marked, scope, action);
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if typed == token && action.ai_key() => {
                    let keyed = aikey::keyed(&std::mem::take(&mut ui.ai_set));
                    aikey::start(ui, shared, action, &keyed, None, None);
                }
                KeyCode::Enter if typed == token => {
                    let pods = scope_pods(shared, &ui.marked, scope);
                    let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
                    ui.mode = Mode::Working(format!("running {} on {} pod(s)…", action.label(), pods.len()));
                    start_action(shared, async move {
                        execute_fleet(&provider, &remote, &cfg, action, pods, None).await
                    });
                }
                KeyCode::Backspace => {
                    typed.pop();
                    ui.mode = Mode::FleetConfirm { action, typed, scope, note };
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    ui.mode = Mode::FleetConfirm { action, typed, scope, note };
                }
                _ => ui.mode = Mode::FleetConfirm { action, typed, scope, note },
            }
        }
        Mode::Input { action, scope, mut value } => match code {
            KeyCode::Esc => ui.mode = Mode::List,
            KeyCode::Enter if !value.trim().is_empty() => submit_input(shared, ui, provider, action, scope, value),
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
        Mode::Select { mut value } => match code {
            KeyCode::Esc => ui.mode = Mode::List,
            KeyCode::Enter => apply_select(ui, &mut shared.lock().unwrap(), &value),
            KeyCode::Backspace => {
                value.pop();
                ui.mode = Mode::Select { value };
            }
            KeyCode::Char(ch) => {
                value.push(ch);
                ui.mode = Mode::Select { value };
            }
            _ => ui.mode = Mode::Select { value },
        },
        // A background action is running — ignore keys (Ctrl+C still quits, above).
        Mode::Working(_) => {}
        // Handled above: `info_key`, `ask::key`, `newpod::key`, `runs::key`.
        Mode::Info { .. } | Mode::Ask { .. } | Mode::Answer(_) | Mode::NewPod(_) | Mode::UpConfirm(_) | Mode::Runs { .. } | Mode::Quit | Mode::Backups(_) => {}
        Mode::Result(_) => {
            if !result_key(ui, code) {
                ui.result_scroll = 0;
                ui.mode = Mode::List;
                nudge.notify_one();
            }
        }
    }
    Ok(Flow::Changed)
}

/// Enter in an input box: `action` with `value` on its pod or its set — one of the CLI's commands
/// as a run (`runs.rs`), the rest in the background. A cap the box shows as refused (an amount
/// that isn't one, a cap under the spend) stays in the box, which says why.
pub(crate) fn submit_input(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, action: Action, scope: InputScope, value: String) {
    if action.ai_key() {
        // The box's own pods and keys (`Ui::ai_set`), as previewed.
        if aikey::limit_preview(&ui.ai_set, &value).is_err() {
            ui.mode = Mode::Input { action, scope, value };
            return;
        }
        let keyed = aikey::keyed(&std::mem::take(&mut ui.ai_set));
        let own = match &scope {
            InputScope::Pod { id, .. } => shared.lock().unwrap().pods.iter().find(|p| &p.id == id).map(|p| p.provider.clone()),
            InputScope::Set(_) => None,
        };
        return aikey::start(ui, shared, action, &keyed, own.as_deref(), Some(&value));
    }
    let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
    match scope {
        InputScope::Pod { name, id } => {
            let pod = {
                let s = shared.lock().unwrap();
                s.pods.iter().find(|p| p.id == id).cloned()
            };
            match pod {
                None => ui.mode = Mode::Result(format!("{name} is gone — refresh")),
                Some(pod) if action.cli() => start_cli_action(ui, shared, action, &pod, Some(&value)),
                Some(pod) => {
                    ui.mode = Mode::Working(format!("{} on {name}…", action.label()));
                    start_action(shared, async move {
                        execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, Some(&value)).await
                    });
                }
            }
        }
        InputScope::Set(sc) => {
            let pods = scope_pods(shared, &ui.marked, sc);
            ui.mode = Mode::Working(format!("{} on {} pod(s)…", action.label(), pods.len()));
            start_action(shared, async move {
                execute_fleet(&provider, &remote, &cfg, action, pods, Some(value)).await
            });
        }
    }
}

/// In a demo (`ARENA_TUI_DEMO`) the keys that would act on a pod, reach one, ask about one or
/// create one are refused — with why, for the footer. The view's own keys all work.
fn demo_refuses(ui: &Ui, code: KeyCode) -> Option<&'static str> {
    if !ui.demo {
        return None;
    }
    // The add-pod form opens (it reads nothing); what it would run is refused by `runs.rs`.
    let refused = matches!(ui.mode, Mode::List | Mode::Detail) && matches!(code, KeyCode::Char('a' | 'A' | 'c' | 'd' | 'D' | '?'));
    refused.then_some("demo: the pods are made up — nothing to act on, reach or create")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use arena_core::remote::FakeRemote;
    use crate::tests::{cfg, draw, fixture_shared, ui};

    /// `/` then Enter, through the handler the key loop calls: a valid selection replaces the
    /// marks (not adds to them) and says what it marked; a typo leaves the marks as they were
    /// and says why in red; either way back to the list.
    #[test]
    fn select_enter_replaces_the_marks_and_a_typo_keeps_them() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let before: HashSet<String> = ["runpod:rp1", "hetzner:88"].map(String::from).into();
        ui.marked = before.clone();

        ui.mode = Mode::Select { value: "alpah".into() };
        apply_select(&mut ui, &mut shared, "alpah");
        assert_eq!(ui.marked, before, "a typo marks nothing and unmarks nothing");
        let n = shared.notice.clone().unwrap();
        assert!(n.error && n.text.starts_with("✗ ") && n.text.contains("alpah"), "{n:?}");
        assert!(matches!(ui.mode, Mode::List));

        ui.mode = Mode::Select { value: "bravo".into() };
        apply_select(&mut ui, &mut shared, "bravo");
        assert_eq!(ui.marked, HashSet::from(["runpod:rp2".to_string()]), "replaced, not added to");
        let n = shared.notice.clone().unwrap();
        assert_eq!((n.text.as_str(), n.error), ("marked 1 pod: bravo", false));
        assert!(matches!(ui.mode, Mode::List));
    }

    /// A mark whose pod left the list shows on no row, so with only such marks `a` acts on the
    /// cursor pod instead of offering actions on "0 marked pods"; a listed mark keeps the set.
    #[test]
    fn marks_of_pods_no_longer_listed_dont_count_as_marked() {
        let shared = fixture_shared();
        let pods = shared.pods.clone();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let gone = "runpod:terminated-earlier".to_string();
        ui.marked = HashSet::from([gone.clone(), "runpod:rp1".to_string()]);
        forget_unlisted_marks(&mut ui, &pods);
        assert_eq!(ui.marked.len(), 2, "one is listed: the set is kept as it is");
        ui.marked.remove("runpod:rp1"); // unmarked: nothing marked on screen
        forget_unlisted_marks(&mut ui, &pods);
        assert!(ui.marked.is_empty(), "only unlisted marks: cleared");
        list_key(&mut ui, KeyCode::Char('a'), pods.len());
        assert!(matches!(ui.mode, Mode::Menu { sel: 0 }), "the cursor pod's menu, not a marked set's");
    }

    /// Esc steps back and never quits: from the detail view to the list (the marks kept), and
    /// on the list it clears the marks, else does nothing; `q` quits, and the footer says so.
    /// Live: Esc on the list quit, so the Esc-Esc that backs out of a popup closed the dashboard.
    #[test]
    fn esc_steps_back_and_never_quits_and_q_does() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let marks: HashSet<String> = ["runpod:rp1".to_string()].into();
        ui.marked = marks.clone();
        ui.mode = Mode::Detail;
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List) && ui.marked == marks, "detail → list, the marks kept");
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List) && ui.marked.is_empty(), "on the list: the marks cleared");
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done, "nothing marked: nothing happens");
        assert!(matches!(ui.mode, Mode::List));
        assert_eq!(list_key(&mut ui, KeyCode::Char('q'), 4), ListKey::Quit);
        ui.mode = Mode::Detail;
        assert_eq!(list_key(&mut ui, KeyCode::Char('q'), 4), ListKey::Quit);
        // The keys that moved out of the loop with it still do what they did.
        ui.mode = Mode::List;
        ui.selected = 0;
        list_key(&mut ui, KeyCode::Down, 4);
        list_key(&mut ui, KeyCode::Char('j'), 4);
        list_key(&mut ui, KeyCode::Up, 4);
        assert_eq!(ui.selected, 1);
        list_key(&mut ui, KeyCode::Enter, 4);
        assert!(matches!(ui.mode, Mode::Detail));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('a'), 4);
        assert!(matches!(ui.mode, Mode::Menu { sel: 0 }));
        ui.mode = Mode::List;
        ui.marked = marks.clone();
        list_key(&mut ui, KeyCode::Char('a'), 4);
        assert!(matches!(ui.mode, Mode::FleetMenu { scope: Scope::Marked, .. }));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('A'), 4);
        assert!(matches!(ui.mode, Mode::FleetMenu { scope: Scope::All, .. }));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('x'), 4);
        assert!(ui.marked.is_empty());
        assert_eq!(list_key(&mut ui, KeyCode::Char('A'), 0), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List), "no pods: no fleet menu");
        list_key(&mut ui, KeyCode::Char('/'), 4);
        assert!(matches!(ui.mode, Mode::Select { .. }));
        ui.mode = Mode::List;
        assert_eq!(list_key(&mut ui, KeyCode::Char('r'), 4), ListKey::Other);
        // The footer says how to quit — first, as the line is cut at the terminal's width.
        for mode in [Mode::List, Mode::Detail] {
            ui.mode = mode;
            let footer = draw(80, 12, &shared, &ui).pop().unwrap();
            assert!(footer.contains("[q] quit"), "{footer}");
        }
    }
}
