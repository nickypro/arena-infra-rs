//! `pods run --background`, `pods jobs`, `pods logs`: the SSH side of detached jobs.
//!
//! What runs on a pod, how its replies are read and how they're shown is pure and tested
//! in `arena_core::jobs`. Here is the fan-out: one bounded call per pod, all pods at once
//! (a wedged pod reports at its budget and never holds up the others), and `logs
//! --follow`'s polling — one independent loop per pod feeding a single printer, so a slow
//! or unreachable pod delays only its own lines. Ctrl+C while following drops the loops
//! (their in-flight ssh calls are stopped with them); the jobs themselves run in their own
//! sessions on the pods and never see it.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::jobs::{self, Follow, JobId, JobInfo, ReadReply};
use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use super::{confirm, exec_each_pod, PodCall, Selected};

/// Budget for starting a job on one pod: the ssh connect (10s), a few file writes, and up
/// to 5s for the job to record its pid. Past this the pod is wedged — though the job may
/// have started anyway, which the report says.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for one read (a listing, a log window of at most 1 MiB, a kill): seconds on a
/// healthy pod.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often `logs --follow` re-reads each pod.
pub(crate) const FOLLOW_INTERVAL: Duration = Duration::from_secs(3);

/// Consecutive failed reads after which `logs --follow` gives up on a pod (the job isn't
/// touched; the command then fails). One bad read is a blip; five in a row is a pod that's
/// gone.
pub(crate) const FOLLOW_MAX_FAILURES: u32 = 5;

/// `pods logs --tail` default.
pub(crate) const DEFAULT_TAIL: u64 = 20;

/// Where a handler's lines go: stdout for real, a recorder in tests.
pub(crate) type Say<'a> = &'a mut dyn FnMut(&str);

/// A pod's reply, parsed — or why there is none. A call that failed outright (non-zero
/// exit and none of our marker lines: ssh couldn't connect, no `sh`) reads as its exit
/// code and last stderr line; otherwise the reply's own parse decides (the start script
/// explains its own refusals).
fn reply<T>(call: PodCall, parse: impl FnOnce(&str) -> Result<T, String>) -> Result<T, String> {
    let out = call?;
    if !out.success && !out.stdout.contains("ARENA_") {
        let stderr = arena_core::ssh::strip_interactive_noise(&out.stderr);
        let last = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("(no output)");
        let code = out.code.map_or_else(|| "killed".to_string(), |c| c.to_string());
        return Err(format!("exit {code}: {}", jobs::sanitize(last)));
    }
    parse(&out.stdout)
}

/// `cmd` on every target at once, each call within `timeout`; the parsed replies in the
/// targets' order (so a caller can pair them back up, even when two pods share a name).
async fn each_reply<T>(
    remote: &Arc<dyn Remote>,
    targets: &[(String, SshTarget)],
    cmd: &str,
    timeout: Duration,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Vec<Result<T, String>> {
    let work = targets.iter().enumerate().map(|(i, (_, t))| (i, t.clone(), cmd.to_string())).collect();
    let mut out: Vec<Option<Result<T, String>>> = targets.iter().map(|_| None).collect();
    exec_each_pod(remote, work, timeout, |_, _, &i, call| out[i] = Some(reply(call, &parse))).await;
    out.into_iter().map(|r| r.unwrap_or_else(|| Err("no result".into()))).collect()
}

/// The selected pods with an SSH endpoint, by name (see [`Selected::named_targets`]).
fn sorted_targets(sel: &Selected, cfg: &Config) -> Result<Vec<(String, SshTarget)>> {
    let mut targets = sel.named_targets(cfg)?;
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(targets)
}

/// The conda env jobs run in — `pods run`'s (`CONDA_ENV`, default `arena-env`; empty = none).
fn conda_env(cfg: &Config) -> &str {
    cfg.get("CONDA_ENV").unwrap_or("arena-env")
}

/// What `run --background` says before it starts anything: the dry-run preview (the job's
/// files and the exact wrapper it will run) or the confirm prompt. Both name every pod.
fn start_preview(dry_run: bool, id: &JobId, cmd: &str, conda_env: &str, names: &[&str]) -> Vec<String> {
    if dry_run {
        let mut lines = vec![
            format!("[dry-run] would start job {id} on {} pod(s): {}", names.len(), names.join(", ")),
            format!("  files: ~/{}/{id}/ (cmd, run, log, pid, started_at, exit); `run` is:", jobs::JOBS_DIR),
        ];
        lines.extend(jobs::run_script(id, cmd, Some(conda_env)).lines().map(|l| format!("    {l}")));
        lines
    } else {
        vec![
            format!("Start `{cmd}` in the background on {} pod(s) as job {id}: {}", names.len(), names.join(", ")),
            format!("It keeps running after this command (and your connection) ends; stop it with `arena pods jobs --kill {id}`."),
        ]
    }
}

/// A failed start's report text. A start that timed out may still have launched the job.
fn start_failure(why: &str) -> String {
    if why.starts_with("timed out after") {
        format!("{why} — the job may have started anyway: check `arena pods jobs`")
    } else {
        why.to_string()
    }
}

/// `pods run --background`: start `cmd` detached on every selected pod, as one job id
/// (made from `now_unix` and the command), and return at once with each pod's pid. Confirms
/// first (it's an arbitrary command), `--dry-run` previews. Fails if any pod didn't start it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_start(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    cmd: &str,
    now_unix: u64,
    dry_run: bool,
    yes: bool,
    say: Say<'_>,
) -> Result<()> {
    if cmd.trim().is_empty() {
        anyhow::bail!("nothing to run: the command is empty");
    }
    let targets = sorted_targets(sel, cfg)?;
    if targets.is_empty() {
        say("(no pods with an SSH endpoint)");
        return Ok(());
    }
    let id = JobId::generate(now_unix, cmd);
    let env = conda_env(cfg);
    let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
    if dry_run {
        start_preview(true, &id, cmd, env, &names).iter().for_each(|l| say(l));
        return Ok(());
    }
    if !confirm(yes, &start_preview(false, &id, cmd, env, &names).join("\n"))? {
        say("aborted.");
        return Ok(());
    }
    let remote_cmd = jobs::start_command(&id, cmd, Some(env));
    let work = targets.into_iter().map(|(n, t)| (n, t, remote_cmd.clone())).collect();
    let mut bad = 0;
    exec_each_pod(&remote, work, START_TIMEOUT, |done, total, name, call| {
        match reply(call, |out| jobs::parse_started(out, &id)) {
            Ok(pid) => say(&format!("[{done}/{total}] ✓ {name}: job {id} (pid {pid})")),
            Err(why) => {
                bad += 1;
                say(&format!("[{done}/{total}] ✗ {name}: {}", start_failure(&why)));
            }
        }
    })
    .await;
    say(&format!(
        "Follow: arena pods logs {id} --follow · status: arena pods jobs · stop: arena pods jobs --kill {id}"
    ));
    if bad > 0 {
        anyhow::bail!("{bad} pod(s) failed to start job {id}");
    }
    Ok(())
}

/// `pods jobs`: every selected pod's jobs (or just `only`), as one table. Read-only; fails
/// if any pod couldn't be read.
pub(crate) async fn handle_list(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    only: Option<&JobId>,
    say: Say<'_>,
) -> Result<()> {
    let targets = sorted_targets(sel, cfg)?;
    if targets.is_empty() {
        say("(no pods with an SSH endpoint)");
        return Ok(());
    }
    let replies = each_reply(&remote, &targets, &jobs::list_command(), READ_TIMEOUT, jobs::parse_list).await;
    let pods: Vec<(String, Result<Vec<JobInfo>, String>)> = targets.into_iter().map(|(n, _)| n).zip(replies).collect();
    let (text, failed) = jobs::render_jobs(&pods, only);
    text.lines().for_each(|l| say(l));
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed");
    }
    Ok(())
}

/// `pods jobs --kill JOB`: after a confirm naming the pods, SIGTERM job `id`'s process
/// group wherever it is running (the pod checks the pid is still that job's first). Fails
/// if a pod couldn't be reached or a running job couldn't be signalled.
pub(crate) async fn handle_kill(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    id: &JobId,
    yes: bool,
    say: Say<'_>,
) -> Result<()> {
    let targets = sorted_targets(sel, cfg)?;
    if targets.is_empty() {
        say("(no pods with an SSH endpoint)");
        return Ok(());
    }
    let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
    let prompt = format!(
        "Stop job {id} — SIGTERM to its process group, on whichever of these {} pod(s) it is running: {}",
        names.len(),
        names.join(", ")
    );
    if !confirm(yes, &prompt)? {
        say("aborted.");
        return Ok(());
    }
    let replies = each_reply(&remote, &targets, &jobs::kill_command(id), READ_TIMEOUT, jobs::parse_kill).await;
    let pods: Vec<_> = targets.into_iter().map(|(n, _)| n).zip(replies).collect();
    let (lines, failed) = jobs::render_kill(&pods, id);
    lines.iter().for_each(|l| say(l));
    say("A stopped job reads `exit 143 (SIGTERM)` in `arena pods jobs` once it has exited.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed");
    }
    Ok(())
}

/// How a followed job's stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    /// The job is over (exited, or lost); its final status.
    Over(JobInfo),
    /// The job's directory went away while it was being followed.
    Vanished,
    /// [`FOLLOW_MAX_FAILURES`] reads in a row failed; the last reason.
    GaveUp(String),
}

impl End {
    /// Anything but a clean `exit 0`.
    fn failed(&self) -> bool {
        match self {
            End::Over(info) => info.state.failed(),
            End::Vanished | End::GaveUp(_) => true,
        }
    }
}

/// The line that closes a pod's stream.
fn end_line(name: &str, end: &End) -> String {
    match end {
        End::Over(info) => format!("── {name} · {} · {}", info.id, jobs::state_label(info)),
        End::Vanished => format!("── {name} ✗ the job's files disappeared while following it"),
        End::GaveUp(why) => format!("── {name} ✗ stopped following after {FOLLOW_MAX_FAILURES} failed reads: {why}"),
    }
}

/// `N job(s): M exit 0, K failed (pods…)`, and K.
fn follow_summary(ends: &[(String, End)]) -> (String, usize) {
    let failed: Vec<&str> = ends.iter().filter(|(_, e)| e.failed()).map(|(n, _)| n.as_str()).collect();
    let mut s = format!("{} job(s): {} exit 0", ends.len(), ends.len() - failed.len());
    if !failed.is_empty() {
        s.push_str(&format!(", {} failed ({})", failed.len(), failed.join(", ")));
    }
    (s, failed.len())
}

/// A line of one pod's log while following: prefixed with the pod when several are
/// followed at once.
fn follow_line(multi: bool, name: &str, line: &str) -> String {
    if multi {
        format!("[{name}] {line}")
    } else {
        line.to_string()
    }
}

/// One pod being followed: where, which job (pinned at the first read, so a newer job
/// started meanwhile doesn't take over the stream), and how far.
struct Followed {
    name: String,
    target: SshTarget,
    job: JobId,
    follow: Follow,
}

/// What a pod's follow loop tells the printer.
enum Event {
    Lines(usize, Vec<String>),
    Retry(usize, String),
    End(usize, End),
}

/// `pods logs`: each selected pod's job (`job`, else its newest) — a header and the last
/// `tail` lines. With `follow`, then keep reading every running job until all have ended
/// (`stop()` — Ctrl+C — ends the following early and leaves the jobs alone); the command
/// then fails if any job ended other than `exit 0`. Without it, it fails only if a pod
/// couldn't be read.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_logs<S, F>(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    job: Option<&JobId>,
    tail: usize,
    follow: bool,
    interval: Duration,
    stop: S,
    say: Say<'_>,
) -> Result<()>
where
    S: FnOnce() -> F,
    F: Future<Output = ()>,
{
    let targets = sorted_targets(sel, cfg)?;
    if targets.is_empty() {
        say("(no pods with an SSH endpoint)");
        return Ok(());
    }
    let first = each_reply(&remote, &targets, &jobs::read_command(job, None), READ_TIMEOUT, jobs::parse_read).await;
    let cut_note = format!("… (older lines are beyond the last {} KiB read)", jobs::LOG_READ_CAP / 1024);
    let (mut unread, mut ends, mut followed) = (0, Vec::new(), Vec::new());
    for ((name, target), result) in targets.into_iter().zip(first) {
        match result {
            Err(why) => {
                unread += 1;
                say(&format!("── {name} ✗ {why}"));
            }
            Ok(ReadReply::NoJobs) => say(&format!("── {name} · no jobs")),
            Ok(ReadReply::Missing) => say(&format!("── {name} · no job {}", job.map_or("", JobId::as_str))),
            Ok(ReadReply::Job { info, chunk }) => {
                jobs::job_header(&name, &info).iter().for_each(|l| say(l));
                let (lines, cut) = if follow && !info.state.is_over() {
                    let (f, lines, cut) = Follow::first(&chunk, tail);
                    followed.push(Followed { name: name.clone(), target, job: info.id.clone(), follow: f });
                    (lines, cut)
                } else {
                    if follow {
                        ends.push((name.clone(), End::Over(info.clone())));
                    }
                    jobs::snapshot_lines(&chunk, tail)
                };
                if cut {
                    say(&cut_note);
                }
                lines.iter().for_each(|l| say(l));
            }
        }
    }
    if !follow {
        if unread > 0 {
            anyhow::bail!("{unread} pod(s) failed");
        }
        return Ok(());
    }
    if !followed.is_empty() {
        say(&format!(
            "── following {} job(s) — Ctrl+C stops following (the jobs keep running)",
            followed.len()
        ));
        match follow_all(&remote, followed, interval, stop(), say).await {
            Some(more) => ends.extend(more),
            None => {
                say("stopped following — the jobs keep running on the pods (`arena pods logs` to look again)");
                return Ok(());
            }
        }
    }
    let failed = if ends.is_empty() {
        say("(no job to follow)");
        0
    } else {
        let (summary, failed) = follow_summary(&ends);
        say(&format!("\n{summary}"));
        failed
    };
    match (unread, failed) {
        (0, 0) => Ok(()),
        (0, j) => anyhow::bail!("{j} job(s) did not exit 0"),
        (p, 0) => anyhow::bail!("{p} pod(s) failed"),
        (p, j) => anyhow::bail!("{j} job(s) did not exit 0; {p} pod(s) failed"),
    }
}

/// Follow every pod in `pods` until each job has ended (each pod's [`End`], in `pods`'
/// order) — or `stop` resolves first (`None`; the loops are dropped, the jobs untouched).
async fn follow_all(
    remote: &Arc<dyn Remote>,
    pods: Vec<Followed>,
    interval: Duration,
    stop: impl Future<Output = ()>,
    say: Say<'_>,
) -> Option<Vec<(String, End)>> {
    let multi = pods.len() > 1;
    let names: Vec<String> = pods.iter().map(|p| p.name.clone()).collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut loops = tokio::task::JoinSet::new();
    for (i, pod) in pods.into_iter().enumerate() {
        loops.spawn(follow_one(remote.clone(), i, pod, interval, tx.clone()));
    }
    drop(tx);
    let mut ends: Vec<Option<End>> = names.iter().map(|_| None).collect();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                loops.abort_all();
                return None;
            }
            event = rx.recv() => match event {
                // Every loop has finished (and dropped its sender).
                None => break,
                Some(Event::Lines(i, lines)) => lines.iter().for_each(|l| say(&follow_line(multi, &names[i], l))),
                Some(Event::Retry(i, why)) => say(&format!("✗ {}: {why} — retrying", names[i])),
                Some(Event::End(i, end)) => {
                    say(&end_line(&names[i], &end));
                    ends[i] = Some(end);
                }
            }
        }
    }
    // A loop that died without reporting (a panic) still counts — as a failure.
    Some(names.into_iter().zip(ends).map(|(n, e)| (n, e.unwrap_or_else(|| End::GaveUp("follow task crashed".into())))).collect())
}

/// One pod's follow loop: every `interval`, read the job's log from where it left off and
/// pass on the new lines, until the job is over (or the reads keep failing).
async fn follow_one(
    remote: Arc<dyn Remote>,
    i: usize,
    mut pod: Followed,
    interval: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    let mut failures = 0;
    loop {
        tokio::time::sleep(interval).await;
        let cmd = jobs::read_command(Some(&pod.job), Some(pod.follow.offset()));
        let call = remote.exec(&pod.target, &cmd, Some(READ_TIMEOUT)).await.map_err(|e| describe_error(&e));
        // A send fails only once the printer is gone (stopped): then just end.
        match reply(call, jobs::parse_read) {
            Ok(ReadReply::Job { info, chunk }) => {
                failures = 0;
                let mut lines = pod.follow.next(&chunk);
                let over = info.state.is_over();
                if over {
                    lines.extend(pod.follow.finish());
                }
                if !lines.is_empty() && tx.send(Event::Lines(i, lines)).is_err() {
                    return;
                }
                if over {
                    let _ = tx.send(Event::End(i, End::Over(info)));
                    return;
                }
            }
            Ok(ReadReply::Missing | ReadReply::NoJobs) => {
                let _ = tx.send(Event::End(i, End::Vanished));
                return;
            }
            Err(why) => {
                failures += 1;
                if failures >= FOLLOW_MAX_FAILURES {
                    let _ = tx.send(Event::End(i, End::GaveUp(why)));
                    return;
                }
                if tx.send(Event::Retry(i, why)).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::jobs::fixtures::{job_line, list_reply, read_reply};
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use arena_core::Pod;
    use std::sync::Mutex;
    use tokio::time::Instant;

    /// `devtest-<name>` at `10.0.0.1:<port>` — the FakeRemote host is `10.0.0.1:<port>`.
    fn pod(name: &str, port: u16) -> Pod {
        Pod {
            id: format!("id-devtest-{name}"),
            name: format!("devtest-{name}"),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(port),
            ..Default::default()
        }
    }

    /// apple / bloom / cloud on ports 22001-22003.
    fn fleet() -> Vec<Pod> {
        vec![pod("apple", 22001), pod("bloom", 22002), pod("cloud", 22003)]
    }

    fn host(port: u16) -> String {
        format!("10.0.0.1:{port}")
    }

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n")
    }

    fn id(s: &str) -> JobId {
        JobId::parse(s).unwrap()
    }

    /// Every line said, with when (on the paused clock) it was said.
    #[derive(Default)]
    struct Said(Mutex<Vec<(Duration, String)>>);

    impl Said {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().map(|(_, l)| l.clone()).collect()
        }
        /// When the first line containing `needle` was said.
        fn at(&self, needle: &str) -> Duration {
            self.0.lock().unwrap().iter().find(|(_, l)| l.contains(needle)).unwrap_or_else(|| panic!("never said {needle:?}")).0
        }
    }

    /// A `say` that records into `said`, timed from `start`.
    fn recorder(said: &Said, start: Instant) -> impl FnMut(&str) + '_ {
        move |l: &str| said.0.lock().unwrap().push((start.elapsed(), l.to_string()))
    }

    /// The (cmd, timeout) of every exec to one host, in order.
    fn execs(fake: &FakeRemote, port: u16) -> Vec<(String, Option<Duration>)> {
        fake.calls_to(&host(port))
            .into_iter()
            .filter_map(|c| match c {
                RemoteCall::Exec { cmd, timeout, .. } => Some((cmd, timeout)),
                RemoteCall::Copy { .. } => None,
            })
            .collect()
    }

    /// `[n/total] rest` → `rest`, sorted (pods finishing together have no fixed order).
    fn unnumbered(lines: &[String]) -> Vec<String> {
        let mut v: Vec<String> = lines.iter().map(|l| l.split_once("] ").map_or(l.as_str(), |(_, r)| r).to_string()).collect();
        v.sort();
        v
    }

    const T0: u64 = 1_791_469_381; // 2026-10-08 14:23:01 UTC

    #[tokio::test(start_paused = true)]
    async fn start_is_one_detached_call_per_pod_and_reports_each_pid() {
        let cmd = "pytest -x tests/ -k 'part1 and not $SLOW'";
        let job = JobId::generate(T0, cmd);
        assert_eq!(job.as_str(), "20261008-142301-pytest-x-tests-k-part1");
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(&format!("ARENA_JOB_STARTED {job} 4242\n"))]);
        fake.script(&host(22002), [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
        fake.script(&host(22003), [FakeReply::hang()]);
        let (said, start) = (Said::default(), Instant::now());
        let err = handle_start(fake.clone(), &cfg(), &Selected::all(fleet()), cmd, T0, false, true, &mut recorder(&said, start))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), format!("2 pod(s) failed to start job {job}"));
        assert_eq!(start.elapsed(), START_TIMEOUT, "the wedged pod reports at its budget");
        assert_eq!(said.at("devtest-apple"), Duration::ZERO, "a healthy pod reports at once");
        let lines = said.lines();
        assert_eq!(
            unnumbered(&lines[..3]),
            [
                format!("✓ devtest-apple: job {job} (pid 4242)"),
                "✗ devtest-bloom: exit 255: ssh: connect to host 10.0.0.1 port 22002: Connection refused".to_string(),
                "✗ devtest-cloud: timed out after 30s — the job may have started anyway: check `arena pods jobs`".to_string(),
            ]
        );
        assert!(lines[3].starts_with(&format!("Follow: arena pods logs {job} --follow")), "{lines:?}");
        // The same job id and command everywhere, each call bounded.
        let want = jobs::start_command(&job, cmd, Some("arena-env"));
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(want.clone(), Some(START_TIMEOUT))], "port {port}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_explains_a_pod_side_refusal_and_previews_without_touching_pods() {
        let job = JobId::generate(T0, "true");
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::exit(3, "").with_stdout(&format!("ARENA_JOB_ERR job {job} already exists on this pod\n"))]);
        let said = Said::default();
        let one = Selected::all(vec![pod("apple", 22001)]);
        let err = handle_start(fake.clone(), &cfg(), &one, "true", T0, false, true, &mut recorder(&said, Instant::now()))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("1 pod(s) failed"), "{err}");
        assert_eq!(said.lines()[0], format!("[1/1] ✗ devtest-apple: job {job} already exists on this pod"));

        // --dry-run: the preview names the pods and shows the wrapper; nothing is run.
        let fake = Arc::new(FakeRemote::new());
        let said = Said::default();
        handle_start(fake.clone(), &cfg(), &Selected::all(fleet()), "nvidia-smi", T0, true, true, &mut recorder(&said, Instant::now()))
            .await
            .unwrap();
        assert!(fake.calls().is_empty());
        let lines = said.lines();
        assert_eq!(lines[0], "[dry-run] would start job 20261008-142301-nvidia-smi on 3 pod(s): devtest-apple, devtest-bloom, devtest-cloud");
        assert!(lines.iter().any(|l| l.contains("conda activate arena-env") && l.contains("nvidia-smi")), "{lines:?}");
        // An empty command is refused before anything.
        let e = handle_start(fake.clone(), &cfg(), &Selected::all(fleet()), "  ", T0, false, true, &mut |_| {}).await.unwrap_err();
        assert_eq!(e.to_string(), "nothing to run: the command is empty");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn previews_name_every_pod() {
        let job = id("20261008-142301-nvidia-smi");
        assert_eq!(
            start_preview(false, &job, "nvidia-smi", "arena-env", &["devtest-apple", "devtest-cloud"]),
            [
                "Start `nvidia-smi` in the background on 2 pod(s) as job 20261008-142301-nvidia-smi: devtest-apple, devtest-cloud",
                "It keeps running after this command (and your connection) ends; stop it with `arena pods jobs --kill 20261008-142301-nvidia-smi`.",
            ]
        );
        let dry = start_preview(true, &job, "nvidia-smi", "", &["devtest-apple"]);
        assert_eq!(dry[1], "  files: ~/.arena/jobs/20261008-142301-nvidia-smi/ (cmd, run, log, pid, started_at, exit); `run` is:");
        assert!(dry.iter().any(|l| l == "    zsh -c 'source ~/.zshrc 2>/dev/null; nvidia-smi'"), "{dry:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn jobs_lists_every_pod_and_fails_when_one_cannot_be_read() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(
            &host(22001),
            [FakeReply::stdout(&list_reply(&[
                job_line("20261008-120000-nvidia-smi", "exit", Some(0), Some(77), "nvidia-smi"),
                job_line("20261008-142301-pytest", "running", None, Some(812), "pytest"),
            ]))],
        );
        fake.script(&host(22002), [FakeReply::stdout("motd only — the script never ran\n")]);
        let said = Said::default();
        let err = handle_list(fake.clone(), &cfg(), &Selected::all(fleet()), None, &mut recorder(&said, Instant::now()))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "2 pod(s) failed");
        let lines = said.lines();
        assert!(lines[1].starts_with("devtest-apple  20261008-142301-pytest ") && lines[1].contains("running (pid 812)"), "{lines:?}");
        assert!(lines[2].starts_with("devtest-apple  20261008-120000-nvidia-smi"), "{lines:?}");
        assert_eq!(lines[3], "✗ devtest-bloom: unexpected reply from the pod: motd only — the script never ran");
        // cloud answered with nothing: its reply has no end marker either — never "no jobs".
        assert!(lines[4].starts_with("✗ devtest-cloud: unexpected reply"), "{lines:?}");
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(jobs::list_command(), Some(READ_TIMEOUT))]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn kill_signals_only_after_a_confirm_and_reports_per_pod() {
        let job = id("20261008-142301-pytest");
        let fake = Arc::new(FakeRemote::new());
        let running = job_line(job.as_str(), "running", None, Some(812), "pytest");
        fake.script(&host(22001), [FakeReply::stdout(&format!("{running}\nARENA_KILL_SENT\nARENA_JOBS_END\n"))]);
        fake.script(&host(22002), [FakeReply::stdout("ARENA_JOB_MISSING\nARENA_JOBS_END\n")]);
        fake.script(&host(22003), [FakeReply::exit(255, "Connection refused")]);
        let said = Said::default();
        let err = handle_kill(fake.clone(), &cfg(), &Selected::all(fleet()), &job, true, &mut recorder(&said, Instant::now()))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed");
        assert_eq!(
            said.lines()[..3],
            [
                "✓ devtest-apple: SIGTERM sent to job 20261008-142301-pytest (process group 812)",
                "– devtest-bloom: no job 20261008-142301-pytest",
                "✗ devtest-cloud: exit 255: Connection refused",
            ]
        );
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(jobs::kill_command(&job), Some(READ_TIMEOUT))]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn logs_shows_each_pods_newest_job_and_its_last_lines() {
        let fake = Arc::new(FakeRemote::new());
        let line = job_line("20261008-142301-pytest", "exit", Some(1), Some(812), "pytest -x");
        fake.script(&host(22001), [FakeReply::stdout(&read_reply(&line, 0, 15, b"one\ntwo\nthree\n"))]);
        fake.script(&host(22002), [FakeReply::stdout("ARENA_JOBS_NONE\nARENA_JOBS_END\n")]);
        fake.script(&host(22003), [FakeReply::hang()]);
        let (said, start) = (Said::default(), Instant::now());
        let never = || std::future::pending::<()>();
        let err = handle_logs(fake.clone(), &cfg(), &Selected::all(fleet()), None, 2, false, FOLLOW_INTERVAL, never, &mut recorder(&said, start))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed");
        assert_eq!(
            said.lines(),
            [
                "── devtest-apple · 20261008-142301-pytest · exit 1 · started 2026-10-08T14:23:01Z",
                "$ pytest -x",
                "two",
                "three",
                "── devtest-bloom · no jobs",
                "── devtest-cloud ✗ timed out after 30s",
            ]
        );
        assert_eq!(start.elapsed(), READ_TIMEOUT);
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(jobs::read_command(None, None), Some(READ_TIMEOUT))]);
        }
    }

    /// `--follow`: one read per pod per interval from where it left off, each pod at its
    /// own pace; a wedged read only delays that pod; the command ends when the last job
    /// does and fails because one exited non-zero.
    #[tokio::test(start_paused = true)]
    async fn follow_reads_each_pod_from_its_offset_until_every_job_ends() {
        let job = id("20261008-142301-pytest");
        let at = |state: &str, code| job_line(job.as_str(), state, code, Some(812), "pytest");
        let fake = Arc::new(FakeRemote::new());
        // apple: two lines and half a third, then the rest, then done (exit 0).
        fake.script(
            &host(22001),
            [
                FakeReply::stdout(&read_reply(&at("running", None), 0, 7, b"a\nb\nc-h")),
                FakeReply::stdout(&read_reply(&at("running", None), 7, 13, b"alf\nd\n")),
                FakeReply::stdout(&read_reply(&at("exit", Some(0)), 13, 18, b"done!")),
            ],
        );
        // bloom: already over at the first read — shown, counted, never polled.
        fake.script(&host(22002), [FakeReply::stdout(&read_reply(&at("exit", Some(2)), 0, 4, b"bad\n"))]);
        // cloud: its first poll hangs (timed out, retried), then it reports it was killed.
        fake.script(
            &host(22003),
            [
                FakeReply::stdout(&read_reply(&at("running", None), 0, 2, b"x\n")),
                FakeReply::hang(),
                FakeReply::stdout(&read_reply(&at("exit", Some(143)), 2, 2, b"")),
            ],
        );
        let (said, start) = (Said::default(), Instant::now());
        let never = || std::future::pending::<()>();
        let err = handle_logs(fake.clone(), &cfg(), &Selected::all(fleet()), Some(&job), 5, true, FOLLOW_INTERVAL, never, &mut recorder(&said, start))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "2 job(s) did not exit 0");
        let i = FOLLOW_INTERVAL;
        // apple ends after two polls; cloud after its timed-out poll and one more.
        assert_eq!(said.at("── devtest-apple · 20261008-142301-pytest · exit 0"), 2 * i);
        assert_eq!(said.at("✗ devtest-cloud: timed out after 30s — retrying"), i + READ_TIMEOUT);
        assert_eq!(said.at("── devtest-cloud · 20261008-142301-pytest · exit 143 (SIGTERM)"), 2 * i + READ_TIMEOUT);
        assert_eq!(start.elapsed(), 2 * i + READ_TIMEOUT);
        let lines = said.lines();
        // The first read: headers + last lines (apple's half line held back); bloom is over.
        assert_eq!(
            lines[..8],
            [
                "── devtest-apple · 20261008-142301-pytest · running (pid 812) · started 2026-10-08T14:23:01Z",
                "$ pytest",
                "a",
                "b",
                "── devtest-bloom · 20261008-142301-pytest · exit 2 · started 2026-10-08T14:23:01Z",
                "$ pytest",
                "bad",
                "── devtest-cloud · 20261008-142301-pytest · running (pid 812) · started 2026-10-08T14:23:01Z",
            ]
        );
        assert_eq!(lines[9], "x");
        // Each line once, whole, prefixed by its pod; the unfinished last line flushed at the end.
        let apple: Vec<&str> = lines.iter().filter_map(|l| l.strip_prefix("[devtest-apple] ")).collect();
        assert_eq!(apple, ["c-half", "d", "done!"]);
        assert_eq!(lines.last().unwrap(), "\n3 job(s): 1 exit 0, 2 failed (devtest-bloom, devtest-cloud)");
        // Every poll names the pinned job and the next offset.
        let reads = |port| execs(&fake, port).into_iter().map(|(c, _)| c).collect::<Vec<_>>();
        assert_eq!(
            reads(22001),
            [jobs::read_command(Some(&job), None), jobs::read_command(Some(&job), Some(7)), jobs::read_command(Some(&job), Some(13))]
        );
        assert_eq!(reads(22002).len(), 1);
        assert_eq!(
            reads(22003),
            [jobs::read_command(Some(&job), None), jobs::read_command(Some(&job), Some(2)), jobs::read_command(Some(&job), Some(2))]
        );
        // Following only ever reads: nothing signals a job.
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, .. } if !cmd.contains("ARENA_KILL"))));
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_c_stops_following_never_the_job() {
        let job = id("20261008-142301-pytest");
        let running = read_reply(&job_line(job.as_str(), "running", None, Some(812), "pytest"), 0, 0, b"");
        let fake = Arc::new(FakeRemote::new());
        // Runs forever: every read says "running, nothing new".
        fake.script(&host(22001), std::iter::repeat_n(FakeReply::stdout(&running), 100));
        let (said, start) = (Said::default(), Instant::now());
        let ctrl_c = || tokio::time::sleep(Duration::from_secs(10));
        let one = Selected::all(vec![pod("apple", 22001)]);
        handle_logs(fake.clone(), &cfg(), &one, None, 5, true, FOLLOW_INTERVAL, ctrl_c, &mut recorder(&said, start)).await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(10), "stopped at the Ctrl+C, not at the job's end");
        assert_eq!(said.lines().last().unwrap(), "stopped following — the jobs keep running on the pods (`arena pods logs` to look again)");
        // The first read plus one per interval before the Ctrl+C (at 3, 6 and 9s) — all reads.
        assert_eq!(fake.calls().len(), 4);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("ARENA_LOG"))));
    }

    #[tokio::test(start_paused = true)]
    async fn follow_gives_up_on_a_pod_whose_reads_keep_failing() {
        let job = id("20261008-142301-pytest");
        let fake = Arc::new(FakeRemote::new());
        let first = FakeReply::stdout(&read_reply(&job_line(job.as_str(), "running", None, Some(812), "pytest"), 0, 0, b""));
        let refused = FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22001: Connection refused");
        fake.script(&host(22001), std::iter::once(first).chain(std::iter::repeat_n(refused, 10)));
        let said = Said::default();
        let one = Selected::all(vec![pod("apple", 22001)]);
        let never = || std::future::pending::<()>();
        let err = handle_logs(fake.clone(), &cfg(), &one, Some(&job), 5, true, FOLLOW_INTERVAL, never, &mut recorder(&said, Instant::now()))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 job(s) did not exit 0");
        let lines = said.lines();
        assert_eq!(lines.iter().filter(|l| l.ends_with("— retrying")).count(), FOLLOW_MAX_FAILURES as usize - 1);
        assert!(lines.contains(&format!(
            "── devtest-apple ✗ stopped following after {FOLLOW_MAX_FAILURES} failed reads: exit 255: ssh: connect to host 10.0.0.1 port 22001: Connection refused"
        )));
        assert_eq!(fake.calls().len(), 1 + FOLLOW_MAX_FAILURES as usize);
    }

    /// A provider that lists a fixed fleet (selection runs against it, as in the command).
    struct Fleet(Vec<Pod>);

    #[async_trait::async_trait]
    impl arena_core::Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(self.0.clone())
        }
        async fn create_pod(&self, _spec: &arena_core::PodSpec) -> arena_core::Result<Pod> {
            unimplemented!("not exercised")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!("not exercised")
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!("not exercised")
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!("not exercised")
        }
    }

    fn parse(args: &[&str]) -> std::result::Result<crate::PodCmd, clap::Error> {
        use clap::Parser;
        let argv = ["arena", "pods"].iter().chain(args).copied();
        crate::Cli::try_parse_from(argv).map(|c| match c.cmd {
            crate::Cmd::Pods(p) => p,
            _ => unreachable!(),
        })
    }

    /// `pods <args…>` through the real command handler over `fake`, with --yes.
    async fn pods(args: &[&str], fake: &Arc<FakeRemote>) -> Result<()> {
        let cmd = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let cfg = Config::parse(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n",
        );
        crate::handle_pods(cmd, &Fleet(fleet()), fake.clone(), &cfg, true).await
    }

    #[test]
    fn background_flags_parse_before_the_command_only() {
        use crate::PodCmd;
        let run = |args: &[&str]| match parse(args) {
            Ok(PodCmd::Run { command, background, .. }) => Ok((command, background)),
            Ok(_) => panic!("not a run"),
            Err(e) => Err(e.kind()),
        };
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run(&["run", "--background", "pytest", "-x"]), Ok((words(&["pytest", "-x"]), true)));
        assert_eq!(run(&["run", "-t", "apple", "--bg", "--", "echo", "hi"]), Ok((words(&["echo", "hi"]), true)));
        assert_eq!(run(&["run", "echo", "hi"]), Ok((words(&["echo", "hi"]), false)));
        // A job has no time limit: --timeout with --background is a mistake, said so.
        assert_eq!(run(&["run", "--background", "--timeout", "60", "x"]), Err(clap::error::ErrorKind::ArgumentConflict));
        // logs: -n/--tail bounded, -f, a job among the targets.
        match parse(&["logs", "apple", "20261008-142301-pytest", "-n", "5", "-f"]).unwrap() {
            PodCmd::Logs { tail, follow, sel } => {
                assert_eq!((tail, follow, sel.targets), (5, true, words(&["apple", "20261008-142301-pytest"])));
            }
            _ => panic!("not logs"),
        }
        assert!(matches!(parse(&["logs"]).unwrap(), PodCmd::Logs { tail: DEFAULT_TAIL, follow: false, .. }));
        assert!(parse(&["logs", "--tail", "100001"]).is_err());
        assert!(matches!(parse(&["jobs", "--kill", "20261008-142301-x", "apple"]).unwrap(), PodCmd::Jobs { kill: Some(_), .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn the_commands_reach_exactly_the_selected_pods() {
        let ports = |fake: &FakeRemote| {
            let mut p: Vec<u16> = fake.calls().iter().filter_map(|c| c.host().rsplit_once(':')?.1.parse().ok()).collect();
            p.dedup();
            p
        };
        let job = id("20261008-142301-pytest");
        // logs: targets and a job in any order; the job is pinned in the read.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::stdout("ARENA_JOB_MISSING\nARENA_JOBS_END\n")]);
        pods(&["logs", "20261008-142301-pytest", "bloom"], &fake).await.unwrap();
        assert_eq!(execs(&fake, 22002), [(jobs::read_command(Some(&job), None), Some(READ_TIMEOUT))]);
        assert_eq!(ports(&fake), [22002]);
        // jobs: a job among the targets filters; --kill on a range.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(&list_reply(&[]))]);
        pods(&["jobs", "apple", "20261008-142301-pytest"], &fake).await.unwrap();
        assert_eq!(execs(&fake, 22001), [(jobs::list_command(), Some(READ_TIMEOUT))]);
        let fake = Arc::new(FakeRemote::new());
        for port in [22002, 22003] {
            fake.script(&host(port), [FakeReply::stdout("ARENA_JOB_MISSING\nARENA_JOBS_END\n")]);
        }
        pods(&["jobs", "--kill", "20261008-142301-pytest", "bloom..cloud"], &fake).await.unwrap();
        assert_eq!(ports(&fake), [22002, 22003]);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, .. } if *cmd == jobs::kill_command(&job))));
        // run --background: one start per selected pod.
        let fake = Arc::new(FakeRemote::new());
        let e = pods(&["run", "--bg", "--exclude", "apple", "true"], &fake).await.unwrap_err();
        assert!(e.to_string().starts_with("2 pod(s) failed to start job"), "{e}"); // the fake's empty replies
        assert_eq!(ports(&fake), [22002, 22003]);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, timeout, .. }
            if cmd.contains("ARENA_JOB_STARTED") && *timeout == Some(START_TIMEOUT))));

        // Refused before any pod is reached: a bad id (it would go into a remote path), two
        // different jobs, a trailing --background (it would run in the foreground), a typo.
        for (argv, why) in [
            (&["jobs", "--kill", "../../etc"][..], "is not a job id"),
            (&["jobs", "--kill", "20261008-142301-$(reboot)"], "is not a job id"),
            (&["jobs", "--kill", "20261008-142301-a", "20261008-142301-b"], "name one job"),
            (&["logs", "20261008-142301-a", "20261008-142301-b"], "one job at a time"),
            (&["logs", "20261008-142301-UPPER"], "is not a job id"),
            (&["run", "pytest", "--background"], "`--background` after the command"),
            (&["run", "pytest", "--bg"], "`--bg` after the command"),
            (&["logs", "aple"], "aple"),
        ] {
            let fake = Arc::new(FakeRemote::new());
            let e = pods(argv, &fake).await.unwrap_err().to_string();
            assert!(e.contains(why), "{argv:?}: {e}");
            assert!(fake.calls().is_empty(), "{argv:?}");
        }
    }

    #[test]
    fn follow_summaries_and_end_lines() {
        let info = |state| JobInfo { id: id("20261008-142301-x"), started: None, state, pid: None, cmd: "x".into() };
        let ends = vec![
            ("devtest-apple".to_string(), End::Over(info(jobs::JobState::Exited(0)))),
            ("devtest-bloom".to_string(), End::Over(info(jobs::JobState::Lost))),
            ("devtest-cloud".to_string(), End::Vanished),
        ];
        assert_eq!(follow_summary(&ends), ("3 job(s): 1 exit 0, 2 failed (devtest-bloom, devtest-cloud)".to_string(), 2));
        assert_eq!(follow_summary(&ends[..1]), ("1 job(s): 1 exit 0".to_string(), 0));
        assert_eq!(end_line("devtest-bloom", &ends[1].1), "── devtest-bloom · 20261008-142301-x · lost (ended without an exit code)");
        assert_eq!(end_line("devtest-cloud", &End::Vanished), "── devtest-cloud ✗ the job's files disappeared while following it");
        assert_eq!((follow_line(true, "devtest-apple", "ok"), follow_line(false, "devtest-apple", "ok")), ("[devtest-apple] ok".into(), "ok".into()));
    }
}
