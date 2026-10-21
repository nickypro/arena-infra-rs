//! The dashboard binary for real, on a terminal of its own — `script`'s pty — ended from outside
//! it (live ASK4 3c, lane ASK8 item 4): its terminal hung up (the pty's master closed, as an SSH
//! session dropping or tmux's server killed closes it) with a SIGHUP reaching it or none, or a
//! hangup or a TERM sent while the terminal lives. Each time it is gone within a couple of seconds — where it used to spin a core
//! for ever on the dead terminal, a TERM no longer stopping it — and, sent a signal, it exits 128
//! + it with the terminal put back.
//!
//! No network: the fleet's provider is RunPod with a made-up key, and every HTTP(S) request goes
//! to a proxy at a closed local port (`127.0.0.1:9`), so the listing just fails; no pod, no ssh.
//! Linux only, and skipped without util-linux's `script` (and `setsid`).

#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where every HTTP(S) request goes: nowhere (a closed local port).
const NOWHERE: &str = "http://127.0.0.1:9";

/// crossterm's alternate screen, entered and left.
const ENTER: &str = "\x1b[?1049h";
const LEAVE: &str = "\x1b[?1049l";

/// How long the dashboard may take to end: its hangup check's period, the terminal's restore and
/// the exit, with room to spare (no ask is running, so no grace is waited).
const ENDS_WITHIN: Duration = Duration::from_secs(3);

fn have(tool: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {tool}")).output().is_ok_and(|o| o.status.success())
}

/// Whether `pid` is a process still running (not a zombie, not gone).
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).is_some_and(|st| st != "Z" && st != "X"))
}

/// How the dashboard runs on the pty.
#[derive(Clone, Copy, Debug)]
enum Seat {
    /// It leads the terminal's session (the shell `script` runs exec's it): the kernel's SIGHUP
    /// reaches it on a hangup.
    Leader,
    /// Under that shell, which ends on the hangup — its exit passes the SIGHUP on to the
    /// foreground group, the dashboard's.
    UnderShell,
    /// In a session of its own (`setsid`), its stdin and stdout still the pty: no SIGHUP ever
    /// reaches it — only its own look at the terminal can end it.
    OwnSession,
}

/// The dashboard under `script`, its screen up: `script` itself (the pty's master), the
/// dashboard's pid, and everything it has drawn.
struct OnPty {
    script: Child,
    tui: String,
    out: Arc<Mutex<Vec<u8>>>,
    /// What copies `script`'s output into `out`: done when `script` has exited.
    reader: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
}

impl OnPty {
    /// `arena-tui` on a 40×120 pty of its own, seated as `seat` says.
    fn start(tag: &str, seat: Seat) -> OnPty {
        let dir = std::env::temp_dir().join(format!("arena-tui-pty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.env");
        std::fs::write(&config, "RUNPOD_API_KEY=made-up-for-a-test\nMACHINE_NAME_PREFIX=devtest\n").unwrap();
        let pidfile = dir.join("tui.pid");
        let tui = env!("CARGO_BIN_EXE_arena-tui");
        let run = match seat {
            Seat::Leader => format!("exec '{tui}'"),
            Seat::UnderShell => format!("'{tui}'; true"),
            Seat::OwnSession => format!("setsid '{tui}'; true"),
        };
        let cmd = format!("stty rows 40 cols 120; echo $$ > '{0}.tmp'; mv '{0}.tmp' '{0}'; {run}", pidfile.display());
        let mut script = Command::new("script")
            .args(["-qfec", &cmd, "/dev/null"])
            .env_clear()
            .envs([("PATH", "/usr/bin:/bin"), ("SHELL", "/bin/sh"), ("TERM", "xterm-256color"), ("ARENA_PROVIDER", "runpod"), ("ARENA_REFRESH_SECS", "3600")])
            .envs(["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"].map(|k| (k, NOWHERE)))
            .env("HOME", &dir)
            .env("ARENA_CONFIG", &config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let (mut pipe, sink) = (script.stdout.take().unwrap(), out.clone());
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let mut on = OnPty { script, tui: String::new(), out, reader: Some(reader), dir };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(pidfile.exists() && on.drawn().contains(ENTER)) {
            if Instant::now() > deadline || on.script.try_wait().unwrap().is_some() {
                let drawn = on.drawn();
                on.end();
                panic!("{tag}: the dashboard never came up: {drawn:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let shell = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
        // The dashboard: the shell itself once it has exec'd, else the shell's child (`setsid`
        // exec's it, not being a group's leader there).
        on.tui = if matches!(seat, Seat::Leader) {
            shell
        } else {
            let children = std::fs::read_to_string(format!("/proc/{shell}/task/{shell}/children")).unwrap_or_default();
            children.split_whitespace().next().unwrap_or_default().to_string()
        };
        assert!(alive(&on.tui) && std::fs::read_link(format!("/proc/{}/exe", on.tui)).is_ok_and(|e| e.ends_with("arena-tui")), "{tag}: {}", on.tui);
        // Its first rounds: the screen drawn, the listing failed (nowhere to send it).
        std::thread::sleep(Duration::from_millis(500));
        on
    }

    fn drawn(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    /// Whatever is left of it gone.
    fn end(&mut self) {
        if !self.tui.is_empty() && alive(&self.tui) {
            let _ = Command::new("kill").args(["-9", &self.tui]).status();
        }
        let _ = self.script.kill();
        let _ = self.script.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Live ASK4 3c: the terminal hangs up — `script`, which holds the pty's master, is killed — and
/// the dashboard is gone within [`ENDS_WITHIN`], whether a SIGHUP reaches it (it leads the
/// terminal's session, or runs under a shell there) or none does (a session of its own: its own
/// look at the terminal ends it). Before, it stayed, spinning a core, until SIGKILLed.
#[test]
fn a_hung_up_terminal_ends_the_dashboard() {
    if !have("script") || !have("setsid") {
        eprintln!("script or setsid (util-linux) missing — skipping");
        return;
    }
    for (tag, seat) in [("leader", Seat::Leader), ("under-sh", Seat::UnderShell), ("own-session", Seat::OwnSession)] {
        let mut on = OnPty::start(tag, seat);
        on.script.kill().unwrap();
        on.script.wait().unwrap();
        let hung = Instant::now();
        while alive(&on.tui) && hung.elapsed() < ENDS_WITHIN {
            std::thread::sleep(Duration::from_millis(20));
        }
        let still = alive(&on.tui);
        on.end();
        assert!(!still, "{tag}: the dashboard is still running {:?} after its terminal hung up", hung.elapsed());
    }
}

/// A hangup or a TERM sent to the dashboard while its terminal lives: it exits 128 + the signal
/// (`script -e` gives its status) within [`ENDS_WITHIN`], the terminal put back (the main screen
/// again).
#[test]
fn a_hangup_or_a_term_ends_the_dashboard_with_its_status() {
    if !have("script") {
        eprintln!("script (util-linux) missing — skipping");
        return;
    }
    for (sig, code) in [("-HUP", 129), ("-TERM", 143)] {
        let mut on = OnPty::start(&format!("signal{code}"), Seat::Leader);
        assert!(Command::new("kill").args([sig, &on.tui]).status().unwrap().success());
        let sent = Instant::now();
        let status = loop {
            if let Some(status) = on.script.try_wait().unwrap() {
                break Some(status);
            }
            if sent.elapsed() > ENDS_WITHIN {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_some() {
            // All it drew, to its last byte.
            let _ = on.reader.take().map(std::thread::JoinHandle::join);
        }
        let drawn = on.drawn();
        on.end();
        let status = status.unwrap_or_else(|| panic!("{sig}: the dashboard is still running {:?} after it", sent.elapsed()));
        assert_eq!(status.code(), Some(code), "{sig}");
        let left = drawn.rfind(LEAVE).unwrap_or_else(|| panic!("{sig}: the terminal wasn't put back: {drawn:?}"));
        assert!(drawn.rfind(ENTER).is_some_and(|entered| entered < left), "{sig}");
    }
}
