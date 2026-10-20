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
