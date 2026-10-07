//! The `Remote` seam: how the control plane runs a command on, or copies a file to, a pod.
//!
//! SSH-touching commands historically called `ssh::run` / `ssh::scp` directly, which made
//! timeouts and partial failures untestable without real hosts. This trait is the seam
//! (the seed of PLAN 1.B): [`SshRemote`] is the real thing, built on the same argv
//! builders as `ssh::run`/`ssh::scp`; `FakeRemote` (behind `cfg(test)` / the `test-util`
//! feature, never in a release build) scripts per-host replies, delays and failures and
//! records every call, so fleet behaviour can be tested with paused time.
//!
//! Timeouts: `timeout` bounds the whole call (connect + transfer/remote run). On expiry
//! the call returns [`Error::Timeout`] — a distinct variant, so a caller never mistakes
//! "took too long" for a retryable connection error. The local `ssh`/`scp` child is
//! stopped when the call is abandoned (timed out, or the caller dropped it): tokio's
//! default is to leave a dropped child *running*, so without this every "timed out" step
//! would keep a stray ssh process (and its remote command) alive behind the operator's
//! back. Stopping is SIGTERM first, SIGKILL after a short grace ([`TERM_GRACE`], plus
//! `kill_on_drop` as the backstop): `scp` runs its own `ssh` transport as a child, and
//! only a catchable signal lets scp take that transport down with it — a bare SIGKILL of
//! scp orphans the transport, which then lingers on a wedged pod. (Deliberately not a
//! separate process group + `killpg`: that would take the children out of the terminal's
//! foreground group, so an operator's Ctrl-C would no longer reach them — they'd outlive
//! the CLI.) Stopping the client closes the connection; the remote side then dies on its
//! next write to the closed channel, and our provisioning commands are idempotent, so a
//! re-run is safe either way.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::ssh::{SshOutput, SshTarget};

/// Run commands on / copy files to a pod. `timeout: None` waits indefinitely (only for
/// callers that bound the call some other way).
#[async_trait]
pub trait Remote: Send + Sync {
    /// Run `cmd` on the target and capture its output. A non-zero exit is `Ok` with
    /// `success == false`; `Err` means the call itself failed (spawn error, timeout).
    async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> Result<SshOutput>;

    /// Copy the local file `local` to `remote` on the target. Same `Ok`/`Err` contract
    /// as [`Remote::exec`] (a refused connection is a failed `Ok`, as scp exits 255).
    async fn copy(
        &self,
        t: &SshTarget,
        local: &str,
        remote: &str,
        timeout: Option<Duration>,
    ) -> Result<SshOutput>;
}

/// The real [`Remote`]: `ssh`/`scp` child processes with the non-interactive, fail-fast
/// options from [`SshTarget`], stdin closed, killed if abandoned.
#[derive(Debug, Clone, Copy, Default)]
pub struct SshRemote;

impl SshRemote {
    /// The `ssh … user@host <cmd>` child for [`Remote::exec`] (exposed so the
    /// construction — argv, stdin, kill-on-drop — is unit-testable without a host).
    pub fn exec_command(t: &SshTarget, cmd: &str) -> Command {
        let mut c = child("ssh");
        c.args(t.ssh_args()).arg(cmd);
        c
    }

    /// The `scp … local user@host:remote` child for [`Remote::copy`].
    pub fn copy_command(t: &SshTarget, local: &str, remote: &str) -> Command {
        let mut c = child("scp");
        c.args(t.scp_args(local, remote));
        c
    }
}

#[async_trait]
impl Remote for SshRemote {
    async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> Result<SshOutput> {
        let what = format!("ssh {}:{}", t.host, t.port);
        output_within(Self::exec_command(t, cmd), &what, timeout).await
    }

    async fn copy(
        &self,
        t: &SshTarget,
        local: &str,
        remote: &str,
        timeout: Option<Duration>,
    ) -> Result<SshOutput> {
        let what = format!("scp to {}:{}", t.host, t.port);
        output_within(Self::copy_command(t, local, remote), &what, timeout).await
    }
}

/// A child process that can't wedge on a prompt (stdin closed) and dies with its future
/// (`kill_on_drop`) — the property the per-step timeouts depend on.
fn child(program: &str) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null()).kill_on_drop(true);
    c
}

/// How long an abandoned child gets to exit on SIGTERM before it is SIGKILLed. scp/ssh
/// exit at once on SIGTERM; this only matters for one that ignores it.
pub const TERM_GRACE: Duration = Duration::from_secs(2);

/// Run `cmd` to completion (capturing stdout/stderr) within `timeout`. On timeout (or if
/// this future is dropped) the child is stopped by [`Stopper`].
async fn output_within(mut cmd: Command, what: &str, timeout: Option<Duration>) -> Result<SshOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let spawned = cmd.spawn().map_err(|e| Error::provider(format!("spawning {what}: {e}")))?;
    let mut child = Stopper(Some(spawned));
    let (status, stdout, stderr) = within(what, timeout, child.output(what)).await?;
    Ok(SshOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Owns a running child; if dropped before the child finished, stops it gracefully (see
/// the module doc): SIGTERM now, then SIGKILL (`kill_on_drop`) once it has had
/// [`TERM_GRACE`] to exit. Holding the `Child` — rather than handing it to
/// `wait_with_output`, whose drop SIGKILLs at once — is what makes SIGTERM-first possible.
struct Stopper(Option<Child>);

impl Stopper {
    /// Wait for exit while draining stdout/stderr (concurrently, so a chatty child can't
    /// block on a full pipe).
    async fn output(&mut self, what: &str) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
        let child = self.0.as_mut().expect("the child is only taken on drop");
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        tokio::try_join!(child.wait(), read_all(stdout), read_all(stderr))
            .map_err(|e| Error::provider(format!("waiting for {what}: {e}")))
    }
}

impl Drop for Stopper {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else { return };
        // `id()` is `None` once the child has been reaped (it finished normally): nothing
        // to stop — and its pid may already belong to another process, so never signal it.
        let Some(pid) = child.id() else { return };
        terminate(pid);
        match tokio::runtime::Handle::try_current() {
            // Let it exit on SIGTERM (scp first takes its ssh transport down); if it is
            // still running after the grace, dropping `child` SIGKILLs it.
            Ok(rt) => {
                rt.spawn(async move {
                    let _ = tokio::time::timeout(TERM_GRACE, child.wait()).await;
                });
            }
            // No runtime to wait on: SIGKILL now, via `kill_on_drop`.
            Err(_) => drop(child),
        }
    }
}

/// SIGTERM `pid` (a child we have not reaped, so the pid can't have been reused).
#[cfg(unix)]
fn terminate(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a plain kill(2) on our own live child; no memory is involved.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

/// No SIGTERM off unix: the `kill_on_drop` kill is all there is.
#[cfg(not(unix))]
fn terminate(_pid: u32) {}

async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// Bound `fut` by `timeout`, mapping expiry to [`Error::Timeout`]. Shared by the real and
/// fake remotes so both time out identically (and on tokio's clock, so paused-time tests
/// are instant).
pub async fn within<T>(
    what: &str,
    timeout: Option<Duration>,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match timeout {
        None => fut.await,
        Some(after) => tokio::time::timeout(after, fut)
            .await
            .unwrap_or_else(|_| Err(Error::Timeout { what: what.to_string(), after })),
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use fake::{FakeRemote, FakeReply, RemoteCall};

/// Scripted [`Remote`] for tests (here and in downstream crates via `test-util`).
#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{within, Remote};
    use crate::error::{Error, Result};
    use crate::ssh::{SshOutput, SshTarget};

    /// One scripted answer: what the call returns, after how long.
    #[derive(Debug, Clone)]
    pub struct FakeReply {
        delay: Duration,
        result: std::result::Result<SshOutput, String>,
    }

    impl FakeReply {
        /// Exit 0, no output.
        pub fn ok() -> Self {
            Self::exit(0, "")
        }

        /// Exit 0 with `stdout`.
        pub fn stdout(stdout: &str) -> Self {
            let mut r = Self::ok();
            if let Ok(o) = &mut r.result {
                o.stdout = stdout.to_string();
            }
            r
        }

        /// Exit `code` with `stderr` (e.g. `exit(255, "ssh: connect to host … Connection
        /// refused")` is what ssh/scp do when sshd isn't up yet).
        pub fn exit(code: i32, stderr: &str) -> Self {
            Self {
                delay: Duration::ZERO,
                result: Ok(SshOutput {
                    success: code == 0,
                    code: Some(code),
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                }),
            }
        }

        /// The call itself fails (`Err(Error::provider(msg))`), like a spawn error.
        pub fn error(msg: &str) -> Self {
            Self { delay: Duration::ZERO, result: Err(msg.to_string()) }
        }

        /// Never answers within any sane timeout (a wedged pod).
        pub fn hang() -> Self {
            Self::ok().after(Duration::from_secs(365 * 24 * 3600))
        }

        /// Answer only after `delay` (on tokio's clock — instant under paused time).
        pub fn after(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }
    }

    /// A recorded call, for asserting what was run where and in which order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum RemoteCall {
        Exec { host: String, cmd: String, timeout: Option<Duration> },
        Copy { host: String, local: String, remote: String, timeout: Option<Duration> },
    }

    impl RemoteCall {
        /// The `host:port` the call went to.
        pub fn host(&self) -> &str {
            match self {
                RemoteCall::Exec { host, .. } | RemoteCall::Copy { host, .. } => host,
            }
        }
    }

    /// Per-host (`host:port`) queues of replies, consumed one per call (exec or copy, in
    /// call order). An unscripted or exhausted host answers every call with success, so a
    /// test only scripts the interesting part.
    #[derive(Debug, Default)]
    pub struct FakeRemote {
        script: Mutex<HashMap<String, VecDeque<FakeReply>>>,
        calls: Mutex<Vec<RemoteCall>>,
    }

    impl FakeRemote {
        pub fn new() -> Self {
            Self::default()
        }

        /// Append replies for `host` (`"ip:port"`), consumed in call order.
        pub fn script(&self, host: &str, replies: impl IntoIterator<Item = FakeReply>) -> &Self {
            self.script.lock().unwrap().entry(host.to_string()).or_default().extend(replies);
            self
        }

        /// Every call so far, in the order they started.
        pub fn calls(&self) -> Vec<RemoteCall> {
            self.calls.lock().unwrap().clone()
        }

        /// The calls made to one `host:port`, in order.
        pub fn calls_to(&self, host: &str) -> Vec<RemoteCall> {
            self.calls().into_iter().filter(|c| c.host() == host).collect()
        }

        /// Record the call (at its *start*, so a hung call is visible) and pop its reply.
        fn begin(&self, call: RemoteCall) -> FakeReply {
            let host = call.host().to_string();
            self.calls.lock().unwrap().push(call);
            self.script
                .lock()
                .unwrap()
                .get_mut(&host)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(FakeReply::ok)
        }

        async fn answer(what: &str, reply: FakeReply, timeout: Option<Duration>) -> Result<SshOutput> {
            within(what, timeout, async move {
                tokio::time::sleep(reply.delay).await;
                reply.result.map_err(Error::provider)
            })
            .await
        }
    }

    #[async_trait]
    impl Remote for FakeRemote {
        async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> Result<SshOutput> {
            let host = format!("{}:{}", t.host, t.port);
            let what = format!("ssh {host}");
            let reply = self.begin(RemoteCall::Exec { host, cmd: cmd.to_string(), timeout });
            Self::answer(&what, reply, timeout).await
        }

        async fn copy(
            &self,
            t: &SshTarget,
            local: &str,
            remote: &str,
            timeout: Option<Duration>,
        ) -> Result<SshOutput> {
            let host = format!("{}:{}", t.host, t.port);
            let what = format!("scp to {host}");
            let reply = self.begin(RemoteCall::Copy {
                host,
                local: local.to_string(),
                remote: remote.to_string(),
                timeout,
            });
            Self::answer(&what, reply, timeout).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "1.2.3.4".into(),
            port: 22001,
            key_paths: vec!["/k/arena_key".into()],
            connect_timeout_secs: 10,
        }
    }

    fn argv(c: &Command) -> Vec<String> {
        c.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn ssh_remote_builds_killable_ssh_and_scp_children() {
        // exec: `ssh <the shared fail-fast args> <cmd>`, killed if the call is abandoned.
        let t = target();
        let c = SshRemote::exec_command(&t, "echo hi");
        assert_eq!(c.as_std().get_program(), "ssh");
        let mut want = t.ssh_args();
        want.push("echo hi".into());
        assert_eq!(argv(&c), want);
        assert!(c.get_kill_on_drop(), "a timed-out ssh must not be left running");

        // copy: `scp <args> local user@host:remote`, same guarantee.
        let c = SshRemote::copy_command(&t, "/local/key", "/root/.ssh/id_ed25519");
        assert_eq!(c.as_std().get_program(), "scp");
        assert_eq!(argv(&c), t.scp_args("/local/key", "/root/.ssh/id_ed25519"));
        assert!(argv(&c).ends_with(&["/local/key".into(), "root@1.2.3.4:/root/.ssh/id_ed25519".into()]));
        assert!(c.get_kill_on_drop());
    }

    #[tokio::test]
    async fn within_maps_expiry_to_timeout_error() {
        let slow = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        };
        let err = within("ssh 1.2.3.4:22", Some(Duration::from_millis(20)), slow).await.unwrap_err();
        assert!(matches!(&err, Error::Timeout { after, .. } if *after == Duration::from_millis(20)), "{err}");
        assert_eq!(err.to_string(), "ssh 1.2.3.4:22 timed out after 20ms");
        // No timeout = just the future's own result.
        assert!(within("x", None, async { Ok(7) }).await.is_ok());
    }

    /// The property the per-step timeouts rely on, checked against a real child: when the
    /// output future is abandoned on timeout, the process is killed (tokio's default would
    /// leave it running). Uses a local `sh` — no network, no ssh.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_child_is_killed_not_leaked() {
        let pidfile = std::env::temp_dir().join(format!("arena-remote-kill-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let mut c = child("sh");
        c.arg("-c").arg(format!("echo $$ > '{}'; exec sleep 30", pidfile.display()));
        let started = std::time::Instant::now();
        let err = output_within(c, "sh", Some(Duration::from_millis(500))).await.unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "returned at the timeout, not at exit");

        let pid = std::fs::read_to_string(&pidfile).expect("child wrote its pid").trim().to_string();
        let _ = std::fs::remove_file(&pidfile);
        // Killed => gone (or a zombie awaiting tokio's reaper), never still `sleep`ing.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from));
            match state.as_deref() {
                None | Some("Z") | Some("X") => break,
                Some(s) if std::time::Instant::now() >= deadline => {
                    panic!("child {pid} still alive (state {s}) after its timeout")
                }
                Some(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// `/proc/<pid>` state letter, or `None` once the process is gone.
    #[cfg(target_os = "linux")]
    fn proc_state(pid: &str) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from))
    }

    /// Wait (yielding to the runtime, so a background stop can run) until `pid` is gone
    /// or a zombie; panic if it is still alive after `within`.
    #[cfg(target_os = "linux")]
    async fn assert_dies(pid: &str, within: Duration, what: &str) {
        let deadline = std::time::Instant::now() + within;
        loop {
            match proc_state(pid).as_deref() {
                None | Some("Z") | Some("X") => return,
                Some(s) if std::time::Instant::now() >= deadline => {
                    panic!("{what} {pid} still alive (state {s})")
                }
                Some(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }

    /// A temp dir for one test's scripts/pid files.
    #[cfg(target_os = "linux")]
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arena-remote-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The finding behind SIGTERM-first: scp runs its ssh transport as a child, and a
    /// SIGKILLed scp orphans it (left running on a wedged pod). A timed-out copy must take
    /// the transport down too. Real `scp`, with a fake transport (`-S`) that records its
    /// pid and hangs like a wedged pod would — no network.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_scp_takes_its_ssh_transport_down_too() {
        if std::process::Command::new("scp").arg("-h").output().is_err() {
            eprintln!("scp not installed — skipping");
            return;
        }
        let dir = scratch("scp");
        let pidfile = dir.join("transport.pid");
        let transport = dir.join("fake-ssh.sh");
        std::fs::write(&transport, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pidfile.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o755)).unwrap();
        let local = dir.join("payload");
        std::fs::write(&local, "x").unwrap();

        let mut c = child("scp");
        c.arg("-S").arg(&transport).args(["-o", "BatchMode=yes"]).arg(&local).arg("root@10.0.0.1:/tmp/payload");
        let err = output_within(c, "scp", Some(Duration::from_millis(700))).await.unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");

        let pid = std::fs::read_to_string(&pidfile).expect("transport wrote its pid").trim().to_string();
        assert_dies(&pid, Duration::from_secs(5), "scp's ssh transport").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A child that ignores SIGTERM is still SIGKILLed once the grace has passed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_ignoring_sigterm_is_killed_after_the_grace() {
        let dir = scratch("ignore-term");
        let pidfile = dir.join("pid");
        let mut c = child("sh");
        c.arg("-c").arg(format!("trap '' TERM; echo $$ > '{}'; while :; do sleep 0.2; done", pidfile.display()));
        let err = output_within(c, "sh", Some(Duration::from_millis(500))).await.unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        let pid = std::fs::read_to_string(&pidfile).expect("child wrote its pid").trim().to_string();
        // SIGTERM came first (ignored here), so it is not dead yet…
        assert!(matches!(proc_state(&pid).as_deref(), Some(s) if s != "Z" && s != "X"), "killed without a grace");
        // …but it is once the grace is over.
        assert_dies(&pid, TERM_GRACE + Duration::from_secs(5), "sh ignoring SIGTERM").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn finished_child_output_is_captured() {
        let mut c = child("sh");
        c.arg("-c").arg("echo out; echo err >&2; exit 3");
        let out = output_within(c, "sh", Some(Duration::from_secs(10))).await.unwrap();
        assert_eq!((out.success, out.code), (false, Some(3)));
        assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("out\n", "err\n"));
        let err = output_within(child("/nonexistent/arena-no-such-binary"), "ssh x", None).await.unwrap_err();
        assert!(err.to_string().contains("spawning ssh x"), "{err}");
    }

    #[tokio::test]
    async fn stdin_is_closed_on_every_child() {
        // Can't read Stdio back off a Command, so pin it behaviourally: a child that reads
        // stdin (`cat`) must see EOF immediately rather than wait for input.
        let out = output_within(child("cat"), "cat", Some(Duration::from_secs(10)))
            .await
            .expect("cat sees EOF on a null stdin and exits");
        assert!(out.success && out.stdout.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn fake_remote_scripts_per_host_and_records_calls() {
        let fake = FakeRemote::new();
        fake.script("1.2.3.4:22001", [FakeReply::exit(255, "Connection refused"), FakeReply::stdout("hi")]);
        let t = target();
        // Scripted replies are consumed in call order across exec + copy.
        let first = fake.copy(&t, "/a", "/b", None).await.unwrap();
        assert!(!first.success && first.code == Some(255));
        let second = fake.exec(&t, "echo hi", Some(Duration::from_secs(5))).await.unwrap();
        assert_eq!(second.stdout, "hi");
        // Exhausted => success.
        assert!(fake.exec(&t, "true", None).await.unwrap().success);
        // A different, unscripted host also succeeds.
        let mut other = target();
        other.port = 22002;
        assert!(fake.exec(&other, "true", None).await.unwrap().success);

        assert_eq!(
            fake.calls_to("1.2.3.4:22001"),
            vec![
                RemoteCall::Copy { host: "1.2.3.4:22001".into(), local: "/a".into(), remote: "/b".into(), timeout: None },
                RemoteCall::Exec {
                    host: "1.2.3.4:22001".into(),
                    cmd: "echo hi".into(),
                    timeout: Some(Duration::from_secs(5)),
                },
                RemoteCall::Exec { host: "1.2.3.4:22001".into(), cmd: "true".into(), timeout: None },
            ]
        );
        assert_eq!(fake.calls().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn fake_remote_hang_times_out_on_the_tokio_clock() {
        let fake = FakeRemote::new();
        fake.script("1.2.3.4:22001", [FakeReply::hang(), FakeReply::error("spawning ssh: boom")]);
        let start = tokio::time::Instant::now();
        let err = fake.exec(&target(), "sleep", Some(Duration::from_secs(300))).await.unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        assert_eq!(start.elapsed(), Duration::from_secs(300)); // paused clock: exact, instant
        let err = fake.exec(&target(), "x", None).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }
}
