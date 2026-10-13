//! Money: `arena balance` (what's left on each prepaid account, how long it lasts and where
//! the number comes from), `balance set`, the balance line under `pods list`, the
//! `create`/`up` preflight, the `config check` rows — and `pods idle` (which pods look
//! abandoned — a report, never an action).
//!
//! Judging and rendering live in `arena_core::balance` / `arena_core::idle` (pure, tested on
//! fixture-shaped replies). Here is only the reading: the balance reads (behind
//! [`BalanceApi`]), the listings, each bounded, and one bounded read-only SSH probe per pod.
//! Every balance surface but `arena balance` itself is **quiet**: a failing source changes a
//! label, never stderr or an exit code.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::balance::{self, Account, BalanceApi, BalancePolicy, Fetched, HttpBalanceApi, Mode};
use arena_core::idle::{self, Probe};
use arena_core::placement::OptionPlan;
use arena_core::proxy::Listing;
use arena_core::remote::Remote;
use arena_core::snapshot::unix_now;
use arena_core::ssh::SshTarget;
use arena_core::status::bills_hourly;
use arena_core::{Config, Pod, Provider};

use super::jobs::Say;
use super::{exec_each_pod, PodCall, Selected, ENRICH_TIMEOUT};

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

/// What the balance surfaces read with: the reads (`HttpBalanceApi` for real, a fake in
/// tests) and where the state file lives (`None`: no state directory — live readings only;
/// `Err`: why there is none).
pub(crate) struct BalanceCtx {
    pub api: Arc<dyn BalanceApi>,
    pub path: Option<std::result::Result<PathBuf, String>>,
}

impl BalanceCtx {
    /// The real reads and this fleet's state file — except in this crate's unit tests, where
    /// no account is configured and there is no state file unless the test's config sets
    /// `ARENA_STATE_DIR`: a stray key in a test config must never reach the network, and a
    /// test never touches the developer's `~/.local/state`. Tests that want balances hand
    /// in a fake.
    pub(crate) fn from_config(cfg: &Config) -> Self {
        let api_cfg = if cfg!(test) { Config::parse("") } else { cfg.clone() };
        let path = (!cfg!(test) || cfg.get("ARENA_STATE_DIR").is_some())
            .then(|| arena_core::snapshot::state_file_path_for(cfg, arena_core::snapshot::BALANCE_FILE));
        BalanceCtx { api: Arc::new(HttpBalanceApi::from_config(&api_cfg)), path }
    }

    /// The state file, when there is one.
    pub(crate) fn state_path(&self) -> Option<&Path> {
        self.path.as_ref().and_then(|p| p.as_ref().ok()).map(PathBuf::as_path)
    }

    /// How an inline surface observes: live (at most once per FRESH) only when the state
    /// can be saved — nothing else remembers the last attempt across these short-lived
    /// commands, so without it every `pods list` would read live again. Otherwise the
    /// cache alone, quietly (`arena balance` says why).
    pub(crate) fn inline_mode(&self) -> Mode {
        match self.state_path() {
            Some(p) if balance::writable(p) => Mode::Inline,
            _ => Mode::CacheOnly,
        }
    }

    /// Remember the per-GPU prices a live catalog or offer search gave a placement plan
    /// (best effort, quiet): a known GPU's balance stand-in is never below them.
    pub(crate) fn remember_gpu_prices(&self, plan: &OptionPlan) {
        let live: Vec<(String, f64)> = plan
            .options
            .iter()
            .filter(|o| o.price_source == Some(arena_core::placement::PriceSource::Live))
            .filter_map(|o| o.price_per_gpu.map(|p| (o.gpu.clone(), p)))
            .collect();
        if let (Some(p), false) = (self.state_path(), live.is_empty()) {
            let _ = balance::remember_gpu_prices(p, &live);
        }
    }

    /// Remember the prices of pods just made (best effort, quiet).
    pub(crate) fn remember_prices<'a>(&self, pods: impl IntoIterator<Item = &'a Pod>) {
        let pods: Vec<Pod> = pods.into_iter().cloned().collect();
        if let Some(p) = self.state_path() {
            let _ = balance::remember_prices(p, &self.api.accounts(), &pods, unix_now());
        }
    }
}

/// The flags of a plain `arena balance`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BalanceFlags {
    pub json: bool,
    pub check: bool,
    pub no_live: bool,
    pub verbose: bool,
}

/// What a command prints: stdout, the stderr lines, and the exit code.
#[derive(Debug)]
pub(crate) struct Printed {
    pub stdout: String,
    pub stderr: Vec<String>,
    pub code: i32,
}

/// `arena balance [--json] [--check] [--no-live] [-v]`: list the fleet (bounded, no
/// details) while observing every configured account (live always; billing when there is an
/// anchor; `--no-live`: billing only — the estimate tier forced, nothing written), record
/// it, then print the table and notes (or JSON). Exit 0 — or with `--check` 2 critical, 3
/// unknown while burning, 1 low. A provider that fails to list makes its burn unknown;
/// it never fails the command.
pub(crate) async fn balance_report(
    provider: &dyn Provider,
    cfg: &Config,
    bal: &BalanceCtx,
    flags: BalanceFlags,
    now: u64,
) -> Result<Printed> {
    let (policy, policy_warning) = BalancePolicy::from_config(cfg);
    let mut stderr: Vec<String> = Vec::new();
    if let Some(Err(e)) = &bal.path {
        stderr.push(format!("warning: no balance state ({e}) — live readings only"));
    }
    let path = bal.state_path();
    if let Some(p) = path.filter(|p| !balance::writable(p)) {
        stderr.push(format!(
            "warning: the balance state {} can't be saved — live readings only here, and the footers and preflight \
             read no live balance until it can",
            p.display()
        ));
    }
    let (state, load_warning) = balance::load_opt(path);
    let mode = if flags.no_live { Mode::NoLive } else { Mode::Full };
    let (listed, fetched) =
        tokio::join!(provider.list_by_provider(), balance::observe(bal.api.as_ref(), &state, mode, balance::COMMAND_TIMEOUT, now));
    let listing = Listing::from_results(listed);
    for (p, e) in listing.errors() {
        stderr.push(format!("warning: {p} failed to list ({e}) — its burn is unknown"));
    }
    let pods = listing.pods();
    let burns = balance::burns_with(&listing, &pods, &state, &policy);
    let (state, save_warning) = balance::commit(path, state, &fetched, &burns, mode, now);
    let mut views = balance::views(&state, &fetched, &burns, &policy, mode, now);
    views.extend(balance::postpaid_view(&listing, &pods));
    stderr.extend([policy_warning, load_warning, save_warning].into_iter().flatten());
    let stdout = if flags.json {
        format!("{}\n", serde_json::to_string_pretty(&views)?)
    } else {
        let mut out = balance::render_table(&views, now);
        if flags.no_live {
            out.push_str("  --no-live: no live call — this is what the fallback estimate says (nothing was written)\n");
        }
        for n in balance::notes(&views, &policy, now) {
            out.push_str(&format!("  {n}\n"));
        }
        if flags.verbose {
            for l in balance::verbose_lines(&views, &state, &fetched, path, now) {
                out.push_str(&format!("{l}\n"));
            }
        }
        out
    };
    let code = if flags.check { balance::check_exit(&views) } else { 0 };
    Ok(Printed { stdout, stderr, code })
}

/// `arena balance set <runpod|vast> <USD>`: record the balance read off the provider's
/// console as a manual anchor — the escape hatch when every API is dead. A local write (no
/// confirm), scoped by the configured key's fingerprint; the next live reading replaces it.
pub(crate) fn balance_set(cfg: &Config, bal: &BalanceCtx, account: Account, usd: f64, now: u64) -> Result<String> {
    let Some((_, fp)) = bal.api.accounts().into_iter().find(|(a, _)| *a == account) else {
        anyhow::bail!(
            "{} isn't set — `balance set {}` records the balance of the account that key reaches (its fingerprint \
             scopes the value)",
            account.key_name(),
            account.name()
        );
    };
    let path = match &bal.path {
        Some(Ok(p)) => p.clone(),
        Some(Err(e)) => anyhow::bail!("nowhere to record it: {e}"),
        None => anyhow::bail!("nowhere to record it: no state directory"),
    };
    let (old, state, _) = balance::record_manual(&path, account, &fp, usd, now)
        .map_err(|e| anyhow::anyhow!("couldn't write {}: {e}", path.display()))?;
    let (policy, _) = BalancePolicy::from_config(cfg);
    let mut out = String::new();
    for l in balance::set_lines(old, &state, account, &fp, &policy, now) {
        out.push_str(&l);
        out.push('\n');
    }
    Ok(out)
}

/// `config check`'s balance rows: the threshold and how each account's sources stand —
/// from the state file alone. `api` only says which accounts are configured: no request.
pub(crate) fn balance_config_rows(cfg: &Config, bal: &BalanceCtx, now: u64) -> Vec<String> {
    let (state, _) = balance::load_opt(bal.state_path());
    balance::config_rows(cfg, &state, &bal.api.accounts(), now)
}

/// The views for a surface that already listed (`pods list`, teardown): record what was
/// observed against the listing's burns and judge it. Quiet: a write that fails is dropped
/// (only `arena balance` says so).
#[allow(clippy::too_many_arguments)]
pub(crate) fn listed_views(
    bal: &BalanceCtx,
    state: balance::BalanceState,
    fetched: &[Fetched],
    listing: &Listing,
    pods: &[Pod],
    policy: &BalancePolicy,
    mode: Mode,
    now: u64,
) -> Vec<balance::AccountView> {
    let burns = balance::burns_with(listing, pods, &state, policy);
    let (state, _) = balance::commit(bal.state_path(), state, fetched, &burns, mode, now);
    balance::views(&state, fetched, &burns, policy, mode, now)
}

/// The `create`/`up` preflight: the target provider's account only, observed inline (no
/// extra listing: the burn is the provider's or the last recorded sample), with the new
/// pods priced at the dearest placement option — an option nobody priced, or the single
/// spec's GPU, at a stand-in that errs high (`~`), never free. Advisory and quiet: lines to
/// print, never an error.
pub(crate) async fn balance_preflight(
    provider: &dyn Provider,
    cfg: &Config,
    bal: &BalanceCtx,
    new_pods: usize,
    options: Option<&OptionPlan>,
    spec: &arena_core::PodSpec,
) -> Vec<String> {
    let Some(account) = Account::from_name(provider.name()) else { return Vec::new() };
    let now = unix_now();
    let mode = bal.inline_mode();
    let (state, _) = balance::load_opt(bal.state_path());
    let Some(fetched) = balance::observe_account(bal.api.as_ref(), &state, account, mode, balance::INLINE_TIMEOUT, now).await else {
        return Vec::new();
    };
    let fetched = [fetched];
    let burns = balance::Burns::new();
    let (state, _) = balance::commit(bal.state_path(), state, &fetched, &burns, mode, now);
    let (policy, _) = BalancePolicy::from_config(cfg);
    let views = balance::views(&state, &fetched, &burns, &policy, mode, now);
    let new = new_pod_price(new_pods, options, spec, &policy, &state.gpu_seen);
    views.first().map(|v| balance::preflight_lines_with(v, new, &policy, now)).unwrap_or_default()
}

/// What the new pods cost per pod at most: the dearest placement option — a live catalog
/// price as it is; a preset estimate, or no price, at least the known GPU's stand-in (the
/// most it is known to cost: RunPod's recorded catalog, live prices seen), an unknown GPU
/// at the ceiling — and on the single-spec path the spec's GPU at that stand-in. `per_pod:
/// None` only when not even a stand-in can be named.
fn new_pod_price(
    count: usize,
    options: Option<&OptionPlan>,
    spec: &arena_core::PodSpec,
    policy: &BalancePolicy,
    seen: &std::collections::BTreeMap<String, f64>,
) -> balance::NewPods {
    use arena_core::placement::PriceSource;
    let (mut guessed, mut ceiling) = (false, false);
    let mut stand_in = |gpu: &str, n: u32, floor: Option<f64>| {
        let found = balance::stand_in_with(Some(gpu), Some(n), policy, seen);
        match (found, floor) {
            (Some((p, how)), f) if f.is_none_or(|f| p > f) => {
                guessed = true;
                ceiling |= how == balance::StandIn::Ceiling;
                Some(p)
            }
            (_, f) => f,
        }
    };
    let per_pod = match options {
        Some(plan) if !plan.options.is_empty() => plan
            .options
            .iter()
            .map(|o| match (o.price_per_pod, o.price_source) {
                (Some(p), Some(PriceSource::Live)) => Some(p),
                (p, _) => stand_in(&o.gpu, o.gpu_count.max(1), p),
            })
            .collect::<Option<Vec<f64>>>()
            .map(|prices| prices.into_iter().fold(0.0, f64::max)),
        _ => stand_in(&spec.gpu_type, spec.gpu_count.max(1), None),
    };
    let known = per_pod.is_some();
    balance::NewPods { count, per_pod, stand_in: guessed && known, ceiling: ceiling && known }
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
    use crate::{handle_pods, BalanceCmd, Cli, Cmd, PodCmd, Select};
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

    // ---- `arena balance`, `balance set`, `config check` (a FakeBalanceApi; no network) ----

    use arena_core::balance::{FakeBalanceApi, Fail, FailKind, Live};
    use arena_core::snapshot::BALANCE_FILE;

    const T0: u64 = 1_791_460_800;

    fn rp_live(usd: f64, rate: f64) -> Live {
        Live { usd, provider_burn_per_hr: Some(rate), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None }
    }

    fn vast_live(usd: f64) -> Live {
        Live { usd, provider_burn_per_hr: None, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    /// A temp state directory (removed at the end), with this fleet's balance file in it.
    struct StateDir(PathBuf);
    impl Drop for StateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl StateDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("arena-money-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            StateDir(d)
        }
        fn file(&self) -> PathBuf {
            self.0.join("devtest").join(BALANCE_FILE)
        }
        fn cfg(&self, extra: &str) -> Config {
            Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\n{extra}", self.0.display()))
        }
    }

    fn ctx(api: FakeBalanceApi, dir: Option<&StateDir>) -> (Arc<FakeBalanceApi>, BalanceCtx) {
        let api = Arc::new(api);
        (api.clone(), BalanceCtx { api, path: dir.map(|d| Ok(d.file())) })
    }

    fn mixed_fleet() -> Fleet {
        Fleet {
            backends: vec![
                ("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])),
                ("vast", Err("list instances HTTP 429")),
                ("hetzner", Ok(vec![Pod { provider: "hetzner".into(), status: "running".into(), cost_per_hr: Some(0.006), ..Default::default() }])),
            ],
        }
    }

    #[test]
    fn balance_flags_and_set_parse() {
        let parse = |argv: &[&str]| Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd);
        assert!(matches!(
            parse(&["balance"]).unwrap(),
            Cmd::Balance { cmd: None, json: false, check: false, no_live: false, verbose: false }
        ));
        assert!(matches!(parse(&["balance", "--check", "--json"]).unwrap(), Cmd::Balance { cmd: None, json: true, check: true, .. }));
        assert!(matches!(parse(&["balance", "--no-live", "-v"]).unwrap(), Cmd::Balance { no_live: true, verbose: true, .. }));
        match parse(&["balance", "set", "runpod", "12.5"]).unwrap() {
            Cmd::Balance { cmd: Some(BalanceCmd::Set { account, usd }), .. } => assert_eq!((account, usd), (Account::Runpod, 12.5)),
            _ => panic!("wrong parse"),
        }
        match parse(&["balance", "set", "vast", "-3"]).unwrap() {
            Cmd::Balance { cmd: Some(BalanceCmd::Set { account, usd }), .. } => assert_eq!((account, usd), (Account::Vast, -3.0)),
            _ => panic!("wrong parse"),
        }
        for bad in [
            &["balance", "set", "hetzner", "5"][..],
            &["balance", "set", "runpod", "NaN"],
            &["balance", "set", "runpod", "inf"],
            &["balance", "set", "runpod", "2e7"],
            &["balance", "set", "runpod"],
            &["balance", "--json", "set", "runpod", "5"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn balance_table_and_json() {
        let dir = StateDir::new("table");
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(32.54, 0.47)).live_ok(Account::Vast, vast_live(136.87));
        let (api, bal) = ctx(api, Some(&dir));
        let out = balance_report(&mixed_fleet(), &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        assert_eq!(out.code, 0, "a plain `arena balance` exits 0 whatever it found");
        assert_eq!(out.stderr, ["warning: vast failed to list (provider error: list instances HTTP 429) — its burn is unknown"]);
        for needle in [
            "ACCOUNT   BALANCE  SOURCE        BURN  RUNWAY  RUNS OUT (UTC)    LEVEL",
            "runpod     $32.54  live       $0.47/h  ≈2.8d   2026-10-11 09:14  ⚠ low",
            // Vast's listing failed: its burn is unknown, so it can't be judged (B2-2).
            "vast      $136.87  live             ?  ?       -                 ? unknown",
            "hetzner  postpaid  postpaid  €0.006/h  -       -                 -",
            "  ⚠ runpod: ≈2.8d of runway at $0.47/h (≈ 2026-10-11 09:14 UTC), under BALANCE_WARN_HOURS=72",
            "  runpod: RunPod reports $0.22/h more than arena lists",
            "  hetzner: postpaid",
        ] {
            assert!(out.stdout.contains(needle), "`{needle}` missing:\n{}", out.stdout);
        }
        assert_eq!(api.live_calls(), 2);
        // Recorded: the anchors are there for the fallback.
        let (state, _) = arena_core::balance::load(&dir.file());
        assert_eq!(state.accounts.len(), 2);
        // -v: the parts behind the numbers.
        let v = balance_report(&mixed_fleet(), &dir.cfg(""), &bal, BalanceFlags { verbose: true, ..Default::default() }, T0 + 600)
            .await
            .unwrap();
        for needle in ["  burn: provider $0.47/h · listed $0.25/h — gap $0.22/h not in the listing", "  spendLimit $80.00/h · underBalance false", "state file: ", "  postpaid €0.006/h — servers only: volumes not counted"] {
            assert!(v.stdout.contains(needle), "`{needle}` missing:\n{}", v.stdout);
        }
        // --json: one object per account, the source always there.
        let j = balance_report(&mixed_fleet(), &dir.cfg(""), &bal, BalanceFlags { json: true, ..Default::default() }, T0 + 700)
            .await
            .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0]["account"].as_str(), rows[0]["source"].as_str(), rows[0]["balance"].as_f64()), (Some("runpod"), Some("live"), Some(32.54)));
        assert_eq!((rows[1]["account"].as_str(), rows[1]["burn"].is_null()), (Some("vast"), true), "a failed listing: burn unknown, not $0");
        assert_eq!((rows[2]["source"].as_str(), rows[2]["currency"].as_str()), (Some("postpaid"), Some("EUR")));
        // A bad threshold: the default is used, and said — never a failure.
        let out = balance_report(&mixed_fleet(), &dir.cfg("BALANCE_WARN_HOURS=soon\n"), &bal, BalanceFlags::default(), T0 + 800).await.unwrap();
        assert!(out.stderr.iter().any(|l| l.contains("BALANCE_WARN_HOURS must be a number") && l.ends_with("using 72")), "{:?}", out.stderr);
    }

    #[tokio::test]
    async fn balance_check_exit_codes() {
        let fleet = || Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 1)])), ("vast", Ok(vec![]))] };
        let check = BalanceFlags { check: true, ..Default::default() };
        let gone = || Fail { kind: FailKind::Gone, http: Some(404), detail: None };
        // (case, runpod live, vast live, exit)
        let cases: Vec<(&str, std::result::Result<Live, Fail>, std::result::Result<Live, Fail>, i32)> = vec![
            ("all fine", Ok(rp_live(500.0, 0.5)), Ok(vast_live(10.0)), 0),
            ("low", Ok(rp_live(30.0, 0.5)), Ok(vast_live(10.0)), 1),
            ("critical", Ok(rp_live(5.0, 0.5)), Ok(vast_live(10.0)), 2),
            ("unknown while burning", Err(gone()), Ok(vast_live(10.0)), 3),
            ("unknown but idle (vast)", Ok(rp_live(500.0, 0.5)), Err(gone()), 0),
            ("worst wins: critical over unknown", Ok(rp_live(5.0, 0.5)), Err(gone()), 2),
            ("worst wins: unknown over low", Err(gone()), Ok(Live { under_balance: None, ..vast_live(10.0) }), 3),
        ];
        for (case, rp, vast, want) in cases {
            let mut api = FakeBalanceApi::new();
            api = match rp {
                Ok(l) => api.live_ok(Account::Runpod, l),
                Err(f) => api.live_err(Account::Runpod, f),
            };
            api = match vast {
                Ok(l) => api.live_ok(Account::Vast, l),
                Err(f) => api.live_err(Account::Vast, f),
            };
            let (_, bal) = ctx(api, None);
            let out = balance_report(&fleet(), &cfg(), &bal, check, T0).await.unwrap();
            assert_eq!(out.code, want, "{case}:\n{}", out.stdout);
            // --check combines with --json: same code, JSON out.
            let j = balance_report(&fleet(), &cfg(), &bal, BalanceFlags { json: true, ..check }, T0).await.unwrap();
            assert_eq!(j.code, want, "{case} --json");
            assert!(serde_json::from_str::<serde_json::Value>(&j.stdout).is_ok());
        }
    }

    #[tokio::test]
    async fn balance_set_records_manual() {
        let dir = StateDir::new("set");
        let api = FakeBalanceApi::new().live_err(Account::Runpod, Fail { kind: FailKind::Gone, http: Some(410), detail: None });
        let (_, bal) = ctx(api, Some(&dir));
        let out = balance_set(&dir.cfg(""), &bal, Account::Runpod, 250.0, T0).unwrap();
        assert!(out.starts_with("runpod: no reading → $250.00 (manual)\n"), "{out}");
        let (state, _) = arena_core::balance::load(&dir.file());
        let st = state.accounts.get(&Account::Runpod).unwrap();
        assert_eq!(st.anchor.map(|a| (a.usd, a.at)), Some((250.0, T0)));
        assert_eq!(st.key_fp, FakeBalanceApi::fp(Account::Runpod), "scoped by the configured key");
        // With live down, the estimate counts down from it.
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 1)]))] };
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { no_live: true, ..Default::default() }, T0 + 3_600).await.unwrap();
        assert!(out.stdout.contains("~$249.50") && out.stdout.contains("est. (manual 1h, metered)"), "{}", out.stdout);
        // Live back: it wins, and replaces the manual anchor.
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(31.0, 0.5));
        let (_, live_bal) = ctx(api, Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &live_bal, BalanceFlags::default(), T0 + 7_200).await.unwrap();
        assert!(out.stdout.contains("$31.00  live"), "{}", out.stdout);
        // `set` right after a live reading: recorded, but live is said to win for now.
        let out = balance_set(&dir.cfg(""), &live_bal, Account::Runpod, 40.0, T0 + 7_260).unwrap();
        assert!(out.starts_with("runpod: $31.00 (live, 1m ago) → $40.00 (manual)\n"), "{out}");
        assert!(out.contains("note: live is $31.00; a manual value is used only while live is unavailable"), "{out}");
    }

    #[test]
    fn balance_set_refuses_unconfigured() {
        let dir = StateDir::new("set-refused");
        let (_, bal) = ctx(FakeBalanceApi::new().with(Account::Runpod), Some(&dir));
        let e = balance_set(&dir.cfg(""), &bal, Account::Vast, 5.0, T0).unwrap_err().to_string();
        assert!(e.starts_with("VAST_API_KEY isn't set"), "{e}");
        assert!(!dir.file().exists(), "nothing written");
        let (_, nowhere) = ctx(FakeBalanceApi::new().with(Account::Runpod), None);
        assert!(balance_set(&cfg(), &nowhere, Account::Runpod, 5.0, T0).unwrap_err().to_string().contains("nowhere to record it"));
    }

    /// Invariant 13: `--no-live` makes no live call and writes nothing (nor do the
    /// cache-only rows of `config check`).
    #[tokio::test]
    async fn no_live_writes_nothing() {
        let dir = StateDir::new("nolive");
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 1)]))] };
        // No file yet: still none after.
        let (api, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(31.0, 0.5)), Some(&dir));
        balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { no_live: true, ..Default::default() }, T0).await.unwrap();
        assert!(!dir.file().exists());
        assert_eq!(api.live_calls(), 0);
        // A file: byte for byte the same after.
        balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        let before = std::fs::read(dir.file()).unwrap();
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { no_live: true, ..Default::default() }, T0 + 3_600).await.unwrap();
        assert!(out.stdout.contains("est. (from live 1h, metered)") && out.stdout.contains("--no-live: no live call"), "{}", out.stdout);
        assert_eq!(std::fs::read(dir.file()).unwrap(), before);
        assert_eq!(api.live_calls(), 1, "only the plain run called live");
        let _ = balance_config_rows(&dir.cfg(""), &bal, T0 + 3_600);
        assert_eq!(std::fs::read(dir.file()).unwrap(), before);
    }

    /// `config check` reads the state file only: the fake panics on any call.
    #[tokio::test]
    async fn config_check_rows_from_cache() {
        let dir = StateDir::new("config");
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(31.0, 0.0)), Some(&dir));
        balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        let panicking = BalanceCtx {
            api: Arc::new(FakeBalanceApi::new().with(Account::Runpod).with(Account::Vast).panicking()),
            path: Some(Ok(dir.file())),
        };
        assert_eq!(
            balance_config_rows(&dir.cfg(""), &panicking, T0 + 240),
            [
                "  ✓ BALANCE_WARN_HOURS         72 (default) — warn below 3d of runway",
                "  ✓ BALANCE_UNPRICED_PER_GPU   $8.00 (default) — ceiling per GPU-h for an unpriced pod of an unknown GPU type",
                "  ✓ runpod                     live 4m ago",
                "  · vast                       no reading yet — run `arena balance`",
            ]
        );
    }

    // ---- `pods list`: the balance line, and everything else unchanged ----

    fn no_remote() -> Arc<dyn Remote> {
        Arc::new(FakeRemote::new())
    }

    /// No balance reads at all: what `pods list` printed before this wave.
    fn no_balance() -> BalanceCtx {
        BalanceCtx { api: Arc::new(FakeBalanceApi::new()), path: None }
    }

    /// Every way a balance source fails, as fakes (the stall costs INLINE_TIMEOUT).
    fn failing_apis() -> Vec<(&'static str, FakeBalanceApi)> {
        let f = |kind, http| Fail { kind, http, detail: None };
        vec![
            ("transient", FakeBalanceApi::new().live_err(Account::Runpod, f(FailKind::Transient, Some(503)))),
            ("auth", FakeBalanceApi::new().live_err(Account::Runpod, f(FailKind::Auth, Some(401)))),
            ("gone", FakeBalanceApi::new().live_err(Account::Runpod, f(FailKind::Gone, Some(404)))),
            ("drift", FakeBalanceApi::new().live_err(Account::Vast, f(FailKind::Drift, None))),
            ("stall", FakeBalanceApi::new().live_stall(Account::Runpod).live_stall(Account::Vast)),
        ]
    }

    fn two_providers() -> Fleet {
        Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 1)])), ("vast", Ok(vec![]))] }
    }

    #[tokio::test]
    async fn pods_list_footer_shows_balance() {
        let dir = StateDir::new("list-footer");
        let api = FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(48.0, 0.0)).live_ok(Account::Vast, vast_live(136.87));
        let (api, bal) = ctx(api, Some(&dir));
        let (out, err) = crate::list_with(&two_providers(), &no_remote(), &dir.cfg(""), &bal, false, false).await.unwrap();
        assert!(err.is_empty(), "{err:?}");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[lines.len() - 2].starts_with("fleet: $0.50/h across 1 billing pod(s)"), "{out}");
        assert_eq!(lines.last().unwrap(), &"balance: runpod $48.00 live ≈4.0d · vast $136.87 live no burn", "{out}");
        assert_eq!(api.live_calls(), 2);
        assert!(dir.file().exists(), "recorded for the fallback");
        // `(no pods)` gets the line too.
        let dir = StateDir::new("list-empty");
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(48.0, 0.0)), Some(&dir));
        let empty = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        let (out, _) = crate::list_with(&empty, &no_remote(), &dir.cfg(""), &bal, false, false).await.unwrap();
        assert_eq!(out, "(no pods)\nbalance: runpod $48.00 live no burn\n");
        // No prepaid account configured: no line at all, as before.
        let (out, _) = crate::list_with(&empty, &no_remote(), &cfg(), &no_balance(), false, false).await.unwrap();
        assert_eq!(out, "(no pods)\n");
    }

    /// Invariant 2 for `pods list --json`: byte for byte the same whatever the balance does —
    /// it makes no balance call and writes no state.
    #[tokio::test(start_paused = true)]
    async fn pods_list_json_identical() {
        let (baseline, base_err) = crate::list_with(&two_providers(), &no_remote(), &cfg(), &no_balance(), true, false).await.unwrap();
        let mut apis = failing_apis();
        apis.push(("ok", FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(1.0, 9.0))));
        for (case, api) in apis {
            let dir = StateDir::new(&format!("json-{case}"));
            let (api, bal) = ctx(api, Some(&dir));
            let (out, err) = crate::list_with(&two_providers(), &no_remote(), &dir.cfg(""), &bal, true, false).await.unwrap();
            assert_eq!((&out, &err), (&baseline, &base_err), "{case}");
            assert_eq!((api.live_calls(), api.billed_calls()), (0, 0), "{case}");
            assert!(!dir.file().exists(), "{case}");
        }
        let pods: Vec<Pod> = serde_json::from_str(&baseline).unwrap();
        assert_eq!(pods.len(), 1);
    }

    #[tokio::test]
    async fn pods_list_partial_warning_unchanged() {
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])), ("vast", Err("list pods HTTP 429"))] };
        let want = ["warning: some providers failed to list (vast: provider error: list pods HTTP 429)"];
        let (_, err) = crate::list_with(&fleet, &no_remote(), &cfg(), &no_balance(), false, false).await.unwrap();
        assert_eq!(err, want);
        // With balances: the same one warning — and Vast's burn is unknown, never $0/h.
        let dir = StateDir::new("partial");
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Vast, vast_live(40.0)), Some(&dir));
        let (out, err) = crate::list_with(&fleet, &no_remote(), &cfg(), &bal, false, false).await.unwrap();
        assert_eq!(err, want);
        assert_eq!(out.lines().last().unwrap(), "balance: vast $40.00 live ?", "{out}");
    }

    #[tokio::test]
    async fn pods_list_all_failed_still_errors() {
        let down = Fleet { backends: vec![("runpod", Err("HTTP 500")), ("vast", Err("HTTP 429"))] };
        for bal in [no_balance(), ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(5.0, 1.0)), None).1] {
            let e = crate::list_with(&down, &no_remote(), &cfg(), &bal, false, false).await.unwrap_err().to_string();
            assert!(e.starts_with("all providers failed to list: runpod: ") && e.contains("vast: "), "{e}");
        }
        // Nothing configured at all is no error (and no line).
        let none = Fleet { backends: vec![] };
        assert_eq!(crate::list_with(&none, &no_remote(), &cfg(), &no_balance(), false, false).await.unwrap(), ("(no pods)\n".into(), vec![]));
    }

    /// Invariant 11: a failing balance source never reaches stderr on an inline surface — the
    /// `pods list` warnings are exactly the baseline's, and the preflight only returns lines
    /// to print with the rest of the plan (a label: `?` or `est.`), never an error.
    #[tokio::test]
    async fn inline_surfaces_silent() {
        let fleet = || Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])), ("vast", Err("HTTP 429"))] };
        let (base_out, base_err) = crate::list_with(&fleet(), &no_remote(), &cfg(), &no_balance(), false, false).await.unwrap();
        for (case, api) in failing_apis() {
            let dir = StateDir::new(&format!("silent-{case}"));
            let (_, bal) = ctx(api, Some(&dir));
            let started = std::time::Instant::now();
            let (out, err) = crate::list_with(&fleet(), &no_remote(), &dir.cfg(""), &bal, false, false).await.unwrap();
            assert!(started.elapsed() < balance::INLINE_TIMEOUT + Duration::from_secs(1), "{case}");
            assert_eq!(err, base_err, "{case}: stderr is the listing's, never the balance's");
            let (table, footer) = out.rsplit_once("balance: ").unwrap_or_else(|| panic!("{case}: {out}"));
            assert_eq!(table, base_out, "{case}");
            assert!(footer.contains('?') || footer.contains("est."), "{case}: {footer}");
            let spec = arena_core::PodSpec::from_config(&dir.cfg(""));
            let lines = balance_preflight(&fleet(), &dir.cfg(""), &bal, 1, None, &spec).await;
            assert!(lines.iter().all(|l| !l.to_lowercase().contains("error")), "{case}: {lines:?}");
        }
    }

    /// B2-4 / B2-6: a state that can't be saved (here `balance.json` is a directory: a
    /// probe beside it would pass, the save never can) makes every inline surface read no
    /// live balance at all — each `pods list` is a new process with no memory of the last
    /// attempt — and say nothing; only `arena balance` reads live, and says why it can't keep it.
    #[tokio::test]
    async fn unsavable_state_makes_inline_surfaces_read_no_live_balance() {
        let dir = StateDir::new("unsavable");
        std::fs::create_dir_all(dir.file()).unwrap();
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 1)]))] };
        let (base_out, base_err) = crate::list_with(&fleet, &no_remote(), &dir.cfg(""), &no_balance(), false, false).await.unwrap();
        for _ in 0..3 {
            // A fresh process each time: a fresh reader, no memory of the last attempt.
            let (api, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)), Some(&dir));
            assert_eq!(bal.inline_mode(), Mode::CacheOnly);
            let (out, err) = crate::list_with(&fleet, &no_remote(), &dir.cfg(""), &bal, false, false).await.unwrap();
            assert_eq!(err, base_err, "silent");
            assert!(out.starts_with(&base_out) && out.ends_with("balance: runpod ? (no reading — arena balance set runpod <USD>) $0.50/h\n"), "{out}");
            let spec = arena_core::PodSpec::from_config(&dir.cfg(""));
            balance_preflight(&fleet, &dir.cfg(""), &bal, 1, None, &spec).await;
            assert_eq!(api.live_calls(), 0, "no live call from an inline surface");
        }
        // `arena balance` reads live, and says the state can't be saved.
        let (api, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)), Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        assert_eq!(api.live_calls(), 1);
        assert!(out.stdout.contains("$40.00  live"), "{}", out.stdout);
        assert!(out.stderr.iter().any(|l| l.contains("can't be saved")), "{:?}", out.stderr);
        // A writable state: inline.
        let ok = StateDir::new("savable");
        let (_, bal) = ctx(FakeBalanceApi::new(), Some(&ok));
        assert_eq!(bal.inline_mode(), Mode::Inline);
    }

    /// The new pods' price on the single-spec path: a known GPU at its preset, an unknown
    /// one (a B300, say) at the ceiling — said as such.
    #[test]
    fn new_pods_of_an_unknown_gpu_are_priced_at_the_ceiling() {
        let policy = BalancePolicy::default();
        let spec = |gpu: &str, n: u32| arena_core::PodSpec { gpu_type: gpu.into(), gpu_count: n, ..arena_core::PodSpec::from_config(&cfg()) };
        let none = std::collections::BTreeMap::new();
        let known = new_pod_price(3, None, &spec("NVIDIA RTX A4000", 1), &policy, &none);
        assert_eq!((known.per_pod, known.stand_in, known.ceiling), (Some(0.32), true, false));
        let unknown = new_pod_price(3, None, &spec("NVIDIA B999 Mystery", 2), &policy, &none);
        assert_eq!((unknown.per_pod, unknown.stand_in, unknown.ceiling), (Some(16.0), true, true));
    }

    /// Review BC5 / BC1 through `arena balance` itself: it lists without details, and RunPod
    /// v1's REST list has no $/h (README: "on RUNPOD_API=v2 the REST list already has GPU and
    /// $"), so a running pod is unpriced. With live working, the provider's whole rate is
    /// blamed on volumes; under `--no-live` ("what the fallback says") the running pod reads
    /// as "no burn".
    #[tokio::test]
    async fn review_bc5_unpriced_v1_pod_in_arena_balance() {
        let dir = StateDir::new("review-bc5");
        let unpriced = Pod { cost_per_hr: None, ..pod("devtest-apple", "RUNNING", 0.0, 1) };
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![unpriced]))] };
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(32.54, 0.47)), Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { verbose: true, ..Default::default() }, T0).await.unwrap();
        let blamed = out.stdout.contains("RunPod reports $0.47/h more than arena lists");
        let no_live = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { no_live: true, ..Default::default() }, T0 + 3_600).await.unwrap();
        let no_burn = no_live.stdout.contains("no burn");
        assert!(!blamed && !no_burn, "blamed on volumes: {blamed}; --no-live says no burn: {no_burn}\n{}\n{}", out.stdout, no_live.stdout);
    }
}
