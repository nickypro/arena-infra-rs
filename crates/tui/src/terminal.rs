//! The terminal: taking it (raw mode, the alternate screen, the mouse) and giving it back, handing it to
//! `c`'s ssh, and ending the dashboard at once on a hangup or a TERM.

use std::io::{stdout, Stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use arena_core::ssh::SshTarget;
use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

use crate::{selected_pod, Mode, Shared, Ui};

/// Whether the terminal is the dashboard's now — raw mode, the alternate screen, the mouse — so
/// an end from outside the loop puts it back ([`restore_best_effort`]); not while `c`'s ssh has
/// it ([`hand_over`]). Held while the terminal changes hands ([`setup_terminal`],
/// [`restore_terminal`]), while a frame is drawn ([`while_ours`]) and while that end puts it back:
/// so none of them interleaves with another, no frame is drawn once it's back (on the main
/// screen), and once the dashboard is ending (`arena_core::remote::ending`) it isn't taken again —
/// `c`'s ssh stopped by the end, the dashboard doesn't set up its screen again under the exit.
static TERMINAL: Mutex<bool> = Mutex::new(false);

pub(crate) fn terminal_ours() -> std::sync::MutexGuard<'static, bool> {
    TERMINAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take the terminal: raw mode, the alternate screen, the mouse. Refused once the dashboard is
/// ending.
pub(crate) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut ours = terminal_ours();
    if arena_core::remote::ending() {
        anyhow::bail!("the dashboard is ending");
    }
    *ours = true;
    enable_raw_mode()?;
    let mut out = stdout();
    // Clicks and the wheel: scroll the list, click to select/multi-select rows.
    execute!(out, EnterAlternateScreen, MouseClicks)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Mouse reporting for clicks, drags and the wheel only: crossterm's `EnableMouseCapture` also
/// asks for every motion (mode 1003), so moving the pointer over the dashboard sent a stream of
/// events (each one a frame, then) — and over SSH, their bytes. `DisableMouseCapture` turns
/// every mode off again.
struct MouseClicks;

impl crossterm::Command for MouseClicks {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        // 1000: presses and releases (the wheel too); 1002: motion only with a button down;
        // 1006: SGR coordinates (any column).
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::EnableMouseCapture.execute_winapi()
    }
}

/// Give the terminal back. `false`: it wasn't the dashboard's — an end from outside the loop has
/// put it back already — and nothing is written.
pub(crate) fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<bool> {
    let mut ours = terminal_ours();
    if !std::mem::replace(&mut *ours, false) {
        return Ok(false);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(true)
}

/// `f` on the terminal — a frame, a clear — while it is the dashboard's ([`TERMINAL`], held
/// meanwhile). `false`, `f` not called: it isn't — an end from outside the loop has put it back.
fn while_ours(terminal: &mut Terminal<CrosstermBackend<Stdout>>, f: impl FnOnce(&mut Terminal<CrosstermBackend<Stdout>>) -> std::io::Result<()>) -> Result<bool> {
    let ours = terminal_ours();
    if !*ours {
        return Ok(false);
    }
    f(terminal)?;
    Ok(true)
}

/// How long [`restore_best_effort`] waits for the terminal to take its last writes.
const RESTORE_WAIT: Duration = Duration::from_millis(300);

/// The terminal put back as [`restore_terminal`] does — raw mode off, the main screen, no mouse
/// capture, the cursor shown — when the dashboard ends from outside its loop: best effort (on a
/// hung-up terminal every step fails, and that's fine), on a thread of its own waited for no more
/// than [`RESTORE_WAIT`] (a terminal that has stopped reading would hold a write for ever, or a
/// frame's draw the lock). Nothing when the terminal isn't the dashboard's ([`TERMINAL`]): `c`'s
/// ssh has it, and what ssh did to it is ssh's to undo.
fn restore_best_effort() {
    let (done, wait) = std::sync::mpsc::channel();
    let restoring = std::thread::Builder::new().name("restore".into()).spawn(move || {
        let mut ours = terminal_ours();
        if std::mem::replace(&mut *ours, false) {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture, crossterm::cursor::Show);
        }
        drop(ours);
        let _ = done.send(());
    });
    if restoring.is_ok() {
        let _ = wait.recv_timeout(RESTORE_WAIT);
    }
}

/// The signals that end the dashboard ([`exit_on_signals`]), by number: the exit status is 128 +
/// it, as a shell reports a process the signal killed (the same on Linux and macOS).
const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;

/// How often [`exit_on_hangup`] looks at the terminal.
const HANGUP_POLL: Duration = Duration::from_millis(200);

/// End the dashboard now, from outside its event loop — which may never come round again: on a
/// hung-up terminal crossterm's read spins for ever (live ASK4 3c) — so nothing here waits on it.
/// In order:
/// 1. the latch (`arena_core::remote::end_children`), before anything else: no child starts after
///    it (review ai-review codex #3), and a later cause — the hangup's SIGHUP and the dead terminal
///    both seen, a TERM after a HUP, the loop failing on the dead terminal — waits for this one's
///    exit;
/// 2. the terminal put back as far as it can be ([`restore_best_effort`]), at once, never after
///    the grace (review ai-review claude #4), and `why` said there;
/// 3. stdin parked (`arena_core::tty::park_stdin`): the loop's next read waits, no longer spinning
///    a core on a dead terminal meanwhile (live ASK5 F1);
/// 4. every child still running stopped — the AI's whole group, and every ssh, scp and rsync of a
///    refresh, an action or a backup, and `c`'s ssh, which an `exit` left running before (review
///    ai-review claude #1) — TERM, then KILL after the grace (`arena_core::remote::stop_all_children`);
/// 5. the asks' scratch directories removed (`exit` drops nothing:
///    `arena_core::ask::ai::remove_scratch`);
/// 6. `exit(code)`.
pub(crate) async fn end_now(code: i32, why: Option<String>) {
    if !arena_core::remote::end_children() {
        return std::future::pending().await;
    }
    restore_best_effort();
    if let Some(why) = why {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "arena-tui: {why}");
    }
    arena_core::tty::park_stdin();
    arena_core::remote::stop_all_children().await;
    arena_core::ask::ai::remove_scratch();
    std::process::exit(code)
}

/// Open an interactive SSH shell to the selected pod: suspend the dashboard (leave the
/// alternate screen + raw mode), hand the terminal to `ssh`, then restore on exit ([`hand_over`]).
/// The one SSH call that doesn't go through `Remote`, on purpose: it's the operator's own shell —
/// it needs the terminal (a PTY, stdin) and has no time budget.
pub(crate) fn connect_ssh(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    ui: &mut Ui,
) -> Result<()> {
    let Some(pod) = selected_pod(shared, ui.selected) else { return Ok(()) };
    let target = match SshTarget::from_pod(&pod, &ui.cfg) {
        Ok(t) => t,
        Err(e) => {
            ui.mode = Mode::Result(format!("✗ {}: {e}", pod.name));
            return Ok(());
        }
    };
    let banner = format!("Connecting to {} ({}@{}:{}) — exit the shell to return to the dashboard…\n", pod.name, target.user, target.host, target.port);
    // ssh_args carries the port/keys.
    let mut ssh = std::process::Command::new("ssh");
    ssh.args(target.ssh_args());
    if let Err(e) = hand_over(terminal, &banner, &mut ssh)? {
        ui.mode = Mode::Result(format!("✗ ssh {}: {e}", pod.name));
    }
    Ok(())
}

/// Hand the terminal to `cmd` until it exits — `c`'s ssh: blocking, it inherits this terminal
/// (an interactive PTY) — `banner` said first on the main screen, then take the terminal back.
/// While `cmd` has it the terminal isn't the dashboard's ([`TERMINAL`]): a hangup or a TERM then
/// stops `cmd` with every other child (it's tracked: `arena_core::remote::run_foreground`) and puts
/// nothing back — the terminal is `cmd`'s to restore — and, the dashboard ending, the terminal
/// isn't taken back: `Err`, which ends the loop, the exit [`end_now`]'s. Never started once the
/// dashboard is ending. `Ok(Err)`: `cmd` couldn't be run.
fn hand_over(terminal: &mut Terminal<CrosstermBackend<Stdout>>, banner: &str, cmd: &mut std::process::Command) -> Result<arena_core::Result<std::process::ExitStatus>> {
    if !restore_terminal(terminal)? {
        anyhow::bail!("the dashboard is ending");
    }
    println!("{banner}");
    let status = arena_core::remote::run_foreground(cmd, "ssh");
    *terminal = setup_terminal()?;
    while_ours(terminal, |t| t.clear())?;
    Ok(status)
}

/// A hangup or a TERM ends the dashboard at once ([`end_now`], exit status 129 or 143), from a
/// task of its own: a question still being asked (`?`) stopped with everything it started
/// (review ai-review claude #2: the AI, in a session of its own, ran on when either signal killed
/// the dashboard outright), the terminal put back. Never through the event loop (lane ASK7 H
/// latched a flag there): after a hangup crossterm's read never returns, so the flag was never
/// seen — the dashboard spun a core for ever, SIGTERM no longer stopping it, and its `?` ran on
/// (live ASK4 3c). Listened for from this call on.
pub(crate) fn exit_on_signals() {
    use tokio::signal::unix::{signal, SignalKind};
    for (kind, signo) in [(SignalKind::hangup(), SIGHUP), (SignalKind::terminate(), SIGTERM)] {
        if let Ok(mut listener) = signal(kind) {
            tokio::spawn(async move {
                if listener.recv().await.is_some() {
                    end_now(128 + signo, None).await;
                }
            });
        }
    }
}

/// A terminal that has hung up (`arena_core::tty::hung_up`: stdout's, and stdin's when it is one)
/// ends the dashboard as a hangup does ([`end_now`], 129), looked at every [`HANGUP_POLL`] from a
/// task of its own: without the SIGHUP — ignored, or never sent to this process — the event loop
/// would spin on the dead terminal for ever.
pub(crate) fn exit_on_hangup() {
    use std::io::IsTerminal;
    let fds: Vec<i32> = [(0, std::io::stdin().is_terminal()), (1, std::io::stdout().is_terminal())].into_iter().filter(|(_, tty)| *tty).map(|(fd, _)| fd).collect();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HANGUP_POLL).await;
            if fds.iter().any(|&fd| arena_core::tty::hung_up(fd)) {
                end_now(128 + SIGHUP, None).await;
            }
        }
    });
}
