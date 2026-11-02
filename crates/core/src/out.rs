//! What a command prints: one line per event, the details behind `-v`, colour only on a
//! terminal. `docs/OUTPUT.md` is the guide; this is its mechanism.
//!
//! - [`out!`](crate::out!) / [`emit`]: a line, sent [`To`] stdout or stderr, always or only with
//!   (or only without) `-v`.
//! - [`ok`], [`fail`], [`warn`], [`bold`], [`dim`], …: styles. They colour only when a stream
//!   that's a terminal will show them ([`init`]); [`emit`] strips them from a stream that isn't
//!   one, so a pipe or a cron log gets the same words with no escape codes.
//! - [`reason`] / [`reason_text`]: a provider error as a few words (`no capacity`, `key
//!   rejected`); the HTTP status and body are for `-v`.
//! - [`capture`]: a test sees what a command printed, at default or at `-v`, colour off.
//!
//! The settings are the process's ([`init`], once, from `main`) — so a deep helper needs no
//! flag threaded to it — except on a thread that is [`capture`]-ing: tests run side by side,
//! each on its own thread, and each keeps its own.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Where a line goes (stdout or stderr), and at which verbosity it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum To {
    /// stdout, always: what will happen, what happened.
    Out,
    /// stderr, always: what's wrong (and the command that fixes it).
    Err,
    /// stdout, only with `-v`: plans, notes, ids, per-step chatter.
    Detail,
    /// stderr, only with `-v`: a raw provider error, a retry that will be retried again.
    ErrDetail,
    /// stdout, only without `-v`: the short form of what `-v`'s detail lines say at length.
    Brief,
    /// stderr, only without `-v`.
    ErrBrief,
}

impl To {
    /// It goes to stderr.
    pub fn is_err(self) -> bool {
        matches!(self, To::Err | To::ErrDetail | To::ErrBrief)
    }

    /// It shows at this verbosity.
    pub fn shows(self, verbose: bool) -> bool {
        match self {
            To::Out | To::Err => true,
            To::Detail | To::ErrDetail => verbose,
            To::Brief | To::ErrBrief => !verbose,
        }
    }

    /// The same level on stderr (a line that is trouble after all).
    pub fn err(self) -> To {
        match self {
            To::Out | To::Err => To::Err,
            To::Detail | To::ErrDetail => To::ErrDetail,
            To::Brief | To::ErrBrief => To::ErrBrief,
        }
    }
}

/// `--color`: when to colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// On a stream that's a terminal, unless `NO_COLOR` is set (or `TERM=dumb`).
    #[default]
    Auto,
    Always,
    Never,
}

impl std::str::FromStr for Color {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Color::Auto),
            "always" => Ok(Color::Always),
            "never" => Ok(Color::Never),
            _ => Err(format!("`{s}`: auto, always or never")),
        }
    }
}

/// Whether a stream gets colour: `always` and `never` as said; `auto` only on a terminal, with
/// `NO_COLOR` unset or empty (no-color.org) and `TERM` not `dumb`. Pure.
pub fn colors(choice: Color, terminal: bool, no_color: Option<&str>, term: Option<&str>) -> bool {
    match choice {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => terminal && no_color.is_none_or(str::is_empty) && term != Some("dumb"),
    }
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static COLOR_OUT: AtomicBool = AtomicBool::new(false);
static COLOR_ERR: AtomicBool = AtomicBool::new(false);
/// stderr is a terminal: an in-place [`progress`] line can be drawn there.
static LIVE: AtomicBool = AtomicBool::new(false);

/// The process's settings, from `main` (`-v`, `--color`). Until it's called — the dashboard,
/// tests — nothing is verbose or coloured.
pub fn init(verbose: bool, color: Color) {
    let no_color = std::env::var("NO_COLOR").ok();
    let term = std::env::var("TERM").ok();
    let (out, err) = (std::io::stdout().is_terminal(), std::io::stderr().is_terminal());
    VERBOSE.store(verbose, Ordering::SeqCst);
    COLOR_OUT.store(colors(color, out, no_color.as_deref(), term.as_deref()), Ordering::SeqCst);
    COLOR_ERR.store(colors(color, err, no_color.as_deref(), term.as_deref()), Ordering::SeqCst);
    // Both streams on the terminal: a line printed to a piped stdout would land below it.
    LIVE.store(out && err && term.as_deref() != Some("dumb"), Ordering::SeqCst);
}

/// `-v`: show the details.
pub fn verbose() -> bool {
    with_capture(|c| c.verbose).unwrap_or_else(|| VERBOSE.load(Ordering::SeqCst))
}

/// Whether styles make escape codes at all: some stream will show them.
fn painting() -> bool {
    with_capture(|c| c.color).unwrap_or_else(|| COLOR_OUT.load(Ordering::SeqCst) || COLOR_ERR.load(Ordering::SeqCst))
}

/// Print `line` [`To`] its stream, if it shows at this verbosity: escape codes stripped where
/// that stream gets no colour, a [`progress`] line on stderr cleared first and drawn again
/// after. Captured instead on a [`capture`]-ing thread.
pub fn emit(to: To, line: &str) {
    if with_capture(|c| c.lines.push((to, line.to_string()))).is_some() {
        return;
    }
    if !to.shows(verbose()) {
        return;
    }
    let colored = if to.is_err() { &COLOR_ERR } else { &COLOR_OUT };
    let text = if colored.load(Ordering::SeqCst) { line.to_string() } else { strip(line) };
    let progress = PROGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let drawn = !progress.is_empty() && LIVE.load(Ordering::SeqCst);
    if drawn {
        clear_progress();
    }
    // The macros, not the streams: a test harness captures what they print.
    if to.is_err() {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
    if drawn {
        draw_progress(&progress);
    }
}

/// `s` as it may be written to `to`'s stream by hand (a prompt, `print!` without a newline):
/// its escape codes stripped unless that stream gets colour.
pub fn for_stream(to: To, s: &str) -> String {
    let colored = if to.is_err() { &COLOR_ERR } else { &COLOR_OUT };
    if colored.load(Ordering::SeqCst) && !capturing() {
        s.to_string()
    } else {
        strip(s)
    }
}

/// `out!(To::Out, "…", args)` — `println!` that knows `-v`: [`emit`] a formatted line.
/// `out!(Detail, …)` names the variant alone too.
#[macro_export]
macro_rules! out {
    (Out, $($arg:tt)*) => { $crate::out::emit($crate::out::To::Out, &format!($($arg)*)) };
    (Err, $($arg:tt)*) => { $crate::out::emit($crate::out::To::Err, &format!($($arg)*)) };
    (Detail, $($arg:tt)*) => { $crate::out::emit($crate::out::To::Detail, &format!($($arg)*)) };
    (ErrDetail, $($arg:tt)*) => { $crate::out::emit($crate::out::To::ErrDetail, &format!($($arg)*)) };
    (Brief, $($arg:tt)*) => { $crate::out::emit($crate::out::To::Brief, &format!($($arg)*)) };
    (ErrBrief, $($arg:tt)*) => { $crate::out::emit($crate::out::To::ErrBrief, &format!($($arg)*)) };
    ($to:expr, $($arg:tt)*) => { $crate::out::emit($to, &format!($($arg)*)) };
}

// ---------------------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------------------

fn paint(code: &str, off: &str, s: impl std::fmt::Display) -> String {
    if painting() {
        format!("\x1b[{code}m{s}\x1b[{off}m")
    } else {
        s.to_string()
    }
}

/// Names and verdicts.
pub fn bold(s: impl std::fmt::Display) -> String {
    paint("1", "22", s)
}

/// Secondary detail.
pub fn dim(s: impl std::fmt::Display) -> String {
    paint("2", "22", s)
}

/// Done (never alone: a ✓ or a word says it too).
pub fn green(s: impl std::fmt::Display) -> String {
    paint("32", "39", s)
}

/// Failed (never alone).
pub fn red(s: impl std::fmt::Display) -> String {
    paint("31", "39", s)
}

/// Needs action (never alone).
pub fn yellow(s: impl std::fmt::Display) -> String {
    paint("33", "39", s)
}

/// `✓`, green.
pub fn tick() -> String {
    green("✓")
}

/// `✗`, red.
pub fn cross() -> String {
    red("✗")
}

/// `‼`, yellow: a warning that needs action.
pub fn bang() -> String {
    yellow("‼")
}

/// `✓ s`: done.
pub fn ok(s: impl std::fmt::Display) -> String {
    format!("{} {s}", tick())
}

/// `✗ s`: failed.
pub fn fail(s: impl std::fmt::Display) -> String {
    format!("{} {s}", cross())
}

/// `‼ s`: a warning that needs action.
pub fn warn(s: impl std::fmt::Display) -> String {
    format!("{} {s}", bang())
}

/// `s` without the escape codes the styles make (any CSI sequence). Pure.
pub fn strip(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.next() == Some('[') {
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------------------
// Provider errors as a reason
// ---------------------------------------------------------------------------------------

/// A provider error as the few words the default output says (`no capacity`, `rate limited`,
/// `key rejected`, `timed out`); the whole error — status, body — is for `-v`. Pure.
pub fn reason(e: &crate::Error) -> String {
    use crate::{Error, ProviderErrorKind as K};
    match e {
        Error::Provider { kind, message } => match kind {
            K::Capacity => "no capacity".into(),
            K::RateLimited => "rate limited".into(),
            K::Transient => with_status("provider error", message),
            K::Auth => "key rejected".into(),
            K::Denied => "no access to this pool".into(),
            K::Locked => "locked".into(),
            K::Rejected => "request rejected".into(),
            K::Quota => "over the account's limit".into(),
            K::Other => reason_text(message),
        },
        Error::Http(e) if e.is_timeout() => "timed out".into(),
        Error::Http(_) => "can't reach the provider".into(),
        Error::Timeout { .. } => "timed out".into(),
        Error::NotImplemented(_) => "not supported".into(),
        Error::Config(m) => first_clause(m),
    }
}

/// [`reason`] for an error that's only text (a placement outcome's, a step's stderr): what
/// its words say — capacity, a status that means a rejected key or throttling, a timeout —
/// else its first clause, with any HTTP status. Pure.
pub fn reason_text(message: &str) -> String {
    let lower = message.to_lowercase();
    if crate::error::looks_like_capacity(message) {
        return "no capacity".into();
    }
    match http_status(message) {
        Some(401 | 403) => return "key rejected".into(),
        Some(429) => return "rate limited".into(),
        Some(s @ 500..=599) => return format!("provider error (HTTP {s})"),
        _ => {}
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "timed out".into();
    }
    if crate::error::looks_like_auth_error(message) || lower.contains("unauthorized") {
        return "key rejected".into();
    }
    with_status(&first_clause(message), message)
}

/// `what (HTTP 400)` when `message` names a status.
fn with_status(what: &str, message: &str) -> String {
    match http_status(message) {
        Some(s) if !what.contains(&s.to_string()) => format!("{what} (HTTP {s})"),
        _ => what.to_string(),
    }
}

/// The status in `… HTTP 400 Bad Request …`.
fn http_status(message: &str) -> Option<u16> {
    let at = message.find("HTTP ")?;
    message[at + 5..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok().filter(|s| (100..600).contains(s))
}

/// Up to the first body, JSON or long tail: `create pod HTTP 400 Bad Request: {…}` →
/// `create pod HTTP 400 Bad Request`; at most 60 characters.
fn first_clause(message: &str) -> String {
    let m = message.trim().trim_start_matches("provider error: ");
    let cut = m.find([':', '{', '[', '\n']).unwrap_or(m.len());
    let head = m[..cut].trim().trim_end_matches(['.', ',', ';', '—', ' ']);
    let head = if head.is_empty() { m } else { head };
    if head.chars().count() > 60 {
        format!("{}…", head.chars().take(59).collect::<String>())
    } else {
        head.to_string()
    }
}

// ---------------------------------------------------------------------------------------
// In-place progress (a terminal's stderr only)
// ---------------------------------------------------------------------------------------

/// The progress line's parts, by key, drawn joined at the foot of a terminal.
static PROGRESS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The widest progress line drawn (a wider one would wrap, and couldn't be cleared in place).
const PROGRESS_WIDTH: usize = 79;

/// Set (`Some`) or take away (`None`) the part `key` of the line a terminal shows in place at
/// its foot — `round 3: no capacity · retrying in 60s` — until the next part replaces it.
/// Other output is printed above it. Nothing at all off a terminal (a pipe or log gets the
/// events' own lines), on a capturing thread, or with `-v` (every line is kept there).
pub fn progress(key: &str, text: Option<&str>) {
    if !live() {
        return;
    }
    let mut parts = PROGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = parts.iter().position(|(k, _)| k == key);
    match (text, at) {
        (Some(t), Some(i)) => parts[i].1 = t.to_string(),
        (Some(t), None) => parts.push((key.to_string(), t.to_string())),
        (None, Some(i)) => {
            parts.remove(i);
        }
        (None, None) => return,
    }
    clear_progress();
    draw_progress(&parts);
}

/// Whether a [`progress`] line can be drawn: stderr is a terminal, nothing is captured, and
/// it isn't `-v`.
pub fn live() -> bool {
    with_capture(|_| ()).is_none() && LIVE.load(Ordering::SeqCst) && !verbose()
}

/// Take the progress line away for good (before a prompt, at the end).
pub fn end_progress() {
    if !live() {
        return;
    }
    let mut parts = PROGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !parts.is_empty() {
        parts.clear();
        clear_progress();
    }
}

fn clear_progress() {
    let _ = write!(std::io::stderr(), "\r\x1b[2K");
}

fn draw_progress(parts: &[(String, String)]) {
    if parts.is_empty() {
        return;
    }
    let line = strip(&parts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" · "));
    let line: String = if line.chars().count() > PROGRESS_WIDTH {
        line.chars().take(PROGRESS_WIDTH - 1).chain(std::iter::once('…')).collect()
    } else {
        line
    };
    let line = if COLOR_ERR.load(Ordering::SeqCst) { dim(&line) } else { line };
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "{line}");
    let _ = err.flush();
}

// ---------------------------------------------------------------------------------------
// Capture (tests)
// ---------------------------------------------------------------------------------------

struct Captured {
    verbose: bool,
    color: bool,
    lines: Vec<(To, String)>,
    /// What a prompt is answered with, in order ([`ask`]).
    answers: VecDeque<String>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Arc<Mutex<Captured>>>> = const { RefCell::new(None) };
}

fn with_capture<T>(f: impl FnOnce(&mut Captured) -> T) -> Option<T> {
    let c = CAPTURE.with(|c| c.borrow().clone())?;
    let mut c = c.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(f(&mut c))
}

/// While it lives, what this thread [`emit`]s is kept here instead of printed, at `verbose`
/// with colour off (a test's command and the tasks it spawns on a current-thread runtime).
/// [`Capture::text`] is what a terminal would have shown.
#[must_use = "capturing stops when the guard is dropped"]
pub struct Capture {
    inner: Arc<Mutex<Captured>>,
    before: Option<Arc<Mutex<Captured>>>,
}

/// Start capturing this thread's output (see [`Capture`]).
pub fn capture(verbose: bool) -> Capture {
    let inner = Arc::new(Mutex::new(Captured { verbose, color: false, lines: Vec::new(), answers: VecDeque::new() }));
    let before = CAPTURE.with(|c| c.borrow_mut().replace(inner.clone()));
    Capture { inner, before }
}

impl Capture {
    fn lock(&self) -> std::sync::MutexGuard<'_, Captured> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the next prompt is answered with, as typed at a terminal ([`ask`]).
    pub fn answer(&self, reply: &str) {
        self.lock().answers.push_back(reply.to_string());
    }

    /// Every line sent, at every level, with where it went.
    pub fn lines(&self) -> Vec<(To, String)> {
        self.lock().lines.clone()
    }

    /// What a terminal would show at this capture's verbosity: the lines that show, both
    /// streams in order, styles stripped.
    pub fn text(&self) -> String {
        let c = self.lock();
        let mut s = String::new();
        for (_, line) in c.lines.iter().filter(|(to, _)| to.shows(c.verbose)) {
            s.push_str(&strip(line));
            s.push('\n');
        }
        s
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let before = self.before.take();
        CAPTURE.with(|c| *c.borrow_mut() = before);
    }
}

/// A prompt's answer on a [`capture`]-ing thread: the prompt and the answer are kept as one
/// line, as a terminal shows them. `None` when not capturing (the caller reads the terminal)
/// or no answer was given (the caller refuses, as with no terminal).
pub fn ask(prompt: &str) -> Option<String> {
    with_capture(|c| {
        let answer = c.answers.pop_front()?;
        c.lines.push((To::Err, format!("{prompt}{answer}")));
        Some(answer)
    })
    .flatten()
}

/// Whether this thread is capturing (a prompt then never reads the real terminal).
pub fn capturing() -> bool {
    with_capture(|_| ()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_only_on_a_terminal_without_no_color() {
        // (choice, terminal, NO_COLOR, TERM) → colour
        let cases = [
            (Color::Auto, true, None, Some("xterm-256color"), true),
            (Color::Auto, false, None, Some("xterm-256color"), false), // a pipe, a cron log
            (Color::Auto, true, Some("1"), Some("xterm"), false),      // NO_COLOR
            (Color::Auto, true, Some(""), Some("xterm"), true),        // NO_COLOR empty: unset
            (Color::Auto, true, None, Some("dumb"), false),
            (Color::Auto, true, None, None, true),
            (Color::Always, false, Some("1"), Some("dumb"), true),
            (Color::Never, true, None, Some("xterm"), false),
        ];
        for (choice, terminal, no_color, term, want) in cases {
            assert_eq!(colors(choice, terminal, no_color, term), want, "{choice:?} terminal={terminal} NO_COLOR={no_color:?} TERM={term:?}");
        }
        assert_eq!("never".parse::<Color>(), Ok(Color::Never));
        assert!("sometimes".parse::<Color>().is_err());
    }

    #[test]
    fn levels_show_at_their_verbosity() {
        let shown = |v: bool| -> Vec<To> {
            [To::Out, To::Err, To::Detail, To::ErrDetail, To::Brief, To::ErrBrief].into_iter().filter(|t| t.shows(v)).collect()
        };
        assert_eq!(shown(false), [To::Out, To::Err, To::Brief, To::ErrBrief]);
        assert_eq!(shown(true), [To::Out, To::Err, To::Detail, To::ErrDetail]);
        assert!(To::ErrBrief.is_err() && !To::Brief.is_err());
        assert_eq!(To::Detail.err(), To::ErrDetail);
    }

    #[test]
    fn styles_are_plain_with_colour_off_and_strip_takes_them_out() {
        let cap = capture(false);
        assert_eq!(ok("done"), "✓ done");
        assert_eq!(fail(bold("x")), "✗ x");
        assert_eq!(warn("low"), "‼ low");
        drop(cap);
        assert_eq!(strip("\x1b[1m\x1b[32m✓\x1b[39m a\x1b[22m b"), "✓ a b");
        assert_eq!(strip("plain — text\n"), "plain — text\n");
    }

    #[test]
    fn a_capture_keeps_every_level_and_renders_its_own() {
        let quiet = capture(false);
        crate::out!(Out, "[created] {}", "apple");
        crate::out!(ErrDetail, "[no capacity] apple: HTTP 400 {{…}}");
        crate::out!(Brief, "short");
        crate::out!(Detail, "long");
        assert_eq!(quiet.text(), "[created] apple\nshort\n");
        assert_eq!(quiet.lines().len(), 4);
        drop(quiet);
        let loud = capture(true);
        crate::out!(To::Out, "[created] apple");
        crate::out!(To::ErrDetail, "[no capacity] apple");
        crate::out!(To::Brief, "short");
        assert!(verbose());
        assert_eq!(loud.text(), "[created] apple\n[no capacity] apple\n");
        loud.answer("y");
        assert_eq!(ask("Create apple? [y/N] ").as_deref(), Some("y"));
        assert_eq!(ask("again? "), None, "no answer left: refused");
        assert!(loud.text().ends_with("Create apple? [y/N] y\n"));
    }

    #[test]
    fn provider_errors_become_a_reason() {
        use crate::{Error, ProviderErrorKind as K};
        let p = |kind, m: &str| Error::Provider { kind, message: m.into() };
        let body = r#"create pod HTTP 400 Bad Request: {"detail":"There are no longer any instances available with the requested specifications. Please refresh and try again.","status":400}"#;
        assert_eq!(reason(&p(K::Capacity, body)), "no capacity");
        assert_eq!(reason(&p(K::Other, body)), "no capacity", "the words say so");
        assert_eq!(reason(&p(K::Auth, "list pods HTTP 401 Unauthorized: {}")), "key rejected");
        assert_eq!(reason(&p(K::RateLimited, "HTTP 429")), "rate limited");
        assert_eq!(reason(&p(K::Transient, "create pod HTTP 502 Bad Gateway: <html>")), "provider error (HTTP 502)");
        assert_eq!(reason(&p(K::Quota, "resource_limit_exceeded")), "over the account's limit");
        assert_eq!(reason(&p(K::Other, r#"create pod HTTP 400 Bad Request: {"error":"bad disk"}"#)), "create pod HTTP 400 Bad Request");
        assert_eq!(reason(&Error::Timeout { what: "create".into(), after: std::time::Duration::from_secs(30) }), "timed out");
        assert_eq!(reason_text("provider error: create pod HTTP 403 Forbidden: {}"), "key rejected");
        assert_eq!(reason_text("ssh: connect to host 1.2.3.4 port 22: Connection timed out"), "timed out");
        assert_eq!(reason_text("weird"), "weird");
        assert!(reason_text(&"x".repeat(200)).chars().count() <= 60);
    }
}
