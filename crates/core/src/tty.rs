//! The terminal the dashboard draws on: whether it has hung up.
//!
//! A hung-up terminal — its other end closed: the operator's SSH session dropped, the tmux server
//! killed — answers every read at once with nothing, and crossterm 0.28's event reader retries
//! such a read for ever without returning, its `poll` timeout never reached (live ASK4 3c:
//! `arena-tui` spun a core, its loop never coming round again). So the dashboard asks this from
//! outside its event loop.

/// Whether the terminal open on `fd` has hung up: a `poll(2)` for no events, which reports a
/// hangup (`POLLHUP`) or an error (`POLLERR`) alone and doesn't wait. Pure but for that one call.
#[cfg(unix)]
pub fn hung_up(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd { fd, events: 0, revents: 0 };
    // SAFETY: one pollfd of our own, a zero timeout: nothing is waited on or written but `p`.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// No `poll(2)` off unix: never.
#[cfg(not(unix))]
pub fn hung_up(_fd: i32) -> bool {
    false
}

/// Park every later read of stdin for good — for a dashboard that is ending: stdin becomes a pipe
/// nothing is ever written to (its write end kept open, close-on-exec, and never closed), so a
/// read of it waits instead of returning. crossterm's event reader reads stdin when it is a
/// terminal, and on a hung-up one retries its empty reads for ever, a core spinning while the
/// ending stops the children (live ASK5 F1: 70–82 ticks in 0.7 s); its next read now waits,
/// until the exit. The terminal stays open on stdout and stderr. Best effort: a pipe that can't
/// be made leaves stdin as it is.
#[cfg(unix)]
pub fn park_stdin() {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: plain pipe/fcntl/dup2/close calls on descriptors of our own; `fds` is ours to fill.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        if fds[0] != 0 {
            libc::dup2(fds[0], 0);
            libc::close(fds[0]);
        }
    }
}

/// Nothing to park off unix.
#[cfg(not(unix))]
pub fn park_stdin() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live terminal hasn't hung up — idle, or with input waiting — and one whose other end
    /// closed has: a pty pair, its master closed (what an SSH session dropping, or tmux's server
    /// killed, does to the dashboard's terminal). Both ends close-on-exec from the start: a
    /// process another test forks and execs meanwhile mustn't keep the master open.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_terminal_whose_other_end_closed_has_hung_up() {
        let os = || std::io::Error::last_os_error();
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        // SAFETY: plain calls on fds of our own; `name` is a buffer we own, NUL-terminated by
        // ptsname_r, and only read up to that NUL.
        unsafe {
            let master = libc::posix_openpt(flags);
            assert!(master >= 0, "posix_openpt: {}", os());
            assert!(libc::grantpt(master) == 0 && libc::unlockpt(master) == 0, "{}", os());
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(master, name.as_mut_ptr(), name.len()), 0, "{}", os());
            let slave = libc::open(name.as_ptr(), flags);
            assert!(slave >= 0, "open the slave: {}", os());
            assert!(!hung_up(slave), "an idle live terminal");
            assert_eq!(libc::write(master, b"x\n".as_ptr().cast(), 2), 2);
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!hung_up(slave), "a live terminal with input waiting");
            assert!(!hung_up(master));
            libc::close(master);
            // A process another test forks holds a copy until it execs: closed, give it that long.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !hung_up(slave) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(hung_up(slave), "its master closed");
            libc::close(slave);
        }
    }
}
