//! The dashboard's `?`: ask claude or codex about the cursor pod — or the marked set, compared
//! in one prompt — what `arena pods ask` does (`arena_core::ask`: the same bundles, masking,
//! prompt, no-tools run and saved runs).
//!
//! A marked set keeps to this cohort's pods, as `pods ask` does when no pod is named: a staff
//! box or another cohort's pod marked along is left out, and the input and the footer say how
//! many; the cursor pod alone is asked about whatever it is (chosen, as a named pod is).
//!
//! `?` opens a one-line question, prefilled with the default; Tab switches between claude and
//! codex, each with its configured model and effort (`ASK_<AI>_MODEL`, `ASK_<AI>_EFFORT`), and the
//! next `?` opens with the one last picked. Enter asks **in the background**, like `d`: the
//! dashboard stays live, the footer says `asking claude about devtest-alpha…`, and the answer
//! opens in its own scrollable popup when it lands (on the list or detail view — a modal open
//! meanwhile isn't covered; the answer waits for it to close), its light Markdown rendered
//! ([`answer_lines`]). Esc closes it. With `ASK_AI=off` (or a malformed value) `?` opens nothing
//! and the footer says how to turn it on. The input says where the data goes before anything is
//! sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use arena_core::ask::{self, Pick};
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
pub(crate) type Asker = Arc<dyn Fn(Vec<Pod>, String, Pick) -> Pin<Box<dyn Future<Output = Answer> + Send>> + Send + Sync>;

/// `devtest-alpha`, `devtest-alpha, devtest-bravo`, or `5 pods`.
fn about(pods: &[Pod]) -> String {
    match pods {
        [] => "no pods".to_string(),
        p if p.len() <= 3 => p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
        p => format!("{} pods", p.len()),
    }
}

/// The asker for real: the bundles over `remote`, one prompt, the pick's AI with no tools within
/// [`ask::DEFAULT_TIMEOUT`], the run saved under the state dir. Everything that goes wrong is
/// the answer's text (`✗ …`), never a crash; on first use the privacy note leads it.
pub(crate) fn real_asker(remote: Arc<dyn Remote>, cfg: Config) -> Asker {
    Arc::new(move |pods: Vec<Pod>, question: String, pick: Pick| {
        let (remote, cfg) = (remote.clone(), cfg.clone());
        Box::pin(async move { ask_now(&remote, &cfg, &pods, &question, pick).await })
    })
}

async fn ask_now(remote: &Arc<dyn Remote>, cfg: &Config, pods: &[Pod], question: &str, pick: Pick) -> Answer {
    let title = format!("{} · {}", pick.label(), about(pods));
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
    let outcome = ask::ai::ask(&pick, &prompt, ask::DEFAULT_TIMEOUT, &ask::AiEnv::from_process()).await;
    let saved = runs.and_then(|dir| {
        let run = ask::Run { pick: &pick, pods, prompt: &prompt, outcome: &outcome, asked_at: now, took: started.elapsed() };
        ask::save_run(&dir, &run).map_err(|e| format!("{}: {e}", dir.display()))
    });
    let mut text = String::new();
    if first {
        text.push_str(&ask::privacy_note(pick.ai));
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
/// pods are marked) or the cursor pod, prefilled with the default question, for the AI last
/// picked (else `ASK_AI`'s) as the config sets it up. Opens nothing — saying why in the footer —
/// while another question is out, with `ASK_AI` off or malformed (or that AI's model or effort),
/// or when no marked pod is this cohort's.
fn open(ui: &mut Ui, shared: &mut Shared) {
    if let Some(running) = &shared.ask_running {
        let why = format!("still {running} — one question at a time");
        shared.notify(why, false);
        return;
    }
    let pick = match ask::ai::choose(&ui.cfg, ui.ask_ai).and_then(|ai| Pick::from_config(&ui.cfg, ai)) {
        Ok(pick) => pick,
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
    ui.mode = Mode::Ask { scope, value: ask::default_question(n), pick };
}

/// Clears [`Shared::ask_running`] when dropped — however the question's task ends, a panic or
/// the runtime closing included — so `?` is never refused for good ("one question at a time";
/// review ai-review claude #4). A poisoned lock is taken all the same.
struct Running(Arc<Mutex<Shared>>);

impl Drop for Running {
    fn drop(&mut self) {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        s.ask_running = None;
        s.ask_since = None;
    }
}

/// Ask in the background: the footer says so (`asking codex · gpt-6-luna about …`, and how many
/// marked pods were left out, `left`) until the answer lands in [`Shared::ask_answer`].
fn start(shared: &Arc<Mutex<Shared>>, ui: &Ui, pods: Vec<Pod>, left: usize, question: String, pick: Pick) {
    let out = if left > 0 { format!(" ({})", left_out(left)) } else { String::new() };
    {
        let mut s = shared.lock().unwrap();
        s.ask_running = Some(format!("asking {} about {}{out}…", pick.label(), about(&pods)));
        s.ask_since = Some(std::time::Instant::now());
    }
    let (shared, asker) = (shared.clone(), ui.asker.clone());
    tokio::spawn(async move {
        let running = Running(shared.clone());
        let answer = asker(pods, question, pick).await;
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
        Mode::Ask { scope, mut value, pick } => {
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                // The other AI, as the config sets it up — kept for the next `?` too.
                KeyCode::Tab | KeyCode::BackTab => match Pick::from_config(&ui.cfg, pick.ai.other()) {
                    Ok(other) => {
                        ui.ask_ai = Some(other.ai);
                        ui.mode = Mode::Ask { scope, value, pick: other };
                    }
                    Err(e) => shared.lock().unwrap().notify(format!("✗ {e}"), true),
                },
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
                        ui.ask_ai = Some(pick.ai);
                        start(shared, ui, pods, left, value.trim().to_string(), pick);
                    }
                    ui.mode = Mode::List;
                }
                KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                    value.clear();
                    ui.mode = Mode::Ask { scope, value, pick };
                }
                KeyCode::Backspace => {
                    value.pop();
                    ui.mode = Mode::Ask { scope, value, pick };
                }
                KeyCode::Char(c) => {
                    value.push(c);
                    ui.mode = Mode::Ask { scope, value, pick };
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

/// The question input: who it's about and which AI (Tab: the other), where the data goes in a
/// line, the question as typed — a popup as tall as that.
pub(crate) fn render_input(f: &mut Frame, shared: &Shared, ui: &Ui, scope: &InputScope, value: &str, pick: &Pick) {
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
    let text = vec![
        Line::from(vec![Span::raw("ask "), Span::styled(pick.label(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)), Span::raw(format!(" about {who}"))]),
        Line::styled(
            format!("read-only, keys masked, to {} with no tools; saved in arena's state dir", pick.ai.vendor()),
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
        Line::raw(format!("  > {value}")),
        Line::raw(""),
        Line::styled(
            format!("[enter] ask  [tab] {} instead  [ctrl-u] clear  [esc] cancel", pick.ai.other().name()),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    let screen = f.area();
    let wide = centered_rect(74, 100, screen);
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    let tall = u16::try_from(para.line_count(wide.width.saturating_sub(2))).unwrap_or(u16::MAX).saturating_add(2).min(screen.height);
    let area = Rect { y: screen.y + (screen.height - tall) / 2, height: tall, ..wide };
    f.render_widget(Clear, area);
    f.render_widget(para.block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" ask ")), area);
}

/// The answer: a wide popup of [`answer_lines`] at its inner width, as tall as they are (at most
/// 85% of the screen), scrolled by `scroll` (clamped to the end, which `max` is set to for the
/// keys). The lines are wrapped here, so their count is the text's height exactly (review
/// ai-review codex #5: a count that came short of a long paragraph's left its last lines out of
/// reach).
pub(crate) fn render_answer(f: &mut Frame, answer: &Answer, scroll: u16, max: &std::cell::Cell<u16>) {
    let screen = f.area();
    let most = centered_rect(90, 85, screen);
    let lines = answer_lines(&answer.text, usize::from(most.width.saturating_sub(2)));
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let tall = total.saturating_add(2).min(most.height);
    let area = Rect { y: screen.y + (screen.height - tall) / 2, height: tall, ..most };
    f.render_widget(Clear, area);
    let inner = area.height.saturating_sub(2);
    max.set(total.saturating_sub(inner));
    let scroll = scroll.min(max.get());
    let color = if answer.text.contains('✗') { Color::Red } else { Color::Green };
    f.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title(format!(" {} — [↑↓ PgUp PgDn] scroll · [esc] close ", answer.title)),
        ),
        area,
    );
}

/// How a run of an answer's text looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Plain,
    Bold,
    /// `` `…` `` and fenced blocks: an `arena` command, mostly.
    Code,
    /// `(saved: …)`: the popup's own note.
    Dim,
}

impl Look {
    fn style(self) -> Style {
        match self {
            Look::Plain => Style::default(),
            Look::Bold => Style::default().add_modifier(Modifier::BOLD),
            Look::Code => Style::default().fg(Color::Cyan),
            Look::Dim => Style::default().fg(Color::DarkGray),
        }
    }
}

/// Columns `s` takes on the terminal (a wide character two).
fn cols(s: &str) -> usize {
    Span::raw(s).width()
}

/// `text`'s runs: `**…**` bold, `` `…` `` code (no bold marker read inside one); a marker with no
/// closing one later on the line is text, and so is a `**` with a space on its inner side (`2 ** 3`),
/// as in Markdown. `base` is the look outside them.
fn inline(text: &str, base: Look) -> Vec<(String, Look)> {
    let (mut runs, mut cur): (Vec<(String, Look)>, String) = (Vec::new(), String::new());
    let (mut bold, mut code) = (false, false);
    let look = |bold: bool, code: bool| if code { Look::Code } else if bold { Look::Bold } else { base };
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let flip = if c == '`' && (code || rest[1..].contains('`')) {
            Some((1, true))
        } else if !code
            && rest.starts_with("**")
            && if bold { !cur.ends_with(char::is_whitespace) } else { rest[2..].starts_with(|c: char| !c.is_whitespace()) && rest[2..].contains("**") }
        {
            Some((2, false))
        } else {
            None
        };
        match flip {
            Some((n, is_code)) => {
                if !cur.is_empty() {
                    runs.push((std::mem::take(&mut cur), look(bold, code)));
                }
                if is_code {
                    code = !code;
                } else {
                    bold = !bold;
                }
                rest = &rest[n..];
            }
            None => {
                cur.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    if !cur.is_empty() {
        runs.push((cur, look(bold, code)));
    }
    runs
}

/// A list item's marker and the text after it: `- `, `* `, `+ `, `• ` (shown as `•`) or `12. `,
/// `3) `.
fn list_marker(line: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = line.strip_prefix(m) {
            return Some(("•".to_string(), rest));
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let after = &line[digits..];
    match after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
        Some(rest) if (1..=3).contains(&digits) => Some((line[..=digits].to_string(), rest)),
        _ => None,
    }
}

/// `runs` wrapped into lines of at most `width` columns, at spaces: the first line starts with
/// `lead`, every one after with as many spaces (a hanging indent — none when `lead` leaves less
/// than half the width). A word longer than a line is cut.
fn wrap(lead: &str, runs: &[(String, Look)], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let hang = if cols(lead) * 2 <= width { cols(lead) } else { 0 };
    // The words, each its runs (a word can change look midway: `` `cmd`, ``).
    let mut words: Vec<Vec<(String, Look)>> = vec![Vec::new()];
    for (text, look) in runs {
        for c in text.chars() {
            let word = words.last_mut().expect("never empty");
            if c == ' ' {
                if !word.is_empty() {
                    words.push(Vec::new());
                }
            } else if let Some((t, _)) = word.last_mut().filter(|(_, l)| l == look) {
                t.push(c);
            } else {
                word.push((c.to_string(), *look));
            }
        }
    }
    words.retain(|w| !w.is_empty());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line: Vec<Span<'static>> = vec![Span::raw(lead.to_string())];
    let (mut used, mut empty) = (cols(lead).min(width), true);
    let mut push_line = |line: &mut Vec<Span<'static>>, used: &mut usize, empty: &mut bool| {
        lines.push(Line::from(std::mem::replace(line, vec![Span::raw(" ".repeat(hang))])));
        (*used, *empty) = (hang, true);
    };
    for word in words {
        let w: usize = word.iter().map(|(t, _)| cols(t)).sum();
        if !empty && used + 1 + w > width {
            push_line(&mut line, &mut used, &mut empty);
        }
        if !empty {
            line.push(Span::raw(" "));
            used += 1;
        }
        for (text, look) in word {
            let mut piece = String::new();
            for c in text.chars() {
                let cw = cols(c.encode_utf8(&mut [0; 4]));
                if used + cw > width && (used > hang || !piece.is_empty()) {
                    line.push(Span::styled(std::mem::take(&mut piece), look.style()));
                    push_line(&mut line, &mut used, &mut empty);
                }
                piece.push(c);
                used += cw;
                empty = false;
            }
            line.push(Span::styled(piece, look.style()));
        }
        empty = false;
    }
    lines.push(Line::from(line));
    lines
}

/// An answer's text as the popup's lines at `width` columns: the light Markdown models write,
/// made readable — `**bold**` bold, `` `code` `` and fenced blocks cyan (the markers dropped),
/// `#` headings bold, a list item's `-`/`*` as `•` — and every line wrapped at spaces with a
/// hanging indent, so a bullet's next lines start under its text, not under its dash. Pure.
pub(crate) fn answer_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fenced = false;
    for raw in text.lines() {
        let raw = raw.replace('\t', "    ");
        let body = raw.trim_start();
        let indent = &raw[..raw.len() - body.len()];
        if body.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if body.is_empty() {
            out.push(Line::raw(""));
        } else if fenced {
            out.extend(wrap(indent, &[(body.to_string(), Look::Code)], width));
        } else if body.starts_with("(saved: ") || body.starts_with("(⚠ ") {
            out.extend(wrap(indent, &[(body.to_string(), Look::Dim)], width));
        } else if let Some(heading) = body.strip_prefix('#').map(|h| h.trim_start_matches('#')).and_then(|h| h.strip_prefix(' ')) {
            out.extend(wrap(indent, &inline(heading, Look::Bold), width));
        } else if let Some((marker, rest)) = list_marker(body) {
            out.extend(wrap(&format!("{indent}{marker} "), &inline(rest, Look::Plain), width));
        } else {
            out.extend(wrap(indent, &inline(body, Look::Plain), width));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each line's text.
    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    /// The style of the span reading `word` on `line`.
    fn style_of(line: &Line, word: &str) -> Style {
        line.spans.iter().find(|s| s.content == word).unwrap_or_else(|| panic!("no {word:?} in {line:?}")).style
    }

    /// The answer's Markdown is rendered, not shown: bold and code lose their markers and keep
    /// their look; an unclosed marker is text.
    #[test]
    fn bold_and_code_are_styled_without_their_markers() {
        let l = answer_lines("devtest-alpha: **busy**, run `arena pods backup devtest-alpha` now", 200);
        assert_eq!(texts(&l), ["devtest-alpha: busy, run arena pods backup devtest-alpha now"]);
        assert_eq!(style_of(&l[0], "busy"), Look::Bold.style());
        assert_eq!(style_of(&l[0], ","), Look::Plain.style());
        for w in ["arena", "pods", "backup", "devtest-alpha"] {
            assert_eq!(style_of(&l[0], w), Look::Code.style(), "{w}");
        }
        assert_eq!(style_of(&l[0], "now"), Look::Plain.style());
        assert_eq!(texts(&answer_lines("2 ** 3 and `x and a**b", 80)), ["2 ** 3 and `x and a**b"], "unclosed, or a space inside");
        assert_eq!(style_of(&answer_lines("a **b c** d", 80)[0], "c"), Look::Bold.style());
        assert_eq!(texts(&answer_lines("`a ** b` then **c**", 80)), ["a ** b then c"], "no bold read inside code");
    }

    /// A list item's wrapped lines start under its text; `-`/`*` show as `•`, numbers stay;
    /// a nested item keeps its indent; a plain indented line hangs at its indent.
    #[test]
    fn list_items_wrap_with_a_hanging_indent() {
        assert_eq!(texts(&answer_lines("- one two three four", 12)), ["• one two", "  three four"]);
        assert_eq!(texts(&answer_lines("* one two three four", 12)), ["• one two", "  three four"]);
        assert_eq!(texts(&answer_lines("12. alpha beta", 10)), ["12. alpha", "    beta"]);
        assert_eq!(texts(&answer_lines("   - sub item here", 12)), ["   • sub", "     item", "     here"]);
        assert_eq!(texts(&answer_lines("    arena pods logs x", 16)), ["    arena pods", "    logs x"]);
        assert_eq!(texts(&answer_lines("-not a bullet", 40)), ["-not a bullet"]);
        assert_eq!(texts(&answer_lines("2026 was a year", 40)), ["2026 was a year"], "a number without `. ` isn't one");
    }

    /// Headings are bold without their `#`; a fenced block is code, its fences dropped; blank
    /// lines stay; the popup's `(saved: …)` note is dim; `#0:` isn't a heading.
    #[test]
    fn headings_fences_blanks_and_the_saved_note() {
        let l = answer_lines("## Problems\n\n```sh\narena pods list\n```\n#0: util 97%\n(saved: /s/ask/x)", 80);
        assert_eq!(texts(&l), ["Problems", "", "arena pods list", "#0: util 97%", "(saved: /s/ask/x)"]);
        assert_eq!(style_of(&l[0], "Problems"), Look::Bold.style());
        assert_eq!(style_of(&l[2], "pods"), Look::Code.style());
        assert_eq!(style_of(&l[4], "/s/ask/x)"), Look::Dim.style());
    }

    /// No line is wider than the popup, whatever the width — a word longer than a line is cut,
    /// a wide character counts two — and nothing is lost but the spaces it broke at.
    #[test]
    fn no_line_is_wider_than_the_popup() {
        let text = "devtest-alpha — busy: **training** on its GPU (97%).\n\n- Git backup is BUSY during a merge: \
                    resolve it, then `arena pods backup devtest-alpha`.\n   - nested 日本語のテキスト here\n\
                    1. averyveryveryverylongwordwithoutanyspacesatall end\n```\ncode line that is long enough to wrap\n```";
        let words = |s: &str| s.replace(['*', '`', '•'], "").replace("```", "").split_whitespace().collect::<String>();
        for width in 8..90 {
            let lines = answer_lines(text, width);
            for l in &lines {
                assert!(l.width() <= width, "{width}: {:?} is {} wide", texts(std::slice::from_ref(l)), l.width());
            }
            assert_eq!(words(&texts(&lines).concat()), words(&text.replace("- ", "").replace("1. ", "1.")), "{width}");
        }
        let cut = texts(&answer_lines(&"x".repeat(25), 10));
        assert_eq!(cut, ["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
    }
}
