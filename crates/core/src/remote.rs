//! The `Remote` seam: how the control plane runs a command on, or copies a file to, a pod.
//!
//! SSH-touching commands historically called `ssh::run` / `ssh::scp` directly, which made
//! timeouts and partial failures untestable without real hosts. This trait is the seam
//! (PLAN 1.B): [`SshRemote`] is the real thing, built on the same argv builders as
//! `ssh::run`/`ssh::scp`; `FakeRemote` (behind `cfg(test)` / the `test-util` feature,
//! never in a release build) scripts per-host replies, delays and failures and records
//! every call, so fleet behaviour can be tested with paused time. Every pod-SSH path in
//! the CLI goes through it, each call with a budget, so one wedged pod can't hang a fleet
//! command. Outside it on purpose: rsync transfers (`pods pull`, `pods backup`'s file step,
//! the replace/migrate via-local file copy) — rsync drives its own ssh transport (`-e`),
//! which is neither an exec nor a single-file copy — and the proxy host's nginx deploy,
//! which isn't a pod. The rsyncs still get the same budget and child discipline through
//! [`run_local`]: one wedged pod must not hold a backup (or the cron behind it) forever.
//!
//! Timeouts: `timeout` bounds the whole call (connect + transfer/remote run). On expiry
//! the call returns [`Error::Timeout`] — a distinct variant, so a caller never mistakes
//! "took too long" for a retryable connection error. The local `ssh`/`scp` child is
//! stopped when the call is abandoned (timed out, or the caller dropped it): tokio's
//! default is to leave a dropped child *running*, so without this every "timed out" step
//! would keep a stray ssh process (and its remote command) alive behind the operator's
//! back. Stopping is SIGTERM first, SIGKILL after a short grace ([`TERM_GRACE`], plus
//! `kill_on_drop` as the backstop). On a timeout the stop is over before the call returns
//! (so a timed-out call takes up to [`TERM_GRACE`] longer than its budget): a CLI whose
//! last job just timed out exits at once, and a grace left running in the background would
//! die with the runtime. Why SIGTERM first: `scp` runs its own `ssh` transport as a child, and
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

use crate::error::{human_duration, Error, Result};
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

    /// Copy the local file *or directory tree* `local` to `remote` (`scp -r`) — what
    /// `pods cp -r` needs. A separate method rather than a flag on [`Remote::copy`], so
    /// every existing caller keeps copying exactly one file. Same contract as `copy`.
    async fn copy_recursive(
        &self,
        t: &SshTarget,
        local: &str,
        remote: &str,
        timeout: Option<Duration>,
    ) -> Result<SshOutput>;
}

/// Budget for a quick read or tiny write on a pod: an identity/marker probe, `mkdir -p`, a
/// post-copy size check, a `~/.name` write, the dashboard's metrics round-trip. On a
/// healthy pod these finish in about a second (plus at most the ssh connect timeout), so
/// 20s means wedged — and they sit inside fleet sweeps and the replace/migrate pipelines,
/// which must never stall on one bad pod.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// How a failed [`Remote`] call reads in a per-pod report line (`✗ <name>: <this>`): a
/// timeout is just `timed out after Ns` — the line already names the pod, so the error's
/// `ssh <host:port>` prefix is noise there — and anything else is the error as-is.
pub fn describe_error(e: &Error) -> String {
    match e {
        Error::Timeout { after, .. } => format!("timed out after {}", human_duration(after)),
        other => other.to_string(),
    }
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

    /// The `scp -r … local user@host:remote` child for [`Remote::copy_recursive`].
    pub fn copy_recursive_command(t: &SshTarget, local: &str, remote: &str) -> Command {
        let mut c = child("scp");
        c.arg("-r").args(t.scp_args(local, remote));
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

    async fn copy_recursive(
        &self,
        t: &SshTarget,
        local: &str,
        remote: &str,
        timeout: Option<Duration>,
    ) -> Result<SshOutput> {
        let what = format!("scp -r to {}:{}", t.host, t.port);
        output_within(Self::copy_recursive_command(t, local, remote), &what, timeout).await
    }
}

/// Run the local `program` with `args` to completion within `timeout`, capturing its
/// output — for a transfer that drives its own ssh transport and so can't be a [`Remote`]
/// call (rsync's `-e ssh …`). Same child discipline as [`SshRemote`]: stdin closed, and on
/// expiry SIGTERM, then SIGKILL after [`TERM_GRACE`], finished before the timeout is
/// returned (if the caller drops this future instead, the same in the background).
/// SIGTERM-first matters doubly for rsync: like scp, it runs `ssh` as a child — and, pulling,
/// a forked receiver — and only takes them down on a catchable signal: a SIGKILLed rsync
/// leaves its ssh and receiver transferring from the wedged pod, the receiver holding the
/// backup cron's lock and a partial temp file. `what` names the call in a timeout/spawn
/// error. Same `Ok`/`Err` contract as [`Remote::exec`]: a non-zero exit is `Ok` with
/// `success == false`; `Err` is a spawn error or [`Error::Timeout`].
pub async fn run_local(program: &str, args: &[String], what: &str, timeout: Option<Duration>) -> Result<SshOutput> {
    let mut c = child(program);
    c.args(args);
    output_within(c, what, timeout).await
}

/// A child process that can't wedge on a prompt (stdin closed) and dies with its future
/// (`kill_on_drop`) — the property the per-step timeouts depend on.
fn child(program: &str) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null()).kill_on_drop(true);
    c
}

/// How long a stopped child gets to exit on SIGTERM before it is SIGKILLed. scp/ssh exit at
/// once on SIGTERM; rsync takes ~400 ms (its handler waits before signalling its ssh
/// transport and forked receiver), so this must stay well above that — a SIGKILL inside
/// that window orphans both.
pub const TERM_GRACE: Duration = Duration::from_secs(2);

/// Run `cmd` to completion (capturing stdout/stderr) within `timeout`. On timeout the child
/// is stopped *before* this returns ([`Stopper::stop`]); if this future is dropped instead,
/// [`Stopper`]'s `Drop` does it in the background.
async fn output_within(mut cmd: Command, what: &str, timeout: Option<Duration>) -> Result<SshOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let spawned = cmd.spawn().map_err(|e| Error::provider(format!("spawning {what}: {e}")))?;
    let mut child = Stopper(Some(spawned));
    let result = within(what, timeout, child.output(what)).await;
    if result.is_err() {
        // Timed out (or the wait itself failed): finish the stop here, not in a detached
        // task. The caller often returns at once — a wedged pod's rsync is usually a
        // backup's last job, so `main` exits and drops the runtime, which would cancel a
        // background grace and leave only `kill_on_drop`'s bare SIGKILL, orphaning the
        // transport and receiver (still transferring, still holding the cron's flock).
        child.stop().await;
    }
    let (status, stdout, stderr) = result?;
    Ok(SshOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Owns a running child and stops it gracefully (see the module doc): SIGTERM, then SIGKILL
/// once it has had [`TERM_GRACE`] to exit. On a timeout [`Stopper::stop`] does that inline;
/// `Drop` is the backstop for a caller that abandons the call (drops the future), where
/// nothing can be awaited. Holding the `Child` — rather than handing it to
/// `wait_with_output`, whose drop SIGKILLs at once — is what makes SIGTERM-first possible.
struct Stopper(Option<Child>);

impl Stopper {
    /// Wait for exit while draining stdout/stderr (concurrently, so a chatty child can't
    /// block on a full pipe).
    async fn output(&mut self, what: &str) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
        let child = self.0.as_mut().expect("the child is only taken when stopping");
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        tokio::try_join!(child.wait(), read_all(stdout), read_all(stderr))
            .map_err(|e| Error::provider(format!("waiting for {what}: {e}")))
    }

    /// Stop the child and wait until it is gone: SIGTERM, up to [`TERM_GRACE`] for it to
    /// exit (rsync/scp take their ssh transport down meanwhile), then SIGKILL and reap.
    /// Awaited, so when it returns the stop is over — whatever the caller does next.
    async fn stop(&mut self) {
        let Some(mut child) = self.0.take() else { return };
        // Already reaped (it finished as the clock ran out): nothing to stop, and the pid
        // may belong to another process by now — never signal it.
        let Some(pid) = child.id() else { return };
        terminate(pid);
        if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_err() {
            // Ignored SIGTERM: SIGKILL, and reap it.
            let _ = child.kill().await;
        }
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
            // still running after the grace, dropping `child` SIGKILLs it. Best effort: a
            // runtime shut down meanwhile cancels this and SIGKILLs at once — which is why
            // the timeout path stops the child inline instead.
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
            Self::ok().with_stdout(stdout)
        }

        /// This reply's stdout set to `stdout` — for a failed call that still printed
        /// something (`exit(3, "").with_stdout(…)`: a remote script explaining a refusal).
        pub fn with_stdout(mut self, stdout: &str) -> Self {
            if let Ok(o) = &mut self.result {
                o.stdout = stdout.to_string();
            }
            self
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
        /// `recursive`: made through [`Remote::copy_recursive`] (`scp -r`).
        Copy { host: String, local: String, remote: String, recursive: bool, timeout: Option<Duration> },
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
    /// test only scripts the interesting part. Standing answers ([`FakeRemote::on`]) cover a
    /// long scenario whose exact call order isn't the point: an exec whose command contains a
    /// rule's text gets that rule's reply whenever the host's queue is empty.
    #[derive(Debug, Default)]
    pub struct FakeRemote {
        script: Mutex<HashMap<String, VecDeque<FakeReply>>>,
        rules: Mutex<Vec<(String, String, FakeReply)>>,
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

        /// A standing answer for `host`: every exec whose command contains `needle` (and that
        /// no queued reply answers) gets `reply`. The first matching rule wins.
        pub fn on(&self, host: &str, needle: &str, reply: FakeReply) -> &Self {
            self.rules.lock().unwrap().push((host.to_string(), needle.to_string(), reply));
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

        /// Record the call (at its *start*, so a hung call is visible) and pop its reply —
        /// else a standing rule's ([`FakeRemote::on`]), else success.
        fn begin(&self, call: RemoteCall) -> FakeReply {
            let host = call.host().to_string();
            let cmd = match &call {
                RemoteCall::Exec { cmd, .. } => Some(cmd.clone()),
                RemoteCall::Copy { .. } => None,
            };
            self.calls.lock().unwrap().push(call);
            if let Some(reply) = self.script.lock().unwrap().get_mut(&host).and_then(VecDeque::pop_front) {
                return reply;
            }
            cmd.and_then(|cmd| {
                self.rules.lock().unwrap().iter().find(|(h, needle, _)| *h == host && cmd.contains(needle.as_str())).map(|(_, _, r)| r.clone())
            })
            .unwrap_or_else(FakeReply::ok)
        }

        async fn answer(what: &str, reply: FakeReply, timeout: Option<Duration>) -> Result<SshOutput> {
            within(what, timeout, async move {
                tokio::time::sleep(reply.delay).await;
                reply.result.map_err(Error::provider)
            })
            .await
        }

        /// [`Remote::copy`] / [`Remote::copy_recursive`]: one scripted reply either way,
        /// recorded with which one it was.
        async fn copying(
            &self,
            t: &SshTarget,
            local: &str,
            remote: &str,
            recursive: bool,
            timeout: Option<Duration>,
        ) -> Result<SshOutput> {
            let host = format!("{}:{}", t.host, t.port);
            let what = format!("scp to {host}");
            let reply = self.begin(RemoteCall::Copy {
                host,
                local: local.to_string(),
                remote: remote.to_string(),
                recursive,
                timeout,
            });
            Self::answer(&what, reply, timeout).await
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
            self.copying(t, local, remote, false, timeout).await
        }

        async fn copy_recursive(
            &self,
            t: &SshTarget,
            local: &str,
            remote: &str,
            timeout: Option<Duration>,
        ) -> Result<SshOutput> {
            self.copying(t, local, remote, true, timeout).await
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

        // copy_recursive: the same scp, with `-r` first (`pods cp -r`).
        let c = SshRemote::copy_recursive_command(&t, "/local/dir", "/root/dir");
        assert_eq!(c.as_std().get_program(), "scp");
        let mut want = vec!["-r".to_string()];
        want.extend(t.scp_args("/local/dir", "/root/dir"));
        assert_eq!(argv(&c), want);
        assert!(c.get_kill_on_drop());
    }

    #[test]
    fn describe_error_shortens_a_timeout_for_a_per_pod_line() {
        let e = Error::Timeout { what: "ssh 1.2.3.4:22001".into(), after: Duration::from_secs(90) };
        assert_eq!(describe_error(&e), "timed out after 90s");
        // Anything else reads as the error itself.
        let e = Error::provider("spawning ssh 1.2.3.4:22001: No such file");
        assert_eq!(describe_error(&e), e.to_string());
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

    /// The same for rsync (`pods pull`, `pods backup`'s file step, the via-local copy): a
    /// timed-out [`run_local`] rsync takes its ssh transport down with it. Real `rsync`, a
    /// fake transport (`-e`) that records its pid and hangs like a wedged pod — no network.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_rsync_takes_its_ssh_transport_down_too() {
        if std::process::Command::new("rsync").arg("--version").output().is_err() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let dir = scratch("rsync");
        let pidfile = dir.join("transport.pid");
        let transport = dir.join("fake-ssh.sh");
        std::fs::write(&transport, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pidfile.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dest = format!("{}/", dir.join("dest").display());
        let args: Vec<String> =
            ["-a", "--timeout=300", "-e", &transport.display().to_string(), "root@10.0.0.1:", &dest].map(String::from).into();
        let started = std::time::Instant::now();
        let err = run_local("rsync", &args, "rsync of devtest-apple", Some(Duration::from_millis(700))).await.unwrap_err();
        assert!(matches!(&err, Error::Timeout { what, .. } if what == "rsync of devtest-apple"), "{err}");
        assert_eq!(describe_error(&err), "timed out after 700ms");
        assert!(started.elapsed() < Duration::from_secs(10), "returned at the budget, not at the transport's exit");

        let pid = std::fs::read_to_string(&pidfile).expect("transport wrote its pid").trim().to_string();
        assert_dies(&pid, Duration::from_secs(5), "rsync's ssh transport").await;
        // A finished run is captured like any other call; a missing binary is a spawn error.
        let out = run_local("sh", &["-c".into(), "echo moved; exit 23".into()], "rsync", None).await.unwrap();
        assert_eq!((out.success, out.code, out.stdout.as_str()), (false, Some(23), "moved\n"));
        let err = run_local("/nonexistent/arena-no-rsync", &[], "rsync of x", None).await.unwrap_err();
        assert!(err.to_string().contains("spawning rsync of x"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live processes (other than this test) whose argv mentions `needle` — the rsync client,
    /// its forked receiver and the local "remote" server all carry the scratch dir. Zombies
    /// have no argv, so they don't count.
    #[cfg(target_os = "linux")]
    fn live_procs_mentioning(needle: &str) -> Vec<u32> {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
        entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| *pid != me)
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|c| String::from_utf8_lossy(&c).replace('\0', " ").contains(needle))
            })
            .collect()
    }

    /// The exit path the CLI actually takes (review finding): the wedged pod's rsync is the
    /// last job, so the timeout comes back and `main` returns, dropping the runtime at once.
    /// The stop must be over by then — a grace left to a background task dies with the
    /// runtime, and the bare SIGKILL that follows (`kill_on_drop`) lands inside rsync's
    /// ~400 ms SIGTERM handling, orphaning its ssh transport and its forked receiver, which
    /// go on transferring (holding the cron's flock, leaving a `.big.bin.XXXXXX` temp file)
    /// after arena has exited. Real rsync pulls an incompressible file, throttled by
    /// `--bwlimit`, through a fake transport that runs the `rsync --server` here (as sshd
    /// would on the pod), so a receiver is forked and a temp file is mid-write when the
    /// budget runs out; the call has its own runtime, dropped right after. No network.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_timed_out_rsync_is_fully_stopped_before_the_runtime_goes() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        if std::process::Command::new("rsync").arg("--version").output().is_err() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let dir = scratch("rsync-exit");
        let (src, dest) = (dir.join("src"), dir.join("dest"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        // 4 MiB of random bytes: -z can't shrink them, so --bwlimit=256 (KiB/s) keeps the
        // transfer going for ~16 s — far past the budget and the checks below.
        let mut data = vec![0u8; 4 << 20];
        std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut data).unwrap();
        std::fs::write(src.join("big.bin"), &data).unwrap();
        let pidfile = dir.join("transport.pid");
        let transport = dir.join("fake-ssh.sh");
        // rsync runs `<transport> [-l user] host rsync --server --sender …`: skip to the
        // remote command and run it locally.
        std::fs::write(
            &transport,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nwhile [ $# -gt 0 ] && [ \"$1\" != rsync ]; do shift; done\nexec \"$@\"\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args: Vec<String> = vec![
            "-avz".into(),
            "--timeout=300".into(),
            "--bwlimit=256".into(),
            "-e".into(),
            transport.display().to_string(),
            format!("root@10.0.0.1:{}/", src.display()),
            format!("{}/", dest.display()),
        ];
        let temp_files = || -> Vec<String> {
            std::fs::read_dir(&dest)
                .map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with(".big.bin.")).collect())
                .unwrap_or_default()
        };
        // Watch for the receiver's temp file while the call runs: proof the budget ran out
        // mid-transfer, with a receiver forked.
        let stop_watch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (stop, dest) = (stop_watch.clone(), dest.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mid = std::fs::read_dir(&dest)
                        .is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with(".big.bin."))));
                    if mid {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                false
            })
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let err = rt
            .block_on(run_local("rsync", &args, "rsync of devtest-bloom [big]", Some(Duration::from_secs(2))))
            .unwrap_err();
        drop(rt); // what returning from `#[tokio::main]` does
        stop_watch.store(true, std::sync::atomic::Ordering::Relaxed);
        let saw_temp = watcher.join().unwrap();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        assert!(saw_temp, "the budget should have run out mid-transfer (no temp file seen)");
        assert!(pidfile.exists(), "the transport ran");

        // Within a few seconds nothing of the transfer is left: no transport/server, no
        // receiver, no partial temp file.
        let needle = dir.display().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let (alive, temps) = (live_procs_mentioning(&needle), temp_files());
            if alive.is_empty() && temps.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                for pid in &alive {
                    // Clean up before failing, so a regression doesn't leave a transfer running.
                    if let Ok(p) = libc::pid_t::try_from(*pid) {
                        // SAFETY: kill(2) on a test-owned stray; no memory involved.
                        unsafe {
                            libc::kill(p, libc::SIGKILL);
                        }
                    }
                }
                panic!("left behind after the runtime was dropped: processes {alive:?}, temp files {temps:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A child that ignores SIGTERM is still SIGKILLed once the grace has passed — and the
    /// timed-out call returns only after that: the stop is finished inline.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_ignoring_sigterm_is_killed_after_the_grace() {
        let dir = scratch("ignore-term");
        let pidfile = dir.join("pid");
        let mut c = child("sh");
        c.arg("-c").arg(format!("trap '' TERM; echo $$ > '{}'; while :; do sleep 0.2; done", pidfile.display()));
        let started = std::time::Instant::now();
        let err = output_within(c, "sh", Some(Duration::from_millis(500))).await.unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        let pid = std::fs::read_to_string(&pidfile).expect("child wrote its pid").trim().to_string();
        // SIGTERM came first (ignored here) and it got the whole grace…
        assert!(started.elapsed() >= Duration::from_millis(500) + TERM_GRACE, "killed without a grace");
        // …and it was gone by the time the timeout came back.
        assert!(matches!(proc_state(&pid).as_deref(), None | Some("Z") | Some("X")), "still alive after the call returned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backstop: a caller that abandons the call (drops the future — no timeout of the
    /// call's own) still gets its child stopped, in the background.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_abandoned_call_still_stops_its_child() {
        let dir = scratch("abandon");
        let pidfile = dir.join("pid");
        let mut c = child("sh");
        c.arg("-c").arg(format!("echo $$ > '{}'; exec sleep 30", pidfile.display()));
        let gave_up = tokio::time::timeout(Duration::from_millis(500), output_within(c, "sh", None)).await;
        assert!(gave_up.is_err(), "the caller gave up first");
        let pid = std::fs::read_to_string(&pidfile).expect("child wrote its pid").trim().to_string();
        assert_dies(&pid, Duration::from_secs(5), "an abandoned call's child").await;
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
        assert!(fake.copy_recursive(&t, "/dir", "/r", None).await.unwrap().success);
        // A different, unscripted host also succeeds.
        let mut other = target();
        other.port = 22002;
        assert!(fake.exec(&other, "true", None).await.unwrap().success);

        assert_eq!(
            fake.calls_to("1.2.3.4:22001"),
            vec![
                RemoteCall::Copy {
                    host: "1.2.3.4:22001".into(),
                    local: "/a".into(),
                    remote: "/b".into(),
                    recursive: false,
                    timeout: None,
                },
                RemoteCall::Exec {
                    host: "1.2.3.4:22001".into(),
                    cmd: "echo hi".into(),
                    timeout: Some(Duration::from_secs(5)),
                },
                RemoteCall::Exec { host: "1.2.3.4:22001".into(), cmd: "true".into(), timeout: None },
                RemoteCall::Copy {
                    host: "1.2.3.4:22001".into(),
                    local: "/dir".into(),
                    remote: "/r".into(),
                    recursive: true,
                    timeout: None,
                },
            ]
        );
        assert_eq!(fake.calls().len(), 5);
    }

    /// Standing answers: by command text, per host, after the queue — and never for a copy.
    #[tokio::test]
    async fn fake_remote_rules_answer_by_command_text_when_nothing_is_queued() {
        let fake = FakeRemote::new();
        let t = target();
        fake.on("1.2.3.4:22001", "SSH_OK", FakeReply::stdout("GPU\nSSH_OK\n"));
        fake.on("1.2.3.4:22001", "", FakeReply::exit(9, "anything else"));
        fake.script("1.2.3.4:22001", [FakeReply::stdout("queued first")]);
        assert_eq!(fake.exec(&t, "echo SSH_OK", None).await.unwrap().stdout, "queued first");
        assert_eq!(fake.exec(&t, "echo SSH_OK", None).await.unwrap().stdout, "GPU\nSSH_OK\n");
        assert_eq!(fake.exec(&t, "true", None).await.unwrap().code, Some(9), "first matching rule wins");
        assert!(fake.copy(&t, "/a", "/b", None).await.unwrap().success, "rules are for execs");
        let mut other = target();
        other.port = 22002;
        assert!(fake.exec(&other, "echo SSH_OK", None).await.unwrap().stdout.is_empty(), "rules are per host");
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
