//! The dashboard's `?`: ask claude or codex about the cursor pod — or the marked set, compared
//! in one prompt — what `arena pods ask` does (`arena_core::ask`: the same bundles, masking,
//! prompt, no-tools run and saved runs).
//!
//! `?` opens a one-line question, prefilled with the default; Enter asks **in the background**,
//! like `d`: the dashboard stays live, the footer says `asking claude about devtest-alpha…`, and
//! the answer opens in its own scrollable popup when it lands (on the list or detail view — a
//! modal open meanwhile isn't covered; the answer waits for it to close). Esc closes it. With
//! `ASK_AI=off` (or a malformed value) `?` opens nothing and the footer says how to turn it on.
//! The input says where the data goes before anything is sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use arena_core::ask::{self, Ai};
use arena_core::remote::Remote;
use arena_core::{Config, Pod};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{centered_rect, marked_pods, scope_names, scope_pods, scroll_by, InputScope, Mode, Scope, Shared, Ui};

/// An answer to show: the popup's title and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub title: String,
    pub text: String,
}

/// What answers a question about pods: [`real_asker`] (read the pods, run the AI, save the run)
/// — a fake in tests.
pub(crate) type Asker = Arc<dyn Fn(Vec<Pod>, String, Ai) -> Pin<Box<dyn Future<Output = Answer> + Send>> + Send + Sync>;

/// `devtest-alpha`, `devtest-alpha, devtest-bravo`, or `5 pods`.
fn about(pods: &[Pod]) -> String {
    match pods {
        [] => "no pods".to_string(),
        p if p.len() <= 3 => p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
        p => format!("{} pods", p.len()),
    }
}

/// The asker for real: the bundles over `remote`, one prompt, `ai` with no tools within
/// [`ask::DEFAULT_TIMEOUT`], the run saved under the state dir. Everything that goes wrong is
/// the answer's text (`✗ …`), never a crash; on first use the privacy note leads it.
pub(crate) fn real_asker(remote: Arc<dyn Remote>, cfg: Config) -> Asker {
    Arc::new(move |pods: Vec<Pod>, question: String, ai: Ai| {
        let (remote, cfg) = (remote.clone(), cfg.clone());
        Box::pin(async move { ask_now(&remote, &cfg, &pods, &question, ai).await })
    })
}

async fn ask_now(remote: &Arc<dyn Remote>, cfg: &Config, pods: &[Pod], question: &str, ai: Ai) -> Answer {
    let title = format!("{} · {}", ai.name(), about(pods));
    if let Err(e) = ask::redact::ready() {
        return Answer { title, text: format!("✗ can't mask the bundles ({e}) — nothing was read or sent") };
    }
    let now = arena_core::snapshot::unix_now();
    let runs = ask::runs_dir(cfg);
    let first = match &runs {
        Ok(dir) => ask::first_use(dir),
        Err(_) => true,
    };
    let bundles = ask::bundles(remote, cfg, pods, now).await;
    let prompt = ask::prompt(question, &bundles, cfg, now);
    let started = std::time::Instant::now();
    let outcome = ask::ai::ask(ai, &prompt, None, ask::DEFAULT_TIMEOUT, &ask::AiEnv::from_process()).await;
    let saved = runs.and_then(|dir| {
        let run = ask::Run { ai, model: None, pods, prompt: &prompt, outcome: &outcome, asked_at: now, took: started.elapsed() };
        ask::save_run(&dir, &run).map_err(|e| format!("{}: {e}", dir.display()))
    });
    let mut text = String::new();
    if first {
        text.push_str(&ask::privacy_note(ai));
        text.push_str("\n\n");
    }
    match &outcome {
        Ok(answer) => text.push_str(answer),
        Err(why) => text.push_str(&format!("✗ {why}")),
    }
    text.push_str(&match saved {
        Ok(dir) => format!("\n\n(saved: {})", dir.display()),
        Err(e) => format!("\n\n(⚠ the prompt and answer weren't saved: {e})"),
    });
    Answer { title, text }
}

/// `?` on the list or detail view: the question input for the marked set (when pods are
/// marked) or the cursor pod, prefilled with the default question. Opens nothing — saying why
/// in the footer — while another question is out, or with `ASK_AI` off or malformed.
fn open(ui: &mut Ui, shared: &mut Shared) {
    if let Some(running) = &shared.ask_running {
        let why = format!("still {running} — one question at a time");
        shared.notify(why, false);
        return;
    }
    let ai = match ask::ai::choose(&ui.cfg, None) {
        Ok(ai) => ai,
        Err(e) => {
            shared.notify(format!("✗ {e}"), true);
            return;
        }
    };
    let (scope, n) = if ui.marked.is_empty() {
        let Some(p) = shared.pods.get(ui.selected) else { return };
        (InputScope::Pod { name: p.name.clone(), id: p.id.clone() }, 1)
    } else {
        match marked_pods(&shared.pods, &ui.marked).len() {
            0 => {
                shared.notify("✗ none of the marked pods is listed now — refresh, or mark again", true);
                return;
            }
            n => (InputScope::Set(Scope::Marked), n),
        }
    };
    ui.mode = Mode::Ask { scope, value: ask::default_question(n), ai };
}

/// Ask in the background: the footer says so (`asking claude about …`) until the answer lands
/// in [`Shared::ask_answer`].
fn start(shared: &Arc<Mutex<Shared>>, ui: &Ui, pods: Vec<Pod>, question: String, ai: Ai) {
    shared.lock().unwrap().ask_running = Some(format!("asking {} about {}…", ai.name(), about(&pods)));
    let (shared, asker) = (shared.clone(), ui.asker.clone());
    tokio::spawn(async move {
        let answer = asker(pods, question, ai).await;
        let mut s = shared.lock().unwrap();
        s.ask_running = None;
        s.ask_answer = Some(answer);
    });
}

/// The keys this feature owns: `?` on the list/detail view, every key while the question is
/// being typed or an answer is open. `false`: not ours (the loop's own handling goes on).
pub(crate) fn key(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, code: KeyCode, mods: KeyModifiers) -> bool {
    match ui.mode.clone() {
        Mode::List | Mode::Detail if code == KeyCode::Char('?') => {
            open(ui, &mut shared.lock().unwrap());
            true
        }
        Mode::Ask { scope, mut value, ai } => {
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if !value.trim().is_empty() => {
                    let pods: Vec<Pod> = match &scope {
                        InputScope::Pod { name, id } => {
                            shared.lock().unwrap().pods.iter().filter(|p| p.id == *id && p.name == *name).take(1).cloned().collect()
                        }
                        InputScope::Set(sc) => scope_pods(shared, &ui.marked, *sc),
                    };
                    if pods.is_empty() {
                        shared.lock().unwrap().notify("✗ the pod isn't listed any more — refresh", true);
                    } else {
                        start(shared, ui, pods, value.trim().to_string(), ai);
                    }
                    ui.mode = Mode::List;
                }
                KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                    value.clear();
                    ui.mode = Mode::Ask { scope, value, ai };
                }
                KeyCode::Backspace => {
                    value.pop();
                    ui.mode = Mode::Ask { scope, value, ai };
                }
                KeyCode::Char(c) => {
                    value.push(c);
                    ui.mode = Mode::Ask { scope, value, ai };
                }
                _ => {}
            }
            true
        }
        Mode::Answer(_) => {
            // Never past the end of the text (`result_max`, set as it's drawn): PgDn there and
            // PgUp moves it back at once.
            let by = |ui: &Ui, delta| scroll_by(ui.result_scroll, ui.result_max.get(), delta);
            match code {
                KeyCode::Up | KeyCode::Char('k') => ui.result_scroll = by(ui, -1),
                KeyCode::Down | KeyCode::Char('j') => ui.result_scroll = by(ui, 1),
                KeyCode::PageUp => ui.result_scroll = by(ui, -10),
                KeyCode::PageDown | KeyCode::Char(' ') => ui.result_scroll = by(ui, 10),
                KeyCode::Home | KeyCode::Char('g') => ui.result_scroll = 0,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    ui.result_scroll = 0;
                    ui.mode = Mode::List;
                }
                _ => {}
            }
            true
        }
        _ => false,
    }
}

/// An answer that landed opens — from the list or detail view only (a modal open meanwhile
/// keeps it waiting until it closes). Called by the loop each round.
pub(crate) fn land(ui: &mut Ui, shared: &Arc<Mutex<Shared>>) {
    if !matches!(ui.mode, Mode::List | Mode::Detail) {
        return;
    }
    if let Some(answer) = shared.lock().unwrap().ask_answer.take() {
        ui.result_scroll = 0;
        ui.mode = Mode::Answer(answer);
    }
}

/// The question input: who it's about, where the data goes, the question as typed.
pub(crate) fn render_input(f: &mut Frame, shared: &Shared, ui: &Ui, scope: &InputScope, value: &str, ai: Ai) {
    let names = match scope {
        InputScope::Pod { name, .. } => vec![ui.shown_name(name)],
        InputScope::Set(sc) => scope_names(shared, ui, *sc),
    };
    let who = match names.as_slice() {
        [one] => one.clone(),
        many if many.len() <= 6 => many.join(", "),
        many => format!("{} … and {} more", many[..6].join(", "), many.len() - 6),
    };
    let text = format!(
        "ask {} about {who}\n\nEach pod is read once (read-only); its state — listing, GPU, disks, logins, the busiest \
         processes' arguments, the repo's changed file names, the newest job's last lines, keys and tokens masked — goes \
         to {} with no tools. The prompt and the answer are saved in arena's state dir.\n\n  > {value}\n\n\
         [enter] ask (in the background)  [ctrl-u] clear  [esc] cancel",
        ai.name(),
        ai.vendor()
    );
    let area = centered_rect(74, 45, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" ask ")),
        area,
    );
}

/// Lines `text` takes wrapped at `width` (close enough for the scroll limit).
fn wrapped_lines(text: &str, width: u16) -> u16 {
    let w = usize::from(width.max(1));
    let n: usize = text.lines().map(|l| l.chars().count().max(1).div_ceil(w)).sum();
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// The answer: a large popup, wrapped, scrolled by `scroll` (clamped to the end, which `max`
/// is set to for the keys).
pub(crate) fn render_answer(f: &mut Frame, answer: &Answer, scroll: u16, max: &std::cell::Cell<u16>) {
    let area = centered_rect(90, 85, f.area());
    f.render_widget(Clear, area);
    let inner = area.height.saturating_sub(2);
    let total = wrapped_lines(&answer.text, area.width.saturating_sub(2));
    max.set(total.saturating_sub(inner));
    let scroll = scroll.min(max.get());
    let color = if answer.text.contains('✗') { Color::Red } else { Color::Green };
    f.render_widget(
        Paragraph::new(answer.text.clone()).wrap(Wrap { trim: false }).scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title(format!(" {} — [↑↓ PgUp PgDn] scroll · [esc] close ", answer.title)),
        ),
        area,
    );
}
