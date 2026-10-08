//! `arena teardown --check`: the end-of-program audit (ops playbook §5, §9). This side does
//! the reading — every source at once, each bounded — and `arena_core::teardown` judges it.
//!
//! Read-only by construction: provider *list* calls, the RunPod volume listing, the
//! OpenRouter key listing, the local proxy file, and three local commands that only read
//! (`crontab -l`, `atq`, `at -c <id>`). Nothing here can delete, stop or edit anything; the
//! checklist prints the commands that would, for the operator to run.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use arena_core::openrouter::{KeyApi, KeyInfo};
use arena_core::provider::runpod::NetworkVolume;
use arena_core::provider::{runpod, runpod_v2, RunpodApi, LIST_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::teardown::{self, AtJob, CronLines, Inputs, Probe, ProxyFile, Report};
use arena_core::{Config, Provider};

use super::{arena_cron_lines, expand_tilde, openrouter_client};

/// Budget for each local scheduler command. They read local files and answer at once; the
/// bound is for a wedged NFS home or a hung `atd` socket, so the check reports `?` instead
/// of hanging.
const SCHED_TIMEOUT: Duration = Duration::from_secs(15);

/// `arena teardown [--check] [--json]`. Only the check exists: plain `teardown` is refused
/// rather than guessed at, so a script written for the check can never end up running a
/// future destructive form by accident.
pub(crate) async fn handle_teardown(provider: &dyn Provider, cfg: &Config, check: bool, json: bool) -> Result<()> {
    if !check {
        anyhow::bail!(
            "`arena teardown` only checks for now — run `arena teardown --check` (read-only): it lists what is still \
             billing or scheduled and prints the command that cleans up each item"
        );
    }
    let or = openrouter_client(cfg);
    let keys = or.as_ref().map(|o| o as &dyn KeyApi);
    teardown_with(provider, cfg, read_volumes(cfg), keys, &SystemSched, json).await
}

/// The check against these sources (the real ones, or fakes in tests): print the checklist
/// (or `--json`), then fail unless everything is clear — something remaining *or* unknown
/// makes the exit non-zero.
pub(crate) async fn teardown_with(
    provider: &dyn Provider,
    cfg: &Config,
    volumes: impl Future<Output = Probe<Vec<NetworkVolume>>>,
    keys: Option<&dyn KeyApi>,
    sched: &dyn Sched,
    json: bool,
) -> Result<()> {
    let report = collect(provider, cfg, volumes, keys, sched).await;
    print!("{}", render_output(&report, json)?);
    exit_status(&report)
}

/// What the check prints: the checklist, or with `--json` the report as JSON. Pure, so
/// both forms are tested without capturing stdout.
pub(crate) fn render_output(report: &Report, json: bool) -> Result<String> {
    Ok(if json { format!("{}\n", serde_json::to_string_pretty(report)?) } else { report.render() })
}

/// The exit: `Ok` only when everything is clear — something remaining *or* unknown fails.
fn exit_status(report: &Report) -> Result<()> {
    if report.clear {
        Ok(())
    } else {
        anyhow::bail!("teardown check: {} item(s) remain, {} couldn't be checked", report.remaining, report.unknown)
    }
}

/// Read every source concurrently and judge them.
async fn collect(
    provider: &dyn Provider,
    cfg: &Config,
    volumes: impl Future<Output = Probe<Vec<NetworkVolume>>>,
    keys: Option<&dyn KeyApi>,
    sched: &dyn Sched,
) -> Report {
    // `list_by_provider`, not `list_pods`: the fleet's `list_pods` drops a backend that
    // failed, which here would read as "that provider has no pods".
    let (listings, volumes, keys, cron, at) =
        tokio::join!(provider.list_by_provider(), volumes, read_keys(keys), read_cron(sched), read_at(sched));
    let inputs = Inputs {
        listings: listings.into_iter().map(|(p, r)| (p, r.map_err(|e| e.to_string()))).collect(),
        volumes,
        keys,
        cron,
        at,
        proxy: read_proxy(cfg),
        user: current_user(),
    };
    teardown::build(&inputs, &Naming::from_config(cfg))
}

/// RunPod's network volumes: REST v2 on `RUNPOD_API=v2` (falling back to GraphQL once), else
/// GraphQL. Bounded like a provider listing; any failure is "couldn't check", never "none".
async fn read_volumes(cfg: &Config) -> Probe<Vec<NetworkVolume>> {
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|k| !k.is_empty()) else {
        return Probe::Skipped("RunPod isn't configured (no RUNPOD_API_KEY) — not checked".into());
    };
    let fetch = async {
        match RunpodApi::from_config(cfg)? {
            RunpodApi::V1 => runpod::fetch_network_volumes(key).await,
            RunpodApi::V2 => match runpod_v2::fetch_network_volumes(key).await {
                Ok(v) => Ok(v),
                Err(v2) => runpod::fetch_network_volumes(key)
                    .await
                    .map_err(|gql| arena_core::Error::provider(format!("{v2}; GraphQL fallback: {gql}"))),
            },
        }
    };
    match tokio::time::timeout(LIST_TIMEOUT, fetch).await {
        Ok(Ok(v)) => Probe::Got(v),
        Ok(Err(e)) => Probe::Failed(e.to_string()),
        Err(_) => Probe::Failed(format!("timed out after {}s", LIST_TIMEOUT.as_secs())),
    }
}

/// Every OpenRouter key on the account (all pages); `teardown::keys_item` picks this cohort's.
async fn read_keys(api: Option<&dyn KeyApi>) -> Probe<Vec<KeyInfo>> {
    let Some(api) = api else {
        return Probe::Skipped("OPENROUTER_PROVISIONING_KEY isn't set — OpenRouter keys not checked".into());
    };
    match tokio::time::timeout(LIST_TIMEOUT, api.list_keys()).await {
        Ok(Ok(keys)) => Probe::Got(keys),
        Ok(Err(e)) => Probe::Failed(e.to_string()),
        Err(_) => Probe::Failed(format!("timed out after {}s", LIST_TIMEOUT.as_secs())),
    }
}

/// The local proxy config — never fetched over SSH (like `snapshot`): a remote proxy is
/// "couldn't check", with where to look.
fn read_proxy(cfg: &Config) -> Probe<ProxyFile> {
    let Ok(px) = arena_core::proxy::ProxyConfig::from_config(cfg) else {
        return Probe::Skipped("no proxy configured (no SSH_PROXY_HOST) — not checked".into());
    };
    if !px.local {
        return Probe::Failed(format!(
            "the proxy is remote ({}@{}:{}) and this check never connects to it — look there for `# arena-forward` blocks",
            px.proxy_user, px.proxy_host, px.nginx_path
        ));
    }
    let path = expand_tilde(&px.nginx_path);
    match std::fs::read_to_string(&path) {
        Ok(text) => Probe::Got(ProxyFile { path, text }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::Got(ProxyFile { path, text: String::new() }),
        Err(e) => Probe::Failed(format!("can't read {path}: {e}")),
    }
}

/// Whose crontab and `at` queue the local commands read.
fn current_user() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "this user".into())
}

/// What running one local command gave.
#[derive(Debug, Clone)]
pub(crate) enum SchedOut {
    /// The program isn't installed.
    Missing,
    /// It ran: whether it exited 0, and what it printed.
    Ran { ok: bool, stdout: String, stderr: String },
    /// It couldn't be started, or didn't finish within [`SCHED_TIMEOUT`].
    Error(String),
}

/// How the check runs this machine's scheduler tools — `crontab -l`, `atq`, `at -c <id>`,
/// all read-only — for real, or canned in tests: so how their exit codes and messages are
/// read is tested without depending on (or touching) the machine's own crontab and queue.
#[async_trait::async_trait]
pub(crate) trait Sched: Send + Sync {
    async fn run(&self, argv: &[&str]) -> SchedOut;
}

/// The real commands, each bounded by [`SCHED_TIMEOUT`] (killed if it overruns).
pub(crate) struct SystemSched;

#[async_trait::async_trait]
impl Sched for SystemSched {
    async fn run(&self, argv: &[&str]) -> SchedOut {
        let Some((prog, args)) = argv.split_first() else {
            return SchedOut::Error("no command".into());
        };
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        match tokio::time::timeout(SCHED_TIMEOUT, cmd.output()).await {
            Err(_) => SchedOut::Error(format!("`{}` timed out after {}s", argv.join(" "), SCHED_TIMEOUT.as_secs())),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => SchedOut::Missing,
            Ok(Err(e)) => SchedOut::Error(format!("running `{}`: {e}", argv.join(" "))),
            Ok(Ok(out)) => SchedOut::Ran {
                ok: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
        }
    }
}

/// The first line of a command's stderr, for a one-line reason.
fn first_line(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no message)");
    line.chars().take(200).collect()
}

/// This user's crontab, split into arena's block and hand-added arena lines. "no crontab for
/// <user>" is an empty crontab; any other failure is "couldn't check".
async fn read_cron(sched: &dyn Sched) -> Probe<CronLines> {
    match sched.run(&["crontab", "-l"]).await {
        SchedOut::Missing => Probe::Skipped("`crontab` isn't installed here — no user crontab to check".into()),
        SchedOut::Error(e) => Probe::Failed(e),
        SchedOut::Ran { ok: true, stdout, .. } => Probe::Got(arena_cron_lines(&stdout)),
        SchedOut::Ran { stderr, .. } if stderr.contains("no crontab for") => Probe::Got(CronLines::default()),
        SchedOut::Ran { stderr, .. } => Probe::Failed(format!("`crontab -l` failed: {}", first_line(&stderr))),
    }
}

/// The pending `at` jobs with their scripts (`at -c <id>`, one at a time). `at` not installed
/// is said and skipped; a job whose script can't be read is kept, as unreadable.
async fn read_at(sched: &dyn Sched) -> Probe<Vec<AtJob>> {
    let listed = match sched.run(&["atq"]).await {
        SchedOut::Missing => {
            return Probe::Skipped("`at` isn't installed here (no `atq`) — nothing can be scheduled with it".into())
        }
        SchedOut::Error(e) => return Probe::Failed(e),
        SchedOut::Ran { ok: false, stderr, .. } => return Probe::Failed(format!("`atq` failed: {}", first_line(&stderr))),
        SchedOut::Ran { stdout, .. } => stdout,
    };
    let lines = match teardown::parse_atq(&listed) {
        Ok(lines) => lines,
        Err(e) => return Probe::Failed(e),
    };
    let mut jobs = Vec::new();
    for job in lines {
        let script = match sched.run(&["at", "-c", &job.id]).await {
            SchedOut::Ran { ok: true, stdout, .. } => Ok(stdout),
            SchedOut::Ran { stderr, .. } => Err(format!("`at -c {}` failed: {}", job.id, first_line(&stderr))),
            SchedOut::Missing => Err("`at` isn't installed (only `atq` is)".into()),
            SchedOut::Error(e) => Err(e),
        };
        jobs.push(AtJob { job, script });
    }
    Probe::Got(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle_tests::FakeKeys;
    use crate::{keys_with, Cli, Cmd, CRON_BEGIN, CRON_END};
    use arena_core::remote::FakeRemote;
    use arena_core::teardown::Verdict;
    use arena_core::{Error, Pod, PodSpec};
    use async_trait::async_trait;
    use clap::Parser;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// A fleet whose listing is scripted per backend (`Err` = that backend failed to list).
    /// Every mutating call panics: the check must never make one.
    struct Fleet {
        backends: Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>,
    }

    #[async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(self.backends.iter().filter_map(|(_, r)| r.clone().ok()).flatten().collect())
        }
        async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
            self.backends.iter().map(|(n, r)| (n.to_string(), r.clone().map_err(Error::provider))).collect()
        }
        async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
            panic!("teardown --check must not create")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("teardown --check must not stop")
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("teardown --check must not restart")
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("teardown --check must not terminate")
        }
    }

    /// Canned command outputs by argv (`crontab -l`, `atq`, `at -c N`); records every call.
    /// An argv without an answer is "not installed".
    #[derive(Default)]
    struct FakeSched {
        answers: HashMap<String, SchedOut>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSched {
        fn with(answers: &[(&str, SchedOut)]) -> Self {
            Self { answers: answers.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), ..Default::default() }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Sched for FakeSched {
        async fn run(&self, argv: &[&str]) -> SchedOut {
            let key = argv.join(" ");
            self.calls.lock().unwrap().push(key.clone());
            self.answers.get(&key).cloned().unwrap_or(SchedOut::Missing)
        }
    }

    fn ran(stdout: &str) -> SchedOut {
        SchedOut::Ran { ok: true, stdout: stdout.into(), stderr: String::new() }
    }
    fn failed(stderr: &str) -> SchedOut {
        SchedOut::Ran { ok: false, stdout: String::new(), stderr: stderr.into() }
    }

    /// at 3.2's `at -c` shape: the environment (a key in it), the `cd` block, a heredoc.
    fn at_script(cmd: &str) -> String {
        format!(
            "#!/bin/sh\n# atrun uid=1000 gid=1000\numask 22\nRUNPOD_API_KEY=rpa_SECRETSECRET; export RUNPOD_API_KEY\n\
             cd /home/dev || {{\n\t echo 'Execution directory inaccessible' >&2\n\t exit 1\n}}\n\
             ${{SHELL:-/bin/sh}} << 'marcinDELIMITER5c6b4c2b'\n{cmd}\nmarcinDELIMITER5c6b4c2b\n"
        )
    }

    fn crontab_with_block() -> String {
        format!(
            "MAILTO=ops@example.org\n0 3 * * * /usr/bin/certbot renew\n{CRON_BEGIN}\n\
             */15 * * * * /usr/local/bin/arena --config /c pods backup --no-pull --yes >> /h/arena-cron.log 2>&1\n{CRON_END}\n"
        )
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-teardown-cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// A local, write-only proxy whose file is `proxy.conf` in `dir`; no RunPod key (the
    /// tests hand the volume listing in), so nothing here can reach the network.
    fn cfg(dir: &Path) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud @james-gpu)\n\
             SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\nSSH_PROXY_STARTING_PORT=9500\n",
            dir.join("proxy.conf").display()
        ))
    }

    fn pod(name: &str, status: &str) -> Pod {
        Pod { id: format!("id-{name}"), name: name.into(), provider: "runpod".into(), status: status.into(), ..Default::default() }
    }

    fn vol(id: &str, gb: u32) -> NetworkVolume {
        NetworkVolume { id: id.into(), name: format!("{id}-data"), size_gb: Some(gb), data_center: None, tier: None }
    }

    async fn check(
        fleet: &Fleet,
        cfg: &Config,
        volumes: Probe<Vec<NetworkVolume>>,
        keys: Option<&FakeKeys>,
        sched: &FakeSched,
    ) -> (Report, std::result::Result<(), String>) {
        let keys = keys.map(|k| k as &dyn KeyApi);
        let report = collect(fleet, cfg, std::future::ready(volumes.clone()), keys, sched).await;
        let exit = teardown_with(fleet, cfg, std::future::ready(volumes), keys, sched, false).await.map_err(|e| e.to_string());
        (report, exit)
    }

    #[tokio::test]
    async fn teardown_flags_parse_and_plain_teardown_is_refused() {
        let parse = |argv: &[&str]| Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd);
        assert!(matches!(parse(&["teardown", "--check"]).unwrap(), Cmd::Teardown { check: true, json: false }));
        assert!(matches!(parse(&["teardown", "--check", "--json"]).unwrap(), Cmd::Teardown { check: true, json: true }));
        assert!(matches!(parse(&["teardown"]).unwrap(), Cmd::Teardown { check: false, .. }));
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING")]))] };
        let e = handle_teardown(&fleet, &Config::parse(""), false, false).await.unwrap_err().to_string();
        assert!(e.contains("run `arena teardown --check`"), "{e}");
    }

    /// `crontab -l`'s answers: a crontab (arena's block and hand-added lines read, the rest
    /// ignored), "no crontab" = empty, any other failure = couldn't check, not installed =
    /// skipped.
    #[tokio::test]
    async fn crontab_answers_are_read_by_exit_code_and_message() {
        let hand = "0 9 * * * /usr/local/bin/arena pods up -n 2 --yes";
        let cases: Vec<(SchedOut, Probe<CronLines>)> = vec![
            (
                ran(&format!("{}{hand}\n", crontab_with_block())),
                Probe::Got(CronLines {
                    managed: vec![
                        "*/15 * * * * /usr/local/bin/arena --config /c pods backup --no-pull --yes >> /h/arena-cron.log 2>&1".into(),
                    ],
                    unmanaged: vec![hand.into()],
                }),
            ),
            (failed("no crontab for dev\n"), Probe::Got(CronLines::default())),
            (failed("crontab: your UID isn't in the passwd file.\nbye\n"), Probe::Failed("`crontab -l` failed: crontab: your UID isn't in the passwd file.".into())),
            (SchedOut::Error("`crontab -l` timed out after 15s".into()), Probe::Failed("`crontab -l` timed out after 15s".into())),
            (SchedOut::Missing, Probe::Skipped("`crontab` isn't installed here — no user crontab to check".into())),
        ];
        for (answer, want) in cases {
            let s = FakeSched::with(&[("crontab -l", answer.clone())]);
            assert_eq!(read_cron(&s).await, want, "{answer:?}");
            assert_eq!(s.calls(), ["crontab -l"], "only reads");
        }
    }

    /// `atq` then `at -c <id>` per job: scripts kept (judged in core), a failed `at -c` kept
    /// as unreadable, junk `atq` output is couldn't-check, no `atq` is skipped.
    #[tokio::test]
    async fn at_queue_is_read_job_by_job() {
        let s = FakeSched::with(&[
            ("atq", ran("3\tThu Oct  9 14:00:00 2026 a dev\n4\tFri Oct 10 09:00:00 2026 a dev\n")),
            ("at -c 3", ran(&at_script("destroy_pods --yes apple"))),
            ("at -c 4", failed("Cannot find jobid 4\n")),
        ]);
        let Probe::Got(jobs) = read_at(&s).await else { panic!("expected jobs") };
        assert_eq!(s.calls(), ["atq", "at -c 3", "at -c 4"], "only reads");
        assert_eq!((jobs[0].job.id.as_str(), jobs[0].job.when.as_str()), ("3", "Thu Oct 9 14:00:00 2026"));
        assert!(jobs[0].script.as_ref().unwrap().contains("destroy_pods --yes apple"));
        assert_eq!(jobs[1].script, Err("`at -c 4` failed: Cannot find jobid 4".into()));
        let missing = read_at(&FakeSched::default()).await;
        assert!(matches!(&missing, Probe::Skipped(w) if w.contains("`at` isn't installed")), "{missing:?}");
        let junk = read_at(&FakeSched::with(&[("atq", ran("Cannot open lockfile\n"))])).await;
        assert!(matches!(&junk, Probe::Failed(e) if e.contains("unexpected atq line")), "{junk:?}");
        let denied = read_at(&FakeSched::with(&[("atq", failed("You do not have permission to use atq.\n"))])).await;
        assert_eq!(denied, Probe::Failed("`atq` failed: You do not have permission to use atq.".into()));
    }

    /// Everything clear → exit 0, and nothing was mutated anywhere.
    #[tokio::test]
    async fn all_clear_exits_zero() {
        let dir = tmp("clear");
        let cfg = cfg(&dir.0);
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-old", "TERMINATED")])), ("hetzner", Ok(vec![]))] };
        let keys = FakeKeys::with(&[("h-7", "arena7-apple")]); // another cohort's: not ours
        let sched = FakeSched::with(&[("crontab -l", ran("0 3 * * * /usr/bin/certbot renew\n")), ("atq", ran(""))]);
        let (report, exit) = check(&fleet, &cfg, Probe::Got(vec![]), Some(&keys), &sched).await;
        assert_eq!(exit, Ok(()), "{}", report.render());
        assert!(report.clear);
        assert!(keys.calls().is_empty(), "keys are only listed");
        assert!(report.render().contains("✓ proxy forwards"), "a missing proxy file has no forwards: {}", report.render());
    }

    /// A stopped pod, a failed provider, a volume, a key, the cron block, an arena `at` job and
    /// a forward: every one reported, the exit non-zero — and still nothing mutated.
    #[tokio::test]
    async fn leftovers_and_unknowns_fail_the_check_with_their_fixes() {
        use arena_core::proxy::{render_nginx, Forward};
        let dir = tmp("left");
        let cfg = cfg(&dir.0);
        let fwd = Forward {
            name: "devtest-apple".into(),
            public_port: 9500,
            target_ip: "203.0.113.7".into(),
            target_port: 22001,
            provider: Some("runpod".into()),
            pod_id: None,
        };
        std::fs::write(dir.0.join("proxy.conf"), render_nginx(&[fwd])).unwrap();
        let fleet = Fleet {
            backends: vec![("runpod", Ok(vec![pod("devtest-bloom", "EXITED")])), ("vast", Err("list pods HTTP 429"))],
        };
        let keys = FakeKeys::with(&[("h-a", "devtest-apple")]);
        let sched = FakeSched::with(&[
            ("crontab -l", ran(&crontab_with_block())),
            ("atq", ran("3\tThu Oct  9 14:00:00 2026 a dev\n")),
            ("at -c 3", ran(&at_script("destroy_pods --yes apple"))),
        ]);
        let (report, exit) = check(&fleet, &cfg, Probe::Got(vec![vol("vol1", 100)]), Some(&keys), &sched).await;
        let verdicts: Vec<(String, Verdict)> =
            report.items.iter().map(|i| (format!("{:?} {}", i.area, i.scope), i.verdict)).collect();
        let want = |a: &str| verdicts.iter().find(|(k, _)| k.starts_with(a)).map(|(_, v)| *v).unwrap();
        assert_eq!(want("Pods runpod"), Verdict::Remaining, "a stopped pod remains");
        assert_eq!(want("Pods vast"), Verdict::Unknown, "a failed listing is never empty");
        assert_eq!(want("Pods hetzner"), Verdict::Skipped);
        for area in ["Volumes", "Keys", "Cron", "At", "Proxy"] {
            assert_eq!(want(area), Verdict::Remaining, "{area}: {verdicts:?}");
        }
        assert_eq!(exit, Err("teardown check: 6 item(s) remain, 1 couldn't be checked".into()));
        assert!(keys.calls().is_empty(), "keys are only listed");
        let text = report.render();
        for needle in [
            "✗ pods (runpod): 1 remains — 0 billing, 1 stopped",
            "? pods (vast): couldn't list (provider error: list pods HTTP 429) — NOT known to be empty",
            // Not `--all`: vast couldn't be listed, so what `--all` would reach there is unknown.
            "fix:  arena pods terminate id-devtest-bloom",
            "fix:  arena cron remove",
            "fix:  atrm 3",
            "fix:  arena keys revoke devtest-apple",
            "fix:  arena proxy apply",
            "network-volumes/vol1",
            "~$7.00/month",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(render_output(&report, false).unwrap(), text, "the text form is the checklist");
        // --json: the same report from the same inputs, machine-readable, the same exit.
        let printed = render_output(&report, true).unwrap();
        assert!(!printed.contains("SECRET"), "{printed}");
        let v: serde_json::Value = serde_json::from_str(&printed).unwrap_or_else(|e| panic!("{e}: {printed}"));
        assert_eq!(v, serde_json::to_value(&report).unwrap());
        assert_eq!((v["clear"].as_bool(), v["remaining"].as_u64(), v["unknown"].as_u64()), (Some(false), Some(6), Some(1)));
        let verdict = |area: &str, scope: &str| {
            v["items"].as_array().unwrap().iter().find(|i| i["area"] == area && i["scope"] == scope).map(|i| i["verdict"].clone())
        };
        assert_eq!(verdict("pods", "vast"), Some("unknown".into()));
        assert_eq!(verdict("volumes", "runpod"), Some("remaining".into()));
        let exit_json = teardown_with(&fleet, &cfg, std::future::ready(Probe::Got(vec![vol("vol1", 100)])), Some(&keys), &sched, true)
            .await
            .map_err(|e| e.to_string());
        assert_eq!(exit_json, exit, "--json exits like the checklist");
        assert!(keys.calls().is_empty(), "keys are only listed");
    }

    /// The `arena …` fix lines of a report, as argv.
    fn arena_fixes(report: &Report) -> Vec<Vec<String>> {
        report
            .items
            .iter()
            .flat_map(|i| i.fix.iter())
            .filter(|f| f.starts_with("arena "))
            .map(|f| f.split("  #").next().unwrap().split_whitespace().map(String::from).collect())
            .collect()
    }

    /// Every `arena …` fix line the checklist prints is a real command line — and the keys
    /// one really revokes this cohort's keys once every pod is gone (where `keys revoke
    /// --all` would revoke nothing: it targets current pods), never a staff box's
    /// (`@james-gpu`) or another cohort's.
    #[tokio::test]
    async fn the_printed_fixes_parse_and_the_keys_fix_works_with_no_pods_left() {
        let dir = tmp("fixes");
        let cfg = cfg(&dir.0);
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-bloom", "EXITED")]))] };
        let keys = FakeKeys::with(&[("h-a", "devtest-apple"), ("h-j", "james-gpu"), ("h-7", "arena7-apple")]);
        let sched = FakeSched::with(&[("crontab -l", ran(&crontab_with_block())), ("atq", ran(""))]);
        let (report, _) = check(&fleet, &cfg, Probe::Got(vec![]), Some(&keys), &sched).await;
        let fixes = arena_fixes(&report);
        assert_eq!(fixes.len(), 3, "{fixes:?}"); // pods, keys, cron
        for argv in &fixes {
            Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        assert_eq!(fixes[0], ["arena", "pods", "terminate", "--all"], "only the cohort's pods are left");
        let revoke = fixes.iter().find(|a| a[1] == "keys").unwrap();
        assert_eq!(revoke, &["arena", "keys", "revoke", "devtest-apple"]);
        let Cmd::Keys(k) = Cli::try_parse_from(revoke).unwrap().cmd else { unreachable!() };
        let gone = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        keys_with(k, &gone, Arc::new(FakeRemote::new()), &cfg, &keys, &dir.0, true).await.unwrap();
        assert_eq!(keys.calls(), ["delete h-a"]);
        assert_eq!(keys.names(), ["arena7-apple", "james-gpu"], "another cohort's and the staff box's keys are left alone");
    }

    /// With a staff box (`@james-gpu`) or a stranger anywhere on the account, the pods fix is
    /// one `terminate <id>` per cohort pod — each a real command line — and never `--all`,
    /// which would destroy them too (even when printed under a provider that holds only the
    /// cohort's pods: `--all` reaches every provider); they're named for the operator.
    #[tokio::test]
    async fn the_pods_fix_never_sweeps_in_a_staff_box() {
        let dir = tmp("staff");
        let cfg = cfg(&dir.0);
        let fleet = Fleet {
            backends: vec![
                ("runpod", Ok(vec![pod("devtest-bloom", "EXITED")])),
                ("vast", Ok(vec![pod("devtest-apple", "RUNNING"), pod("james-gpu", "RUNNING"), pod("registered_pink_prawn", "RUNNING")])),
            ],
        };
        let sched = FakeSched::with(&[("crontab -l", ran("")), ("atq", ran(""))]);
        let (report, exit) = check(&fleet, &cfg, Probe::Got(vec![]), None, &sched).await;
        assert!(exit.is_err(), "they all still bill");
        let text = report.render();
        assert!(report.items.iter().flat_map(|i| &i.fix).all(|f| !f.contains("--all")), "{text}");
        let fixes = arena_fixes(&report);
        assert_eq!(
            fixes,
            [["arena", "pods", "terminate", "id-devtest-bloom"], ["arena", "pods", "terminate", "id-devtest-apple"]],
            "{text}"
        );
        for argv in &fixes {
            let Cmd::Pods(_) = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}")).cmd else {
                panic!("{argv:?}")
            };
        }
        for needle in [
            "james-gpu              id=id-james-gpu  RUNNING  billing  (staff box — not this cohort's)",
            "registered_pink_prawn  id=id-registered_pink_prawn  RUNNING  billing  (not this cohort's)",
            "note: 2 not this cohort's (james-gpu (staff box: `@` list entry), registered_pink_prawn) — decide by hand",
            "note: one command per pod, not `arena pods terminate --all`",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
    }
}
