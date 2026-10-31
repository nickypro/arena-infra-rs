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
    /// (best effort, quiet): per GPU the dearest (a known GPU's balance stand-in is never
    /// below it), per GPU and tier the latest (a preflight's price for a day).
    pub(crate) fn remember_gpu_prices(&self, plan: &OptionPlan) {
        let live: Vec<balance::GpuQuote> = plan
            .options
            .iter()
            .chain(plan.dropped.iter().map(|d| &d.option))
            .filter(|o| o.price_source == Some(arena_core::placement::PriceSource::Live))
            // A near twin's price is the region's (on Vast, its cheapest offer there): the GPU's
            // stand-in is what it costs anywhere — its twin, which is in the plan too.
            .filter(|o| o.near.is_none())
            .filter_map(|o| {
                o.price_per_gpu.map(|per_gpu| balance::GpuQuote {
                    provider: plan.provider.clone(),
                    gpu: o.gpu.clone(),
                    cloud: o.cloud.clone(),
                    per_gpu,
                })
            })
            .collect();
        if let (Some(p), false) = (self.state_path(), live.is_empty()) {
            let _ = balance::remember_gpu_prices(p, &live, unix_now());
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
    // A state that can't be saved is one warning, with why — the save that then fails is the
    // same news, not a second line.
    let unsavable = path.and_then(|p| balance::unsavable(p).map(|why| (p, why)));
    if let Some((p, why)) = &unsavable {
        stderr.push(format!(
            "warning: the balance state {} can't be saved ({why}) — live readings only here, and the footers and \
             preflight read no live balance until it can",
            p.display()
        ));
    }
    let (state, load_warning) = balance::load_opt(path);
    let mode = if flags.no_live { Mode::NoLive } else { Mode::Full };
    // The backup archive bills the Hetzner account but is never a fleet pod (the listing
    // leaves it out): its cost is read from its own project, alongside, within the same
    // budget — not under `--no-live`, which makes no call beyond billing.
    let archive = async {
        if flags.no_live {
            return None;
        }
        match arena_core::ratelimit::bounded(balance::COMMAND_TIMEOUT, bal.api.archive_eur_per_hr()).await {
            Ok(read) => read,
            Err(_) => Some(Err(format!("timed out after {}s", balance::COMMAND_TIMEOUT.as_secs()))),
        }
    };
    // The capacity spares (Hetzner) likewise: in no fleet listing, billing all the same.
    let spares = async {
        if flags.no_live {
            return None;
        }
        match arena_core::ratelimit::bounded(balance::COMMAND_TIMEOUT, bal.api.spares_eur_per_hr()).await {
            Ok(read) => read,
            Err(_) => Some(Err(format!("timed out after {}s", balance::COMMAND_TIMEOUT.as_secs()))),
        }
    };
    let (listed, fetched, archive, spares) = tokio::join!(
        provider.list_by_provider(),
        balance::observe(bal.api.as_ref(), &state, mode, balance::COMMAND_TIMEOUT, now),
        archive,
        spares
    );
    let listing = Listing::from_results(listed);
    let pods = listing.pods();
    // What each machine costs, for `arena spend` (quietly) — not under `--no-live`, which
    // writes nothing.
    if !flags.no_live {
        super::spend::SpendCtx::from_config(cfg).listing(&listing, &pods, now);
    }
    let burns = balance::burns_with(&listing, &pods, &state, &policy);
    let (state, save_warning) = balance::commit(path, state, &fetched, &burns, mode, now);
    let save_warning = save_warning.filter(|_| unsavable.is_none());
    let mut views = balance::views(&state, &fetched, &burns, &policy, mode, now);
    let mut hetzner = balance::postpaid_view(&listing, &pods);
    if let (Some(v), Some(read)) = (hetzner.as_mut(), archive) {
        balance::add_archive(v, read);
    }
    if let (Some(v), Some(read)) = (hetzner.as_mut(), spares) {
        balance::add_spares(v, read);
    }
    views.extend(hetzner);
    for (p, e) in listing.errors() {
        // What stands in for its burn: the one last observed, if recent enough — else nothing.
        let burn = views.iter().find(|v| v.account == p).and_then(|v| v.burn).filter(|b| b.stale);
        let what = match burn {
            Some(b) if b.recent() => {
                format!("its burn is the one observed {} ago (stale)", arena_core::snapshot::fmt_age(b.stale_secs.unwrap_or(0)))
            }
            Some(b) => format!(
                "its burn is at least the one observed {} ago (stale)",
                arena_core::snapshot::fmt_age(b.stale_secs.unwrap_or(0))
            ),
            None => "its burn is unknown".into(),
        };
        stderr.push(format!("warning: {p} failed to list ({e}) — {what}"));
    }
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

/// `arena balance set <runpod|vast|shadeform> <USD>`: record the balance read off the provider's
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

/// The single spec's live price, read alongside the preflight's balance ([`balance_preflight`]):
/// the plan placement would make for that one GPU and tier, priced as placement prices it,
/// or `None` when no answer came in time.
pub(crate) type SpecQuote<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Option<OptionPlan>> + Send + 'a>>;

/// The `create`/`up` preflight: the target provider's account only, observed inline (no
/// extra listing: the burn is the provider's or the last recorded sample), with the new
/// pods priced at the dearest placement option — on the single-spec path, its one GPU and
/// tier read live alongside (`quote`, within the same bound; remembered like any plan's
/// prices) — an option with no live price at the one read for that GPU and tier under a day
/// ago, else at a stand-in that errs high (`~`), never free. Advisory and quiet: lines to
/// print, never an error.
pub(crate) async fn balance_preflight(
    provider: &dyn Provider,
    cfg: &Config,
    bal: &BalanceCtx,
    new_pods: usize,
    options: Option<&OptionPlan>,
    spec: &arena_core::PodSpec,
    quote: Option<SpecQuote<'_>>,
) -> Vec<String> {
    let Some(account) = Account::from_name(provider.name()) else { return Vec::new() };
    // No balance to show: no catalog request either.
    if !bal.api.accounts().iter().any(|(a, _)| *a == account) {
        return Vec::new();
    }
    let now = unix_now();
    let mode = bal.inline_mode();
    let (state, _) = balance::load_opt(bal.state_path());
    let quoting = async move {
        match quote {
            Some(q) if options.is_none() => q.await,
            _ => None,
        }
    };
    let observing = balance::observe_account(bal.api.as_ref(), &state, account, mode, balance::INLINE_TIMEOUT, now);
    let (fetched, quoted) = tokio::join!(observing, quoting);
    let Some(fetched) = fetched else { return Vec::new() };
    let fetched = [fetched];
    let burns = balance::Burns::new();
    let (state, _) = balance::commit(bal.state_path(), state, &fetched, &burns, mode, now);
    if let Some(plan) = &quoted {
        bal.remember_gpu_prices(plan);
    }
    let (policy, _) = BalancePolicy::from_config(cfg);
    let views = balance::views(&state, &fetched, &burns, &policy, mode, now);
    let new = new_pod_price(new_pods, options.or(quoted.as_ref()), spec, provider.name(), &policy, &state, now);
    views.first().map(|v| balance::preflight_lines_with(v, new, &policy, now)).unwrap_or_default()
}

/// What the new pods cost per pod at most: the dearest placement option (on the single-spec
/// path, the spec's one GPU and tier). Per option: a live price as it is; else the live price
/// read for that GPU and tier on `provider` under a day ago ([`balance::QUOTE_FRESH`],
/// `quoted`); else a preset estimate, or no price, at least the known GPU's stand-in (the
/// most it is known to cost: RunPod's recorded catalog, live prices seen), an unknown GPU at
/// the ceiling. `per_pod: None` only when not even a stand-in can be named.
fn new_pod_price(
    count: usize,
    options: Option<&OptionPlan>,
    spec: &arena_core::PodSpec,
    provider: &str,
    policy: &BalancePolicy,
    state: &balance::BalanceState,
    now: u64,
) -> balance::NewPods {
    use arena_core::placement::PriceSource;
    let (mut guessed, mut ceiling, mut quoted) = (false, false, None::<u64>);
    let mut price = |gpu: &str, cloud: Option<&str>, n: u32, live: Option<f64>, estimate: Option<f64>| {
        if live.is_some() {
            return live;
        }
        if let Some((per_gpu, age)) = state.fresh_quote(provider, gpu, cloud, now) {
            quoted = Some(quoted.map_or(age, |q| q.max(age)));
            return Some(per_gpu * f64::from(n));
        }
        match (balance::stand_in_with(Some(gpu), Some(n), policy, &state.gpu_seen), estimate) {
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
            .map(|o| {
                let live = o.price_per_pod.filter(|_| o.price_source == Some(PriceSource::Live));
                price(&o.gpu, o.cloud.as_deref(), o.gpu_count.max(1), live, o.price_per_pod)
            })
            .collect::<Option<Vec<f64>>>()
            .map(|prices| prices.into_iter().fold(0.0, f64::max)),
        _ => {
            // The tier the single spec creates on (RunPod's only — Vast has none).
            let cloud = (provider == "runpod").then(|| spec.cloud_type.trim().to_uppercase());
            price(&spec.gpu_type, cloud.as_deref(), spec.gpu_count.max(1), None, None)
        }
    };
    let known = per_pod.is_some();
    balance::NewPods { count, per_pod, stand_in: guessed && known, ceiling: ceiling && known, quoted: quoted.filter(|_| known) }
}

/// One pod's probe call, read: the reply, or why there is none. A call that failed outright
/// (no marker in its output: ssh couldn't connect) reads as its exit code and last stderr line.
pub(crate) fn read_probe(call: PodCall) -> Probe {
    let out = match call {
        Ok(out) => out,
        Err(why) => return Probe::Failed(why),
    };
    if !out.success && !out.stdout.contains("ARENA_IDLE_") {
        let stderr = arena_core::ssh::strip_interactive_noise(&out.stderr);
        let last = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("(no output)");
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
        idle::render(&report).lines().for_each(say);
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
            "  hetzner: postpaid",
        ] {
            assert!(out.stdout.contains(needle), "`{needle}` missing:\n{}", out.stdout);
        }
        // One reading isn't a gap yet: RunPod's rate lags a terminate by minutes (live
        // follow-up 5) — the next one, 10 min on, confirms it.
        assert!(!out.stdout.contains("more than arena lists"), "{}", out.stdout);
        assert_eq!(api.live_calls(), 2);
        // Recorded: the anchors are there for the fallback.
        let (state, _) = arena_core::balance::load(&dir.file());
        assert_eq!(state.accounts.len(), 2);
        // -v: the parts behind the numbers.
        let v = balance_report(&mixed_fleet(), &dir.cfg(""), &bal, BalanceFlags { verbose: true, ..Default::default() }, T0 + 600)
            .await
            .unwrap();
        for needle in [
            "  runpod: RunPod reports $0.22/h more than arena lists",
            "  burn: provider $0.47/h · listed $0.25/h — gap $0.22/h not in the listing",
            "  spendLimit $80.00/h · underBalance false",
            "state file: ",
            "  postpaid €0.006/h — servers only: volumes not counted",
        ] {
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
        type Case = (&'static str, std::result::Result<Live, Fail>, std::result::Result<Live, Fail>, i32);
        let cases: Vec<Case> = vec![
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

    /// Merge of wave 7 balance with lane G: `config check` shows both state-file sections —
    /// the balance sources and the machines `up --check` remembers as bad — read from one
    /// state directory, whose lock both writers take: a bad-host write after a balance
    /// reading, then a price write after it, and each file keeps the other's records. Neither
    /// section makes a balance call.
    #[tokio::test]
    async fn config_check_shows_the_balance_and_the_bad_hosts_from_one_state_dir() {
        use arena_core::snapshot::{self, BadHost};
        let dir = StateDir::new("merge-w7");
        let conf = dir.cfg("");
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(31.0, 0.0)), Some(&dir));
        balance_report(&fleet, &conf, &bal, BalanceFlags::default(), T0).await.unwrap();
        // G's health cache resolves into the balance file's directory.
        let health = match crate::health_cache_path(&conf) {
            Some(Ok(p)) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(health.parent(), dir.file().parent());
        let now = snapshot::unix_now();
        let bad = BadHost { failed_at: now - 3_600, pod: "devtest-apple".into(), reason: "cuda: RuntimeError: CUDA unknown error".into() };
        assert_eq!(snapshot::record_bad_host(&health, "runpod machine mbad", &bad, now).unwrap(), None);
        bal.remember_prices(&[pod("devtest-apple", "RUNNING", 0.17, 1)]);

        let panicking = BalanceCtx { api: Arc::new(FakeBalanceApi::new().with(Account::Runpod).panicking()), path: Some(Ok(dir.file())) };
        let mut missing = Vec::new();
        let balance = crate::balance_section(&conf, &panicking, T0 + 240, &mut missing);
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(balance[0], "\nBalance (`arena balance`):");
        assert!(balance.iter().any(|l| l == "  ✓ runpod                     live 4m ago"), "{balance:#?}");
        let bad_hosts = crate::bad_hosts_section(&conf, now);
        assert_eq!(bad_hosts[0], "\nMachines `up --check` remembers as bad (`pods up --retry-bad-hosts` ignores them):");
        assert!(matches!(&bad_hosts[1..], [l] if l.starts_with("  ⚠ runpod machine mbad — failed 1h ago")), "{bad_hosts:#?}");
        let (state, warning) = balance::load(&dir.file());
        assert_eq!(warning, None);
        assert_eq!(state.accounts[&Account::Runpod].prices.get("id-devtest-apple").map(|p| p.0), Some(0.17));
        assert!(state.accounts[&Account::Runpod].anchor.is_some(), "the reading survived both writes");
        // No state directory: no bad-hosts section at all.
        assert!(crate::bad_hosts_section(&cfg(), now).is_empty());
    }

    /// A ✗ row fails `config check`: an invalid `BALANCE_WARN_HOURS` / `BALANCE_UNPRICED_PER_GPU`
    /// joins "missing or invalid keys", as G's invalid CUDA value does — while every runtime
    /// surface keeps using the default: `pods list` prints exactly what it prints with the
    /// keys unset.
    #[tokio::test]
    async fn an_invalid_balance_key_fails_config_check_but_never_pods_list() {
        // Everything `config check` requires for hetzner (a dummy key: nothing is called).
        let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nHETZNER_API_KEY=not-a-real-key\n";
        crate::config_check(&Config::parse(base), "hetzner", &[]).unwrap();
        let mut missing = Vec::new();
        crate::balance_section(&Config::parse(base), &no_balance(), T0, &mut missing);
        assert!(missing.is_empty(), "{missing:?}");
        let cases = [
            ("BALANCE_WARN_HOURS=soon", "missing or invalid keys: BALANCE_WARN_HOURS (must be a number of hours ≥ 0)"),
            (
                "BALANCE_UNPRICED_PER_GPU=free",
                "missing or invalid keys: BALANCE_UNPRICED_PER_GPU (must be USD per GPU-hour, above 0 and at most 1000)",
            ),
        ];
        for (line, want) in cases {
            let conf = Config::parse(&format!("{base}{line}\n"));
            let mut missing = Vec::new();
            let rows = crate::balance_section(&conf, &no_balance(), T0, &mut missing);
            assert!(rows.iter().any(|r| r.starts_with("  ✗ BALANCE_")), "{rows:#?}");
            assert_eq!(missing.len(), 1, "{line}");
            assert_eq!(crate::config_check(&conf, "hetzner", &[]).unwrap_err().to_string(), want);
        }
        // Both wrong: both named, after anything else that's missing.
        let both = Config::parse("BALANCE_WARN_HOURS=-1\nBALANCE_UNPRICED_PER_GPU=0\n");
        let e = crate::config_check(&both, "hetzner", &[]).unwrap_err().to_string();
        assert!(e.starts_with("missing or invalid keys: HETZNER_API_KEY, MACHINE_NAME_LIST") && e.contains("BALANCE_WARN_HOURS (") && e.ends_with("at most 1000)"), "{e}");

        // Runtime: the defaults — `pods list` byte for byte as with the keys unset.
        let listed = |extra: &'static str| async move {
            let dir = StateDir::new(&format!("invalid-keys-{}", extra.len()));
            let api = FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(48.0, 0.0)).live_ok(Account::Vast, vast_live(136.87));
            let (_, bal) = ctx(api, Some(&dir));
            crate::list_with(&two_providers(), &no_remote(), &dir.cfg(extra), &bal, false, false).await.unwrap()
        };
        let plain = listed("").await;
        assert_eq!(listed("BALANCE_WARN_HOURS=soon\nBALANCE_UNPRICED_PER_GPU=free\n").await, plain);
        assert!(plain.0.ends_with("balance: runpod $48.00 live ≈4.0d · vast $136.87 live no burn\n"), "{}", plain.0);
    }

    /// Merge of wave 7 archive: the backup archive bills the Hetzner account but is never a
    /// fleet pod (the Hetzner listing leaves it out). `arena balance` reads its cost from its
    /// own project and counts it in Hetzner's burn as its own part, said in a note and under
    /// `-v`; a read that fails is said and left out; no archive changes nothing; `--no-live`
    /// doesn't ask.
    #[tokio::test]
    async fn balance_counts_the_archive_in_hetzners_burn_never_as_a_pod() {
        let cpu = Pod {
            id: "h1".into(),
            name: "devtest-cpu".into(),
            provider: "hetzner".into(),
            status: "running".into(),
            cost_per_hr: Some(0.012),
            ..Default::default()
        };
        let fleet = Fleet { backends: vec![("hetzner", Ok(vec![cpu]))] };
        let dir = StateDir::new("archive-burn");
        let json = BalanceFlags { json: true, ..Default::default() };
        let hetzner = |out: &Printed| -> serde_json::Value {
            let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
            v.as_array().unwrap().iter().find(|a| a["account"] == "hetzner").cloned().unwrap()
        };
        let (_, bal) = ctx(FakeBalanceApi::new().archive(Ok(Some(0.03))), Some(&dir));
        let hz = hetzner(&balance_report(&fleet, &dir.cfg(""), &bal, json, T0).await.unwrap());
        assert!((hz["burn"]["per_hr"].as_f64().unwrap() - 0.042).abs() < 1e-9, "{hz:#}");
        assert_eq!((hz["listed_per_hr"].as_f64(), hz["archive_per_hr"].as_f64()), (Some(0.012), Some(0.03)), "{hz:#}");
        let text = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { verbose: true, ..Default::default() }, T0).await.unwrap().stdout;
        assert!(text.contains("hetzner: includes the backup archive, €0.030/h (its server and volumes — not a fleet pod"), "{text}");
        assert!(text.contains("postpaid €0.042/h — fleet servers €0.012/h + the backup archive €0.030/h"), "{text}");
        // `--no-live`: not asked.
        let hz = hetzner(&balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags { no_live: true, ..json }, T0).await.unwrap());
        assert!(hz.get("archive_per_hr").is_none() && (hz["burn"]["per_hr"].as_f64().unwrap() - 0.012).abs() < 1e-9, "{hz:#}");
        // No archive: nothing changes. A failed read: said, not counted.
        let (_, none) = ctx(FakeBalanceApi::new().archive(Ok(None)), Some(&dir));
        let hz = hetzner(&balance_report(&fleet, &dir.cfg(""), &none, json, T0).await.unwrap());
        assert!(hz.get("archive_per_hr").is_none() && hz.get("archive_unread").is_none(), "{hz:#}");
        let (_, failed) = ctx(FakeBalanceApi::new().archive(Err("hetzner archive list servers HTTP 503".into())), Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &failed, BalanceFlags::default(), T0).await.unwrap();
        assert!(out.stdout.contains("hetzner: the backup archive's cost couldn't be read (hetzner archive list servers HTTP 503) — not included"), "{}", out.stdout);
        assert_eq!(out.code, 0, "information only");
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

    /// `pods list` keeps `SSH_CONFIG_FILE` current from its own listing (spec ssh-config-include):
    /// written when an endpoint moved, untouched (and nothing said) when none did, never from a
    /// listing a provider failed; the note goes with the warnings (stderr), the table and
    /// `--json` stay as they were.
    #[tokio::test]
    async fn pods_list_keeps_the_ssh_config_file_current() {
        let path = std::env::temp_dir().join(format!("arena-list-sshconfig-{}.conf", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nSSH_CONFIG_FILE={}\n", path.display()));
        let fleet = |port| Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, port)])), ("vast", Ok(vec![]))] };
        let (out, err) = crate::list_with(&fleet(22001), &no_remote(), &cfg, &no_balance(), false, false).await.unwrap();
        assert!(out.contains("devtest-apple"), "{out}");
        assert_eq!(err, [format!("[ssh-config] after pods list: rewrote {}", path.display())]);
        assert!(std::fs::read_to_string(&path).unwrap().contains("Host devtest-apple\n    HostName 203.0.113.7\n    Port 22001\n"));
        let (json, err) = crate::list_with(&fleet(22001), &no_remote(), &cfg, &no_balance(), true, false).await.unwrap();
        assert!(err.is_empty(), "nothing moved: {err:?}");
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok(), "stdout stays JSON");
        let (_, err) = crate::list_with(&fleet(22002), &no_remote(), &cfg, &no_balance(), false, false).await.unwrap();
        assert_eq!(err.len(), 1, "{err:?}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("Port 22002"));
        let partial = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.5, 22003)])), ("vast", Err("HTTP 429"))] };
        let (_, err) = crate::list_with(&partial, &no_remote(), &cfg, &no_balance(), false, false).await.unwrap();
        assert!(err.iter().all(|w| !w.contains("[ssh-config]")), "{err:?}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("Port 22002"), "a partial listing rewrites nothing");
        let _ = std::fs::remove_file(&path);
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

    /// Merge of wave 7 balance with fix batch P (finding #9): `list_with` keeps the footer's
    /// PARTIAL mark and the "NOT known to be empty" line when a provider didn't answer — the
    /// balance line still comes under them.
    #[tokio::test]
    async fn pods_list_marks_a_partial_fleet_above_the_balance_line() {
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![pod("devtest-apple", "RUNNING", 0.25, 1)])), ("vast", Err("list pods HTTP 429"))] };
        let dir = StateDir::new("partial-footer");
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Vast, vast_live(40.0)), Some(&dir));
        let (out, _) = crate::list_with(&fleet, &no_remote(), &cfg(), &bal, false, false).await.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let footer = lines[lines.len() - 2];
        assert!(
            footer.starts_with("fleet: $0.25/h across 1 billing pod(s)")
                && footer.ends_with(" — PARTIAL: vast didn't answer, so its pods and their cost are missing"),
            "{out}"
        );
        assert_eq!(lines[lines.len() - 1], "balance: vast $40.00 live ?");
        // Nothing listed while a provider failed: not "(no pods)" — and the JSON is the pods alone.
        let empty = Fleet { backends: vec![("runpod", Ok(vec![])), ("vast", Err("list pods HTTP 429"))] };
        let (out, _) = crate::list_with(&empty, &no_remote(), &cfg(), &no_balance(), false, false).await.unwrap();
        assert_eq!(out, "(no pods listed — vast didn't answer, so this is NOT known to be empty)\n");
        let (json, _) = crate::list_with(&empty, &no_remote(), &cfg(), &no_balance(), true, false).await.unwrap();
        assert_eq!(json, "[]\n");
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
            let lines = balance_preflight(&fleet(), &dir.cfg(""), &bal, 1, None, &spec, None).await;
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
            balance_preflight(&fleet, &dir.cfg(""), &bal, 1, None, &spec, None).await;
            assert_eq!(api.live_calls(), 0, "no live call from an inline surface");
        }
        // `arena balance` reads live, and says the state can't be saved.
        let (api, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)), Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        assert_eq!(api.live_calls(), 1);
        assert!(out.stdout.contains("$40.00  live"), "{}", out.stdout);
        // Said once, with why — not again as "couldn't save" (the load's own "can't read" is
        // another matter: here the state is a directory).
        assert!(out.stderr[0].contains("can't be saved (it isn't a regular file)"), "{:?}", out.stderr);
        assert!(!out.stderr.iter().any(|l| l.contains("couldn't save")), "{:?}", out.stderr);
        // A writable state: inline.
        let ok = StateDir::new("savable");
        let (_, bal) = ctx(FakeBalanceApi::new(), Some(&ok));
        assert_eq!(bal.inline_mode(), Mode::Inline);
    }

    /// Live follow-up 6: a read-only state directory (`chmod a-w`) printed two warnings —
    /// "can't be saved", then "couldn't save … Permission denied". One, with the reason.
    #[tokio::test]
    async fn a_read_only_state_dir_is_one_warning() {
        use std::os::unix::fs::PermissionsExt;
        let dir = StateDir::new("read-only");
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(30.63, 0.0)), Some(&dir));
        balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        let state_dir = dir.file().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let readonly = !balance::writable(&dir.file()); // root writes anyway: nothing to see
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(30.63, 0.0)), Some(&dir));
        let out = balance_report(&fleet, &dir.cfg(""), &bal, BalanceFlags::default(), T0 + 600).await.unwrap();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readonly {
            assert_eq!(out.stderr.len(), 1, "{:?}", out.stderr);
            assert!(out.stderr[0].contains("can't be saved (Permission denied"), "{:?}", out.stderr);
            assert!(out.stdout.contains("$30.63  live"), "{}", out.stdout);
        }
    }

    /// Live follow-up 4: one 429 on Vast's listing (its idle $0 seen 15 min before) read
    /// `source: unknown, level: unknown` under `--no-live`. The recent burn stands in: an
    /// estimate, level ok — and the warning says what stands in.
    #[tokio::test]
    async fn a_throttled_vast_listing_keeps_an_idle_account_ok() {
        let dir = StateDir::new("vast-429");
        let api = || FakeBalanceApi::new().live_ok(Account::Vast, vast_live(136.84));
        let ok = Fleet { backends: vec![("vast", Ok(vec![]))] };
        let (_, bal) = ctx(api(), Some(&dir));
        balance_report(&ok, &dir.cfg(""), &bal, BalanceFlags::default(), T0).await.unwrap();
        let throttled = Fleet { backends: vec![("vast", Err("vast list HTTP 429 Too Many Requests"))] };
        let (_, bal) = ctx(api(), Some(&dir));
        let flags = BalanceFlags { no_live: true, check: true, json: true, ..Default::default() };
        let out = balance_report(&throttled, &dir.cfg(""), &bal, flags, T0 + 900).await.unwrap();
        assert_eq!(out.stderr, ["warning: vast failed to list (provider error: vast list HTTP 429 Too Many Requests) — its burn is the one observed 15m ago (stale)"]);
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!((v[0]["source"].as_str(), v[0]["level"].as_str(), v[0]["burn"]["stale"].as_bool()), (Some("estimate"), Some("ok"), Some(true)), "{v}");
        assert_eq!(out.code, 0);
        // Still failing 40 min on: unknown, as before (`--check` 3).
        let (_, bal) = ctx(api(), Some(&dir));
        let out = balance_report(&throttled, &dir.cfg(""), &bal, flags, T0 + 2_400).await.unwrap();
        assert!(out.stderr[0].ends_with("— its burn is unknown"), "{:?}", out.stderr);
        assert_eq!(out.code, 3);
    }

    /// The new pods' price on the single-spec path: a known GPU at its preset, an unknown
    /// one (a B300, say) at the ceiling — said as such.
    #[test]
    fn new_pods_of_an_unknown_gpu_are_priced_at_the_ceiling() {
        let policy = BalancePolicy::default();
        let spec = |gpu: &str, n: u32| arena_core::PodSpec { gpu_type: gpu.into(), gpu_count: n, ..arena_core::PodSpec::from_config(&cfg()) };
        let none = balance::BalanceState::default();
        let known = new_pod_price(3, None, &spec("NVIDIA RTX A4000", 1), "runpod", &policy, &none, T0);
        assert_eq!((known.per_pod, known.stand_in, known.ceiling), (Some(0.32), true, false));
        let unknown = new_pod_price(3, None, &spec("NVIDIA B999 Mystery", 2), "runpod", &policy, &none, T0);
        assert_eq!((unknown.per_pod, unknown.stand_in, unknown.ceiling), (Some(16.0), true, true));
    }

    /// One option of a plan, priced as the catalog or an offer search priced it.
    fn option(gpu: &str, cloud: Option<&str>, per_gpu: Option<f64>, source: Option<arena_core::placement::PriceSource>) -> arena_core::placement::PlacementOption {
        arena_core::placement::PlacementOption {
            listed: 0,
            gpu: gpu.into(),
            label: format!("1×{gpu}"),
            cloud: cloud.map(String::from),
            gpu_count: 1,
            price_per_gpu: per_gpu,
            price_per_pod: per_gpu,
            price_source: source,
            stock: None,
            near: None,
            cap: None,
            server_type: None,
        }
    }

    fn plan_of(provider: &str, options: Vec<arena_core::placement::PlacementOption>) -> OptionPlan {
        OptionPlan {
            provider: provider.into(),
            gpu_count: 1,
            order: arena_core::placement::Order::Listed,
            max_price: None,
            options,
            dropped: Vec::new(),
            notes: Vec::new(),
            near: None,
        }
    }

    /// Live follow-up 2: `pods up bravo --gpu A4000 --cloud community --dry-run` stood the A4000
    /// preset ($0.32) in while `offers` knew $0.17 community. The single spec is priced by
    /// its tier's live price read now; else by the one read for that GPU and tier under a day
    /// ago (`~`, with its age); else at the stand-in. Per tier: a secure price never prices a
    /// community pod, nor one on another provider.
    #[test]
    fn the_single_spec_is_priced_by_its_tiers_live_price() {
        use arena_core::placement::PriceSource::{Estimate, Live as Now};
        let policy = BalancePolicy::default();
        let a4000 = |cloud: &str| arena_core::PodSpec {
            gpu_type: "NVIDIA RTX A4000".into(),
            gpu_count: 1,
            cloud_type: cloud.into(),
            ..arena_core::PodSpec::from_config(&cfg())
        };
        let price = |plan: Option<&OptionPlan>, spec: &arena_core::PodSpec, provider: &str, st: &balance::BalanceState, now: u64| {
            let n = new_pod_price(1, plan, spec, provider, &policy, st, now);
            (n.per_pod, n.stand_in, n.quoted)
        };
        let empty = balance::BalanceState::default();
        // Read now (the quote the preflight fetched alongside): as it is.
        let now = plan_of("runpod", vec![option("NVIDIA RTX A4000", Some("COMMUNITY"), Some(0.17), Some(Now))]);
        assert_eq!(price(Some(&now), &a4000("COMMUNITY"), "runpod", &empty, T0), (Some(0.17), false, None));
        // Nothing read now: the price `offers` saw 3 h ago for that very tier.
        let mut st = balance::BalanceState::default();
        st.gpu_seen.insert("NVIDIA RTX A4000".into(), 0.25);
        st.gpu_quotes.insert(balance::quote_key("runpod", "NVIDIA RTX A4000", Some("COMMUNITY")), (0.17, T0 - 3 * 3_600));
        st.gpu_quotes.insert(balance::quote_key("runpod", "NVIDIA RTX A4000", Some("SECURE")), (0.25, T0 - 3 * 3_600));
        assert_eq!(price(None, &a4000("COMMUNITY"), "runpod", &st, T0), (Some(0.17), false, Some(3 * 3_600)));
        assert_eq!(price(None, &a4000("community"), "runpod", &st, T0), (Some(0.17), false, Some(3 * 3_600)), "tier case");
        assert_eq!(price(None, &a4000("SECURE"), "runpod", &st, T0), (Some(0.25), false, Some(3 * 3_600)));
        // A catalog that failed (preset estimates) still goes by the fresh quote.
        let failed = plan_of("runpod", vec![option("NVIDIA RTX A4000", Some("COMMUNITY"), Some(0.32), Some(Estimate))]);
        assert_eq!(price(Some(&failed), &a4000("COMMUNITY"), "runpod", &st, T0), (Some(0.17), false, Some(3 * 3_600)));
        // Another provider's quote prices nothing here; a day-old one neither: the stand-in.
        assert_eq!(price(None, &a4000("COMMUNITY"), "vast", &st, T0), (Some(0.32), true, None));
        assert_eq!(price(None, &a4000("COMMUNITY"), "runpod", &st, T0 + 21 * 3_600 + 1), (Some(0.32), true, None));
        // Vast has no tiers: its quote is keyed without one, whatever CLOUD_TYPE says.
        st.gpu_quotes.insert(balance::quote_key("vast", "NVIDIA RTX A4000", None), (0.098, T0 - 60));
        assert_eq!(price(None, &a4000("COMMUNITY"), "vast", &st, T0), (Some(0.098), false, Some(60)));
    }

    /// The live case end to end: $30.77 left, $0.14/h burning, `up bravo --gpu A4000 --cloud
    /// community`. With the A4000 preset stood in, ≈2.7d and a ⚠ under 72 h; with the
    /// community price read alongside, `≤$0.17/h` (no `~`) and no warning — and that price is
    /// remembered, so a preflight whose catalog read fails goes by it, said with its age.
    #[tokio::test]
    async fn the_single_spec_preflight_reads_its_live_price_alongside() {
        let dir = StateDir::new("single-quote");
        let spec = arena_core::PodSpec {
            gpu_type: "NVIDIA RTX A4000".into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            ..arena_core::PodSpec::from_config(&dir.cfg(""))
        };
        let fleet = Fleet { backends: vec![("runpod", Ok(vec![]))] };
        let live = |q: Option<OptionPlan>| -> Option<SpecQuote<'static>> { Some(Box::pin(async move { q })) };
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(30.77, 0.14)), Some(&dir));
        let stood_in = balance_preflight(&fleet, &dir.cfg(""), &bal, 1, None, &spec, live(None)).await;
        assert!(stood_in[0].ends_with("$0.14/h now + ≤~$0.32/h for 1 new pod(s) (a stand-in that errs high — no live price) → ≈2.7d ⚠ below BALANCE_WARN_HOURS=72"), "{stood_in:?}");
        let quote = plan_of("runpod", vec![option("NVIDIA RTX A4000", Some("COMMUNITY"), Some(0.17), Some(arena_core::placement::PriceSource::Live))]);
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(30.77, 0.14)), Some(&dir));
        let read = balance_preflight(&fleet, &dir.cfg(""), &bal, 1, None, &spec, live(Some(quote))).await;
        assert!(read[0].ends_with("$0.14/h now + ≤$0.17/h for 1 new pod(s) → ≈4.1d"), "{read:?}");
        let (state, _) = balance::load(&dir.file());
        assert_eq!(state.gpu_quotes.get(&balance::quote_key("runpod", "NVIDIA RTX A4000", Some("COMMUNITY"))).map(|q| q.0), Some(0.17));
        // The catalog down now: the remembered price, said as such.
        let (_, bal) = ctx(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(30.77, 0.14)), Some(&dir));
        let remembered = balance_preflight(&fleet, &dir.cfg(""), &bal, 1, None, &spec, live(None)).await;
        assert!(remembered[0].contains("+ ≤~$0.17/h for 1 new pod(s) (the live price seen ") && remembered[0].ends_with(" ago) → ≈4.1d"), "{remembered:?}");
        // No balance account: no preflight — and the quote isn't even awaited.
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = asked.clone();
        let never: Option<SpecQuote<'static>> = Some(Box::pin(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            None
        }));
        assert!(balance_preflight(&fleet, &dir.cfg(""), &no_balance(), 1, None, &spec, never).await.is_empty());
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst), "no catalog request without a balance to show");
    }

    /// Review BC5 / BC1 through `arena balance` itself: it lists without details, and RunPod
    /// v1's REST list has no $/h (docs/REFERENCE.md, `pods list`: "on REST v2, the default, the
    /// REST list already has GPU and $"), so a running pod is unpriced. With live working, the
    /// provider's whole rate is blamed on volumes; under `--no-live` ("what the fallback says")
    /// the running pod reads as "no burn".
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
