//! Money: `arena balance` (what's left on each provider account and how long it lasts), the
//! one-line balance under `pods list`, the informational lines under `teardown --check`, and
//! `pods idle` (which pods look abandoned — a report, never an action).
//!
//! Judging and rendering live in `arena_core::balance` / `arena_core::idle` (pure, tested on
//! fixture-shaped replies). Here is only the reading: provider accounts and listings, each
//! bounded, and one bounded read-only SSH probe per pod.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::balance::{self, AccountProbe, Burns};
use arena_core::idle::{self, Probe};
use arena_core::remote::Remote;
use arena_core::ssh::SshTarget;
use arena_core::status::bills_hourly;
use arena_core::{Config, Pod, Provider};

use super::jobs::Say;
use super::{exec_each_pod, PodCall, Selected, ENRICH_TIMEOUT};

/// How long `pods list` waits for the balances (read alongside the listing, so usually not
/// at all): the list never waits longer than this for its footer.
pub(crate) const FOOTER_TIMEOUT: Duration = Duration::from_secs(15);

/// Budget for one pod's idle probe: the ssh connect (10 s), three GPU samples a second
/// apart, and the file scan (20 s at most) — a healthy pod answers in a few seconds.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// `--hours` for `pods idle`: a positive number of hours, at most a year.
pub(crate) fn parse_hours(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.trim().trim_end_matches('h').parse().map_err(|_| format!("`{s}` is not a number of hours (e.g. 6)"))?;
    if v.is_finite() && v > 0.0 && v <= 24.0 * 365.0 {
        Ok(v)
    } else {
        Err(format!("must be more than 0 and at most 8760 hours (got {s})"))
    }
}

/// The lines under `pods list`'s fleet footer: one balance line, then any ⚠. Burn per
/// provider comes from this same listing; a provider whose listing `failed` (already warned
/// about above) has an unknown fleet cost — `?`, never "no billing pods". Pure.
pub(crate) fn list_footer(probes: &[AccountProbe], pods: &[Pod], failed: &[String], cfg: &Config, now: u64) -> Vec<String> {
    let (warn_hours, bad_config) = balance::warn_hours_or_default(cfg);
    let report = balance::report(probes, &balance::burns(pods, failed), warn_hours, now);
    let mut out: Vec<String> = balance::compact(&report).map(|(line, _)| line).into_iter().collect();
    out.extend(balance::warn_lines(&report));
    out.extend(bad_config);
    out
}

/// The lines `teardown --check` prints after its checklist (informational: its verdict and
/// exit don't depend on them). No fleet costs here — the check reads no pod details — so
/// they show what's left and what the provider itself still reports spending.
pub(crate) fn teardown_lines(probes: &[AccountProbe], cfg: &Config, now: u64) -> Vec<String> {
    let (warn_hours, bad_config) = balance::warn_hours_or_default(cfg);
    let mut out = balance::teardown_lines(&balance::report(probes, &Burns::unknown(), warn_hours, now));
    out.extend(bad_config);
    out
}

/// `pods list`'s listing, as `Provider::list_pods` on the fleet provider gives it — every
/// provider's pods, a partial failure one warning line, only a total failure an error (same
/// words) — plus the providers that failed, so the balance footer can call their fleet cost
/// unknown instead of zero. (`list_pods` itself swallows which ones failed.)
pub(crate) async fn list_noting_failures(provider: &dyn Provider) -> Result<(Vec<Pod>, Vec<String>)> {
    let mut pods = Vec::new();
    let (mut failed, mut errs) = (Vec::new(), Vec::new());
    let mut any_ok = false;
    for (name, listed) in provider.list_by_provider().await {
        match listed {
            Ok(p) => {
                any_ok = true;
                pods.extend(p);
            }
            Err(e) => {
                errs.push(format!("{name}: {e}"));
                failed.push(name);
            }
        }
    }
    if !any_ok && !errs.is_empty() {
        anyhow::bail!("all providers failed to list: {}", errs.join("; "));
    }
    if !errs.is_empty() {
        eprintln!("warning: some providers failed to list ({})", errs.join("; "));
    }
    Ok((pods, failed))
}

/// Every provider's pods (each listing bounded) with their details filled where the
/// provider can (RunPod's $/h comes from the details query; best-effort, bounded), and the
/// providers whose listing failed — their burn is unknown, never zero.
async fn fleet_pods(provider: &dyn Provider) -> (Vec<Pod>, Vec<String>, Option<String>) {
    let mut pods = Vec::new();
    let mut failed = Vec::new();
    for (name, listed) in provider.list_by_provider().await {
        match listed {
            Ok(p) => pods.extend(p),
            Err(_) => failed.push(name),
        }
    }
    let note = super::enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await;
    (pods, failed, note)
}

/// `arena balance [--json]`: read every configured account and the fleet at once, print
/// the table (or JSON). Exits non-zero when any account needs a top-up (⚠) or couldn't be
/// read — so `arena balance || <alert>` works from cron.
pub(crate) async fn handle_balance(provider: &dyn Provider, cfg: &Config, json: bool) -> Result<()> {
    let warn_hours = balance::warn_hours(cfg).map_err(anyhow::Error::msg)?;
    let (probes, (pods, failed, note)) = tokio::join!(balance::fetch_all(cfg, balance::FETCH_TIMEOUT), fleet_pods(provider));
    if let Some(n) = note {
        eprintln!("{n}");
    }
    if !failed.is_empty() {
        eprintln!("warning: {} failed to list — their fleet cost is unknown", failed.join(", "));
    }
    let report = balance::report(&probes, &balance::burns(&pods, &failed), warn_hours, arena_core::snapshot::unix_now());
    print!("{}", balance_output(&report, json)?);
    balance_exit(&report)
}

/// What `arena balance` prints. Pure.
pub(crate) fn balance_output(report: &balance::Report, json: bool) -> Result<String> {
    Ok(if json { format!("{}\n", serde_json::to_string_pretty(report)?) } else { balance::render(report) })
}

/// `Ok` unless an account needs a top-up or couldn't be read.
fn balance_exit(report: &balance::Report) -> Result<()> {
    match (report.warnings, report.unknown) {
        (0, 0) => Ok(()),
        (w, u) => anyhow::bail!("balance: {w} account(s) below BALANCE_WARN_HOURS or out of funds, {u} couldn't be read"),
    }
}

/// One pod's probe call, read: the reply, or why there is none. A call that failed outright
/// (no marker in its output: ssh couldn't connect) reads as its exit code and last stderr line.
fn read_probe(call: PodCall) -> Probe {
    let out = match call {
        Ok(out) => out,
        Err(why) => return Probe::Failed(why),
    };
    if !out.success && !out.stdout.contains("ARENA_IDLE_") {
        let stderr = arena_core::ssh::strip_interactive_noise(&out.stderr);
        let last = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("(no output)");
        let code = out.code.map_or_else(|| "killed".to_string(), |c| c.to_string());
        return Probe::Failed(format!("exit {code}: {}", arena_core::jobs::sanitize(last)));
    }
    match idle::parse_reply(&out.stdout) {
        Ok(r) => Probe::Read(r),
        Err(e) => Probe::Failed(e),
    }
}

/// `pods idle`: probe every selected billing pod with an SSH endpoint at once (each within
/// [`IDLE_TIMEOUT`]) while the provider's details fill in the $/h, then print the report —
/// the candidates' commands are printed, never run. Read-only: the only remote command is
/// the probe, and nothing is stopped, terminated or written anywhere.
pub(crate) async fn handle_idle(
    provider: &dyn Provider,
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    hours: f64,
    json: bool,
    say: Say<'_>,
) -> Result<()> {
    let cmd = idle::probe_command(&arena_core::backup::repo_path(cfg));
    let mut probes: Vec<Probe> = Vec::with_capacity(sel.pods.len());
    let mut work = Vec::new();
    for (i, pod) in sel.pods.iter().enumerate() {
        if !bills_hourly(&pod.provider, &pod.status) {
            probes.push(Probe::NotBilling);
            continue;
        }
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => {
                probes.push(Probe::Failed("no result".into()));
                work.push((i, t, cmd.clone()));
            }
            Err(_) => probes.push(Probe::NoEndpoint),
        }
    }
    let mut pods = sel.pods.clone();
    let probing = exec_each_pod(&remote, work, IDLE_TIMEOUT, |_, _, &i, call| probes[i] = read_probe(call));
    let (_, note) = tokio::join!(probing, super::enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT));
    if let Some(n) = note {
        eprintln!("{n}");
    }
    let report = idle::report(&pods, &probes, hours, &arena_core::selector::Naming::from_config(cfg));
    if json {
        say(&serde_json::to_string_pretty(&report)?);
    } else {
        idle::render(&report).lines().for_each(|l| say(l));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{handle_pods, Cli, Cmd, PodCmd, Select};
    use arena_core::balance::Account;
    use arena_core::idle::fixture::Reply;
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use arena_core::PodSpec;
    use async_trait::async_trait;
    use clap::Parser;

    /// A fleet whose listing is scripted per backend. Every mutating call panics: nothing
    /// here may make one.
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
            self.backends.iter().map(|(n, r)| (n.to_string(), r.clone().map_err(arena_core::Error::provider))).collect()
        }
        async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
            panic!("money commands must not create")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("money commands must not stop")
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("money commands must not restart")
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("money commands must not terminate")
        }
    }

    fn pod(name: &str, status: &str, cost: f64, port: u16) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: status.into(),
            cost_per_hr: Some(cost),
            gpu_count: Some(1),
            ssh_ip: Some("203.0.113.7".into()),
            ssh_port: Some(port),
            ..Default::default()
        }
    }

    /// No provider keys: nothing here can reach the network.
    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud dune)\nBACKUP_REPO_PATH=/root/ARENA_materials\n")
    }

    const NOW: u64 = 1_791_460_800;

    #[test]
    fn hours_flag_parses_and_defaults_to_six() {
        assert_eq!(parse_hours("6"), Ok(6.0));
        assert_eq!(parse_hours("1.5h"), Ok(1.5));
        for bad in ["0", "-2", "abc", "9000", "inf"] {
            assert!(parse_hours(bad).is_err(), "{bad}");
        }
        let parse = |argv: &[&str]| Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd);
        match parse(&["pods", "idle"]).unwrap() {
            Cmd::Pods(PodCmd::Idle { hours, json: false, sel }) => {
                assert_eq!(hours, idle::DEFAULT_HOURS);
                assert!(sel.targets.is_empty());
            }
            _ => panic!("wrong parse"),
        }
        match parse(&["pods", "idle", "apple..cloud", "--hours", "12", "--json", "--exclude", "bloom"]).unwrap() {
            Cmd::Pods(PodCmd::Idle { hours, json: true, sel }) => {
                assert_eq!(hours, 12.0);
                assert_eq!(sel.targets, ["apple..cloud"]);
                assert_eq!(sel.opts.exclude, ["bloom"]);
            }
            _ => panic!("wrong parse"),
        }
        assert!(parse(&["pods", "idle", "--hours", "0"]).is_err());
        assert!(matches!(parse(&["balance"]).unwrap(), Cmd::Balance { json: false }));
        assert!(matches!(parse(&["balance", "--json"]).unwrap(), Cmd::Balance { json: true }));
    }

    /// The scenario end to end through `pods idle`'s dispatch: an idle pod, one with a VS Code
    /// session, one that never answers, a stopped one. Only the probe runs (once per billing
    /// pod with an endpoint), the stopped pod is never dialed, and the printed commands are
    /// real command lines — none of which ran.
    #[tokio::test(start_paused = true)]
    async fn pods_idle_reports_and_never_acts() {
        let pods = vec![
            pod("devtest-apple", "RUNNING", 0.25, 22001),
            pod("devtest-bloom", "RUNNING", 0.40, 22002),
            pod("devtest-cloud", "RUNNING", 0.30, 22003),
            pod("devtest-dune", "EXITED", 0.20, 22004),
        ];
        let fleet = Fleet { backends: vec![("runpod", Ok(pods))] };
        let fake = Arc::new(FakeRemote::new());
        fake.script("203.0.113.7:22001", [FakeReply::stdout(&Reply::default().render())]);
        fake.script("203.0.113.7:22002", [FakeReply::stdout(&Reply { sessions: vec![[198, 51, 100, 7]], ..Default::default() }.render())]);
        fake.script("203.0.113.7:22003", [FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let cmd = PodCmd::Idle { hours: 6.0, json: false, sel: Select::default() };
        handle_pods(cmd, &fleet, fake.clone(), &cfg(), true).await.unwrap();
        assert_eq!(start.elapsed(), IDLE_TIMEOUT, "the wedged pod costs exactly its budget");
        let calls = fake.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        let probe = idle::probe_command("/root/ARENA_materials");
        for c in &calls {
            assert!(matches!(c, RemoteCall::Exec { cmd, timeout, .. } if *cmd == probe && *timeout == Some(IDLE_TIMEOUT)), "{c:?}");
        }
        assert!(fake.calls_to("203.0.113.7:22004").is_empty(), "a stopped pod is never dialed");

        // The report itself, through the same reading path.
        let sel = Selected::all(fleet.list_pods().await.unwrap());
        let fake = Arc::new(FakeRemote::new());
        fake.script("203.0.113.7:22001", [FakeReply::stdout(&Reply::default().render())]);
        fake.script("203.0.113.7:22002", [FakeReply::stdout(&Reply { sessions: vec![[198, 51, 100, 7]], ..Default::default() }.render())]);
        fake.script("203.0.113.7:22003", [FakeReply::exit(255, "ssh: connect to host 203.0.113.7 port 22003: Connection refused")]);
        let mut lines = Vec::new();
        handle_idle(&fleet, fake.clone(), &cfg(), &sel, 6.0, false, &mut |l| lines.push(l.to_string())).await.unwrap();
        let text = lines.join("\n");
        for needle in [
            "devtest-apple  runpod    $0.25    0      0    0%     0.0G           9h   10h  3d  IDLE 9h",
            "devtest-bloom  runpod    $0.40    1      0    0%     0.0G           9h   10h  3d  in use: 1 SSH session",
            "devtest-cloud  runpod    $0.30    ?      ?     ?        ?            ?     ?   ?  ? exit 255: ssh: connect to host",
            "devtest-dune   runpod        -    -      -     -        -            -     -   -  - not billing (EXITED)",
            "    arena pods backup devtest-apple",
            "    arena pods terminate devtest-apple",
            "Nothing was changed.",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
        // Each suggested command is a real command line (and would need its own confirm).
        let suggested: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| l.starts_with("arena pods ")).collect();
        assert_eq!(suggested, ["arena pods backup devtest-apple", "arena pods terminate devtest-apple"]);
        for c in suggested {
            Cli::try_parse_from(c.split_whitespace()).unwrap_or_else(|e| panic!("{c}: {e}"));
        }
        // --json: the same verdicts, machine-readable.
        let fake = Arc::new(FakeRemote::new());
        fake.script("203.0.113.7:22001", [FakeReply::stdout(&Reply::default().render())]);
        let mut out = String::new();
        let one = Selected::all(sel.pods[..1].to_vec());
        handle_idle(&fleet, fake, &cfg(), &one, 6.0, true, &mut |l| out.push_str(l)).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["candidates"], serde_json::json!(["devtest-apple"]));
        assert_eq!(v["pods"][0]["commands"][1], "arena pods terminate devtest-apple");
    }

    /// A probe reply that isn't one: the failed call's exit code and last stderr line, or the
    /// parser's reason — never a reading.
    #[test]
    fn a_probe_that_did_not_answer_is_a_failure_not_a_reading() {
        use arena_core::ssh::SshOutput;
        let out = |success: bool, code: i32, stdout: &str, stderr: &str| {
            Ok(SshOutput { success, code: Some(code), stdout: stdout.into(), stderr: stderr.into() })
        };
        assert_eq!(read_probe(Err("timed out after 1m".into())), Probe::Failed("timed out after 1m".into()));
        assert_eq!(
            read_probe(out(false, 255, "", "Warning: x\nroot@1.2.3.4: Permission denied (publickey).\n")),
            Probe::Failed("exit 255: root@1.2.3.4: Permission denied (publickey).".into())
        );
        assert_eq!(read_probe(out(true, 0, "hello\n", "")), Probe::Failed("no reply from the probe".into()));
        // A non-zero exit that still printed the reply (a failing `sed` at the end): read it.
        assert!(matches!(read_probe(out(false, 1, &Reply::default().render(), "")), Probe::Read(_)));
    }

    #[test]
    fn the_list_footer_is_one_line_plus_any_warning() {
        let probes = vec![
            AccountProbe {
                provider: "runpod".into(),
                account: Ok(Account::Prepaid {
                    balance: 10.0,
                    provider_per_hr: Some(0.0),
                    spend_limit_per_hr: None,
                    under_balance: None,
                    owed: None,
                }),
            },
            AccountProbe { provider: "vast".into(), account: Err("vast balance HTTP 429 Too Many Requests: {}".into()) },
        ];
        let pods = vec![pod("devtest-apple", "RUNNING", 0.5, 1), pod("devtest-bloom", "RUNNING", 0.5, 2)];
        assert_eq!(
            list_footer(&probes, &pods, &[], &cfg(), NOW),
            [
                "balance: ⚠ runpod $10.00 ~10h · vast ?",
                "⚠ runpod: $10.00 left at $1.00/h — runs out in ~10h (≈ 2026-10-08 22:00 UTC), under BALANCE_WARN_HOURS=48 — top up (or set up auto-pay) before it stops every pod",
            ]
        );
        // A threshold of 5h: no warning; a bad value: the default, said once.
        let lines = list_footer(&probes, &pods, &[], &Config::parse("BALANCE_WARN_HOURS=5"), NOW);
        assert_eq!(lines, ["balance: runpod $10.00 ~10h · vast ?"]);
        let lines = list_footer(&probes, &pods, &[], &Config::parse("BALANCE_WARN_HOURS=soon"), NOW);
        assert_eq!(lines.last().unwrap(), "warning: BALANCE_WARN_HOURS must be a number of hours ≥ 0 (got `soon`) — using 48");
        assert!(list_footer(&[], &pods, &[], &cfg(), NOW).is_empty(), "no keys, no line");
    }

    /// `pods list`'s footer knows which listings failed: Vast reports no rate of its own, so
    /// with its listing gone its runway is `?` — never "no billing pods" off a missing list.
    #[tokio::test]
    async fn a_failed_listing_makes_the_footer_runway_unknown_not_idle() {
        let vast = || AccountProbe {
            provider: "vast".into(),
            account: Ok(Account::Prepaid { balance: 40.0, provider_per_hr: None, spend_limit_per_hr: None, under_balance: None, owed: None }),
        };
        let fleet = Fleet {
            backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])), ("vast", Err("list pods HTTP 429"))],
        };
        let (pods, failed) = list_noting_failures(&fleet).await.unwrap();
        assert_eq!((pods.len(), failed.as_slice()), (1, &["vast".to_string()][..]));
        assert_eq!(list_footer(&[vast()], &pods, &failed, &cfg(), NOW), ["balance: vast $40.00 ?"]);
        // The same pods with Vast listed fine (no pods there): nothing billing, said so.
        assert_eq!(list_footer(&[vast()], &pods, &[], &cfg(), NOW), ["balance: vast $40.00 no billing pods"]);
        // Every listing failed: the list fails, as `list_pods` would.
        let down = Fleet { backends: vec![("runpod", Err("HTTP 500")), ("vast", Err("HTTP 429"))] };
        let e = list_noting_failures(&down).await.unwrap_err().to_string();
        assert!(e.starts_with("all providers failed to list: runpod: ") && e.contains("vast: "), "{e}");
    }

    #[test]
    fn balance_output_and_exit_follow_the_report() {
        let ok = vec![AccountProbe { provider: "hetzner".into(), account: Ok(Account::Postpaid) }];
        let r = balance::report(&ok, &balance::burns(&[], &[]), 48.0, NOW);
        assert!(balance_exit(&r).is_ok());
        assert!(balance_output(&r, false).unwrap().contains("hetzner   postpaid"));
        let v: serde_json::Value = serde_json::from_str(&balance_output(&r, true).unwrap()).unwrap();
        assert_eq!(v["rows"][0]["billing"], "postpaid");
        let bad = vec![AccountProbe { provider: "runpod".into(), account: Err("runpod balance HTTP 401".into()) }];
        let r = balance::report(&bad, &balance::burns(&[], &[]), 48.0, NOW);
        assert_eq!(
            balance_exit(&r).unwrap_err().to_string(),
            "balance: 0 account(s) below BALANCE_WARN_HOURS or out of funds, 1 couldn't be read"
        );
        assert_eq!(teardown_lines(&bad, &cfg(), NOW)[1], "  ? runpod: balance unavailable — runpod balance HTTP 401");
    }

    /// `arena balance` reads the fleet per provider: a failed listing makes that provider's
    /// fleet cost unknown (`?`), never zero — and nothing is mutated.
    #[tokio::test]
    async fn the_fleet_cost_comes_from_every_listing_that_answered() {
        let fleet = Fleet {
            backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])), ("vast", Err("list pods HTTP 429"))],
        };
        let (pods, failed, note) = fleet_pods(&fleet).await;
        assert_eq!((pods.len(), failed.as_slice(), note), (1, &["vast".to_string()][..], None));
        let probes = vec![
            AccountProbe {
                provider: "vast".into(),
                account: Ok(Account::Prepaid { balance: 100.0, provider_per_hr: None, spend_limit_per_hr: None, under_balance: None, owed: None }),
            },
        ];
        let r = balance::report(&probes, &balance::burns(&pods, &failed), 48.0, NOW);
        let text = balance_output(&r, false).unwrap();
        assert!(text.contains("vast      $100.00              -      ?  ?"), "{text}");
    }
}
