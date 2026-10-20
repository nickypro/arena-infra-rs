//! The dashboard's `?`: ask claude or codex about the cursor pod — or the marked set, compared
//! in one prompt — what `arena pods ask` does (`arena_core::ask`: the same bundles, masking,
//! prompt, no-tools run and saved runs).
//!
//! A marked set keeps to this cohort's pods, as `pods ask` does when no pod is named: a staff
//! box or another cohort's pod marked along is left out, and the input and the footer say how
//! many; the cursor pod alone is asked about whatever it is (chosen, as a named pod is).
//!
//! `?` opens a one-line question, prefilled with the default; Enter asks **in the background**,
//! like `d`: the dashboard stays live, the footer says `asking claude about devtest-alpha…`, and
//! the answer opens in its own scrollable popup when it lands (on the list or detail view — a
//! modal open meanwhile isn't covered; the answer waits for it to close). Esc closes it. With
//! `ASK_AI=off` (or a malformed value) `?` opens nothing and the footer says how to turn it on.
//! The input says where the data goes before anything is sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use arena_core::ask::{self, Ai};
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::{Config, Pod};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{centered_rect, marked_pods, scroll_by, InputScope, Mode, Scope, Shared, Ui};

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

/// The marked pods a question may be about — this cohort's only ([`Naming::is_cohort`]), as
/// `pods ask` narrows a selection that names no pod (review ai-review claude #9: a staff box or
/// another cohort's pod marked from the dashboard was read and sent along) — and the shown names
/// of the marked pods that leaves out.
fn cohort_marked(shared: &Shared, ui: &Ui) -> (Vec<Pod>, Vec<String>) {
    let naming = Naming::from_config(&ui.cfg);
    let (kept, out): (Vec<&Pod>, Vec<&Pod>) = marked_pods(&shared.pods, &ui.marked).into_iter().partition(|p| naming.is_cohort(&p.name));
    (kept.into_iter().cloned().collect(), out.into_iter().map(|p| ui.shown_name(&p.name)).collect())
}

/// `N non-cohort pod(s) left out` — for the input and the footer.
fn left_out(n: usize) -> String {
    format!("{n} non-cohort pod{} left out", if n == 1 { "" } else { "s" })
}

/// `?` on the list or detail view: the question input for the marked set's cohort pods (when
/// pods are marked) or the cursor pod, prefilled with the default question. Opens nothing —
/// saying why in the footer — while another question is out, with `ASK_AI` off or malformed, or
/// when no marked pod is this cohort's.
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
        let (pods, out) = cohort_marked(shared, ui);
        match (pods.len(), out.as_slice()) {
            (0, []) => {
                shared.notify("✗ none of the marked pods is listed now — refresh, or mark again", true);
                return;
            }
            (0, out) => {
                let why = format!("✗ none of the marked pods is this cohort's ({}) — put the cursor on one to ask about it alone", out.join(", "));
                shared.notify(why, true);
                return;
            }
            (n, _) => (InputScope::Set(Scope::Marked), n),
        }
    };
    ui.mode = Mode::Ask { scope, value: ask::default_question(n), ai };
}

/// Clears [`Shared::ask_running`] when dropped — however the question's task ends, a panic or
/// the runtime closing included — so `?` is never refused for good ("one question at a time";
/// review ai-review claude #4). A poisoned lock is taken all the same.
struct Running(Arc<Mutex<Shared>>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).ask_running = None;
    }
}

/// Ask in the background: the footer says so (`asking claude about …`, and how many marked pods
/// were left out, `left`) until the answer lands in [`Shared::ask_answer`].
fn start(shared: &Arc<Mutex<Shared>>, ui: &Ui, pods: Vec<Pod>, left: usize, question: String, ai: Ai) {
    let out = if left > 0 { format!(" ({})", left_out(left)) } else { String::new() };
    shared.lock().unwrap().ask_running = Some(format!("asking {} about {}{out}…", ai.name(), about(&pods)));
    let (shared, asker) = (shared.clone(), ui.asker.clone());
    tokio::spawn(async move {
        let running = Running(shared.clone());
        let answer = asker(pods, question, ai).await;
        shared.lock().unwrap_or_else(PoisonError::into_inner).ask_answer = Some(answer);
        drop(running);
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
                    let (pods, left): (Vec<Pod>, usize) = match &scope {
                        InputScope::Pod { name, id } => {
                            (shared.lock().unwrap().pods.iter().filter(|p| p.id == *id && p.name == *name).take(1).cloned().collect(), 0)
                        }
                        InputScope::Set(_) => {
                            let (pods, out) = cohort_marked(&shared.lock().unwrap(), ui);
                            (pods, out.len())
                        }
                    };
                    if pods.is_empty() {
                        shared.lock().unwrap().notify("✗ the pod isn't listed any more — refresh", true);
                    } else {
                        start(shared, ui, pods, left, value.trim().to_string(), ai);
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
    let (names, out) = match scope {
        InputScope::Pod { name, .. } => (vec![ui.shown_name(name)], Vec::new()),
        InputScope::Set(_) => {
            let (pods, out) = cohort_marked(shared, ui);
            (pods.iter().map(|p| ui.shown_name(&p.name)).collect(), out)
        }
    };
    let mut who = match names.as_slice() {
        [one] => one.clone(),
        many if many.len() <= 6 => many.join(", "),
        many => format!("{} … and {} more", many[..6].join(", "), many.len() - 6),
    };
    if !out.is_empty() {
        who.push_str(&format!(" — {}: {} (put the cursor on one to ask about it alone)", left_out(out.len()), out.join(", ")));
    }
    let text = format!(
        "ask {} about {who}\n\nEach pod is read once (read-only); its state — listing, GPU, disks, logins, the busiest \
         processes' and the jobs' commands (program, script and flag names, never their values), the repo's changed file \
         names, the newest job's last lines, keys and tokens masked — goes to {} with no tools. The prompt and the answer are saved in arena's state dir.\n\n  > {value}\n\n\
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

/// The answer: a large popup, wrapped, scrolled by `scroll` (clamped to the end, which `max`
/// is set to for the keys). The end is where the `Paragraph` itself ends it: its own count of
/// the lines it wraps the text into at the popup's inner width (`line_count`: word by word, a
/// wide character two columns — review ai-review codex #5: counting characters per line came
/// short of a long paragraph's, and its last lines couldn't be scrolled to).
pub(crate) fn render_answer(f: &mut Frame, answer: &Answer, scroll: u16, max: &std::cell::Cell<u16>) {
    let area = centered_rect(90, 85, f.area());
    f.render_widget(Clear, area);
    let inner = area.height.saturating_sub(2);
    let text = Paragraph::new(answer.text.clone()).wrap(Wrap { trim: false });
    let total = u16::try_from(text.line_count(area.width.saturating_sub(2))).unwrap_or(u16::MAX);
    max.set(total.saturating_sub(inner));
    let scroll = scroll.min(max.get());
    let color = if answer.text.contains('✗') { Color::Red } else { Color::Green };
    f.render_widget(
        text.scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title(format!(" {} — [↑↓ PgUp PgDn] scroll · [esc] close ", answer.title)),
        ),
        area,
    );
}
