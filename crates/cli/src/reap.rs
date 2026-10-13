//! `arena pods reap …` — the opt-in auto-reaper (spec `wave7-reaper`): terminate a pod past
//! the end time an operator set, or one demonstrably unused for an operator-chosen window,
//! after a verified backup — and only when every check is certain. The decisions are
//! `arena_core::reap` (pure, table-tested); this is the I/O around them: the state file and
//! its locks, the listing, the probes over `Remote`, the per-pod backup (git, the two rsync
//! tiers, the verify), the re-listing, the one terminate (through a `ReapTicket`, never
//! directly), the proxy sync, the participant notice and `wall`, notifications and the log.
//!
//! Off by default, and in layers (spec I1): a pod needs a policy (`reap set`, or `--expire-*`
//! / `--idle-after` at `up`/`create`); a run acts only with `--apply`, `REAP_ENABLED=1` in the
//! config FILE (the environment can't set it) and, with no terminal, `--yes`. Everything
//! else — and every check that can't be made — is "observe and log".

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::reap::judge::{
    self, caps_left, classify, due_row, observation_row, order_candidates, pre_ssh_gate, quiet_now, recheck_state, relist_verdict,
    BackupEvidence, BackupProof, ClassifyCtx, Decision, Frozen, GateCtx, JobsOutcome, Listed, Mode, QuietCtx, ReapTicket, RunGate,
    RunInputs, Row, StepEnd, TriggerProof,
};
use arena_core::reap::probe::{self, ProbeError, ProbeFacts, RepoSite, Since};
use arena_core::reap::render::{self, LogLine, NotifyKind, PlanHeader, PlanRow};
use arena_core::reap::store::{self, Store, StoreError};
use arena_core::reap::{self as core_reap, Hold, Observation, Pin, PodPolicy, ReapSettings, ReapState, Reason, Tombstone, Trigger, Verdict};
use arena_core::remote::Remote;
use arena_core::selector::{Naming, Selector};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod, Provider};

use super::Select;

/// `arena pods reap …`.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum ReapCmd {
    /// Look at every pod with a reap policy (one read-only probe each), record what was seen,
    /// and print the plan: who would be reaped and why not. Nothing is changed on any pod.
    ///
    /// --apply: also act, on at most REAP_MAX_PER_RUN due pods (--max lowers it): back each up
    /// (git push + both rsync tiers into the backup dir, then a dry-run verify), check it's still
    /// quiet and still the same pod, re-list it, then terminate it. Needs REAP_ENABLED=1 in the
    /// config file (else it runs as observe), no hold (`reap hold`), a verified clock and, with no
    /// terminal, --yes. --rehearse does everything up to and including the real backup and
    /// verify, then stops — no terminate. Exits 3 when something needs attention.
    #[command(verbatim_doc_comment)]
    Run {
        /// Act on due pods (see above).
        #[arg(long, conflicts_with = "rehearse")]
        apply: bool,
        /// Run every gate including the real backup and verify, but never terminate.
        #[arg(long)]
        rehearse: bool,
        /// At most this many terminate attempts this run (only lowers REAP_MAX_PER_RUN).
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=5))]
        max: Option<u32>,
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Give the selected pods a reap policy (or change it): an end time and/or an idle window.
    /// Best effort, writes ~/.arena/NOTICE on each pod and a login-banner line (interactive
    /// shells only) that shows it. Needs names or --all; only this cohort's MACHINE_NAME_LIST
    /// machines can have one.
    Set {
        /// Reap after this long from now: e.g. `90m`, `8h`, `1d12h` (10 min .. 60 days).
        #[arg(long, value_name = "DUR", conflicts_with_all = ["expire_at", "extend"])]
        expire_in: Option<String>,
        /// Reap at this time: RFC 3339 (`2026-10-12T18:00:00Z`) or `YYYY-MM-DD HH:MM` (local).
        #[arg(long, value_name = "TIME", conflicts_with = "extend")]
        expire_at: Option<String>,
        /// Push the current end time back by this long (from now, if it has passed or there is none).
        #[arg(long, value_name = "DUR")]
        extend: Option<String>,
        /// Reap after this long with no activity at all: e.g. `8h` (REAP_IDLE_FLOOR_MINS .. 14 days).
        #[arg(long, value_name = "DUR")]
        idle_after: Option<String>,
        /// A note kept with the policy.
        #[arg(long)]
        note: Option<String>,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Remove the selected pods' expiry and/or idle policy (both when neither flag is given).
    Clear {
        /// Remove only the end time.
        #[arg(long)]
        expire: bool,
        /// Remove only the idle window.
        #[arg(long)]
        idle: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Pin pods: never reaped while pinned, whatever their policy (stored by name AND id).
    Pin {
        /// Until this time (RFC 3339 or `YYYY-MM-DD HH:MM` local); default: until unpinned.
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        /// A note kept with the pin.
        #[arg(long)]
        note: Option<String>,
        #[command(flatten)]
        sel: Select,
    },
    /// Remove pins matching these names or ids (or --all). Reads only the state.
    Unpin {
        #[command(flatten)]
        sel: Select,
    },
    /// The reaper's state — policies, pins, streaks, last decisions, hold, what was reaped.
    /// No network.
    Status {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// The reaper's log (`reap-log.jsonl`): one line per evaluated pod per run, plus events.
    Log {
        /// The last N lines.
        #[arg(long, default_value_t = 50)]
        tail: usize,
        /// Only this pod's lines (full or bare name).
        #[arg(long)]
        pod: Option<String>,
        /// Print the raw JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Stop all acting: `--apply` runs observe until `reap release`.
    Hold {
        /// Why (shown by every run until released).
        #[arg(long)]
        reason: String,
    },
    /// Show the hold's reason and clear it (asks first) — also a hold the reaper set itself.
    Release,
}

/// How a reap command ends when it isn't plain success or failure (spec §4 step 13): `2` the
/// config is invalid (G1), `3` something needs attention (a backup or terminate failed, a hold
/// is set, the clock or the destination refused).
#[derive(Debug)]
pub(crate) struct ReapExit {
    pub code: u8,
    pub why: String,
}

impl std::fmt::Display for ReapExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for ReapExit {}

fn config_error(why: impl Into<String>) -> anyhow::Error {
    ReapExit { code: 2, why: why.into() }.into()
}

/// Everything the reaper reaches the world through — real in `main`, scripted in tests.
pub(crate) struct ReapEnv<'a> {
    pub provider: &'a dyn Provider,
    pub remote: Arc<dyn Remote>,
    pub cfg: &'a Config,
    pub yes: bool,
    /// The control clock (unix seconds).
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Whether the clock is NTP-synchronised (`timedatectl`); `None` = couldn't tell.
    pub ntp: &'a (dyn Fn() -> Option<bool> + Sync),
    /// The local zone's offset (seconds), for display.
    pub tz: i64,
    /// The rsync binary.
    pub rsync: String,
    /// A fresh probe nonce.
    pub nonce: Arc<dyn Fn() -> String + Send + Sync>,
    /// `YYYY-MM-DD HH:MM` (local) → unix seconds.
    pub resolve_local: &'a (dyn Fn(&str) -> Option<i64> + Sync),
    /// Where output lines go.
    pub out: &'a (dyn Fn(&str) + Sync),
    /// Sync the proxy after a reap (off in tests).
    pub sync_proxy: bool,
    /// The config FILE, re-read right before a terminate for `REAP_ENABLED` (RS8). `None` =
    /// can't be re-read: never terminates.
    pub config_file: Option<PathBuf>,
}

/// `REAP_ENABLED=1` in the config FILE as it is now (RS8). Unreadable = off.
fn enabled_in_file(env: &ReapEnv<'_>) -> bool {
    let Some(path) = &env.config_file else { return false };
    std::fs::read_to_string(path).map(|t| ReapSettings::from_config(&Config::parse(&t)).is_ok_and(|s| s.enabled)).unwrap_or(false)
}

/// The real environment's pieces.
pub(crate) mod real {
    use super::*;

    pub fn clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(arena_core::snapshot::unix_now)
    }

    /// `timedatectl show -p NTPSynchronized --value` says `yes`.
    pub fn ntp() -> Option<bool> {
        let out = std::process::Command::new("timedatectl").args(["show", "-p", "NTPSynchronized", "--value"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim() == "yes")
    }

    /// The local zone's offset from `date +%z` (`+0200`); 0 if unreadable.
    pub fn tz() -> i64 {
        let Ok(out) = std::process::Command::new("date").arg("+%z").output() else { return 0 };
        parse_tz(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or(0)
    }

    pub fn nonce() -> Arc<dyn Fn() -> String + Send + Sync> {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Arc::new(move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
            probe::nonce_from(t ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9E37_79B9))
        })
    }

    /// `date -d '<text>' +%s` (the text checked to be `YYYY-MM-DD HH:MM` first by the caller).
    pub fn resolve_local(text: &str) -> Option<i64> {
        let out = std::process::Command::new("date").args(["-d", text, "+%s"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok()).flatten()
    }
}

/// `+0200` / `-0430` → seconds.
pub(crate) fn parse_tz(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i64, i64) = (rest[..2].parse().ok()?, rest[2..].parse().ok()?);
    Some(sign * (h * 3600 + m * 60))
}

/// The state dir, required explicit and absolute (G1).
fn store_for(cfg: &Config) -> Result<Store> {
    let dir = arena_core::snapshot::state_dir_explicit(cfg).map_err(config_error)?;
    Ok(Store::new(dir))
}

fn settings_for(cfg: &Config) -> Result<ReapSettings> {
    ReapSettings::from_config(cfg).map_err(config_error)
}

fn prefix(cfg: &Config) -> &str {
    cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena")
}

/// After a state-changing command (RS3): which state file it wrote — the one the reap cron
/// reads (the config file's `ARENA_STATE_DIR`), so a hold or pin is seen landing there.
fn wrote(env: &ReapEnv<'_>, store: &Store) {
    (env.out)(&format!("(state: {})", store.state_path().display()));
}

/// [`wrote`] for the hold, which has its own file next to the state (RR13).
fn wrote_hold(env: &ReapEnv<'_>, store: &Store) {
    (env.out)(&format!("(state: {})", store.hold_path().display()));
}

/// `arena pods reap …`.
pub(crate) async fn handle_reap(cmd: ReapCmd, env: &ReapEnv<'_>) -> Result<()> {
    let json = matches!(&cmd, ReapCmd::Run { json: true, .. } | ReapCmd::Status { json: true, .. } | ReapCmd::Log { json: true, .. });
    if !json {
        return reap_cmd(cmd, env).await;
    }
    // `--json` (RR9): exactly one JSON document on stdout, whatever happens — a command that
    // fails before printing one prints `{"error": …}` (and still fails); every other line goes
    // to stderr.
    let printed = std::sync::atomic::AtomicBool::new(false);
    let (seen, inner) = (&printed, env.out);
    let out = move |l: &str| {
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
        inner(l)
    };
    let quiet = ReapEnv {
        provider: env.provider,
        remote: env.remote.clone(),
        cfg: env.cfg,
        yes: env.yes,
        clock: env.clock.clone(),
        ntp: env.ntp,
        tz: env.tz,
        rsync: env.rsync.clone(),
        nonce: env.nonce.clone(),
        resolve_local: env.resolve_local,
        out: &out,
        sync_proxy: env.sync_proxy,
        config_file: env.config_file.clone(),
    };
    let r = reap_cmd(cmd, &quiet).await;
    if let Err(e) = &r {
        if !printed.load(std::sync::atomic::Ordering::SeqCst) {
            (env.out)(&serde_json::json!({ "error": format!("{e:#}") }).to_string());
        }
    }
    r
}

/// [`handle_reap`]'s commands.
async fn reap_cmd(cmd: ReapCmd, env: &ReapEnv<'_>) -> Result<()> {
    match cmd {
        ReapCmd::Run { apply, rehearse, max, json, sel } => {
            let asked = if apply {
                Mode::Apply
            } else if rehearse {
                Mode::Rehearse
            } else {
                Mode::Observe
            };
            handle_run(env, &sel, asked, max, json).await
        }
        ReapCmd::Set { expire_in, expire_at, extend, idle_after, note, dry_run, sel } => {
            let ask = SetAsk { expire_in, expire_at, extend, idle_after, note };
            handle_set(env, &sel, ask, dry_run).await
        }
        ReapCmd::Clear { expire, idle, dry_run, sel } => handle_clear(env, &sel, expire, idle, dry_run).await,
        ReapCmd::Pin { until, note, sel } => handle_pin(env, &sel, until, note).await,
        ReapCmd::Unpin { sel } => handle_unpin(env, &sel),
        ReapCmd::Status { json, sel } => handle_status(env, &sel, json),
        ReapCmd::Log { tail, pod, json } => handle_log(env, tail, pod.as_deref(), json),
        ReapCmd::Hold { reason } => {
            let store = store_for(env.cfg)?;
            let now = (env.clock)();
            let reason: String = reason.chars().filter(|c| !c.is_control()).take(300).collect();
            // Its own file, under the state lock: works whatever state reap.json is in (RR13).
            store.set_hold(Some(&Hold { reason: reason.clone(), set_at: now, auto: false }))?;
            (env.out)(&format!("reap: on hold — `--apply` runs observe until `arena pods reap release` ({reason})"));
            wrote_hold(env, &store);
            Ok(())
        }
        ReapCmd::Release => {
            let store = store_for(env.cfg)?;
            // The hold file alone (RR13): reap.json may be unreadable. One that can't be read is
            // offered for removal like any hold.
            let h = match store.read_hold() {
                Ok(Some(h)) => h,
                Ok(None) => {
                    (env.out)("reap: no hold");
                    return Ok(());
                }
                Err(e) => {
                    if !super::confirm(env.yes, &format!("The hold file can't be read ({e}).\nRemove it (`--apply` runs act again)?"))? {
                        (env.out)("aborted.");
                        return Ok(());
                    }
                    store.set_hold(None)?;
                    (env.out)("reap: hold released");
                    wrote_hold(env, &store);
                    return Ok(());
                }
            };
            let who = if h.auto { "set by the reaper itself" } else { "set by an operator" };
            let what = format!(
                "The reaper is on hold since {} ({who}): {}\nRelease it (`--apply` runs act again)?",
                render::fmt_local(h.set_at, env.tz),
                h.reason
            );
            if !super::confirm(env.yes, &what)? {
                (env.out)("aborted.");
                return Ok(());
            }
            if !store.clear_hold_if(&h)? {
                anyhow::bail!("the hold changed while you were asked — not released; run `arena pods reap release` again");
            }
            (env.out)("reap: hold released");
            wrote_hold(env, &store);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------
// set / clear / pin / unpin
// ---------------------------------------------------------------------------------------

/// `reap set`'s flags.
#[derive(Debug, Clone, Default)]
pub(crate) struct SetAsk {
    pub expire_in: Option<String>,
    pub expire_at: Option<String>,
    pub extend: Option<String>,
    pub idle_after: Option<String>,
    pub note: Option<String>,
}

/// What `--expire-in/--expire-at/--idle-after` at `up`/`create` resolve to — once, at command
/// start, so every pod of the run gets the same end time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreatePolicy {
    pub expires_at: Option<u64>,
    pub idle_after_secs: Option<u64>,
}

/// Resolve `--expire-in DUR | --expire-at TIME` and `--idle-after DUR` for `up`/`create` at `now`
/// (`None` when none was given). Refused — before anything is created — when the reaper's
/// state dir isn't explicit and absolute, its settings are invalid, or the state file is
/// unreadable.
pub(crate) fn resolve_create_policy(
    cfg: &Config,
    expire_in: Option<&str>,
    expire_at: Option<&str>,
    idle_after: Option<&str>,
    now: u64,
    resolve_local: &dyn Fn(&str) -> Option<i64>,
) -> Result<Option<(CreatePolicy, Store)>> {
    if expire_in.is_none() && expire_at.is_none() && idle_after.is_none() {
        return Ok(None);
    }
    if expire_in.is_some() && expire_at.is_some() {
        anyhow::bail!("pass --expire-in or --expire-at, not both");
    }
    let settings = settings_for(cfg)?;
    let store = store_for(cfg)?;
    store.load().context("the reaper's state file (fix or move it before creating pods with a policy)")?;
    let expires_at = match (expire_in, expire_at) {
        (Some(d), _) => {
            let secs = core_reap::parse_dur(d).map_err(anyhow::Error::msg)?;
            Some(now + core_reap::check_expire_in(secs).map_err(|e| anyhow::anyhow!("--expire-in {e}"))?)
        }
        (None, Some(t)) => Some(core_reap::parse_time(t, now, resolve_local).map_err(|e| anyhow::anyhow!("--expire-at: {e}"))?),
        (None, None) => None,
    };
    let idle_after_secs = idle_after
        .map(|d| core_reap::parse_dur(d).and_then(|s| core_reap::check_idle_after(s, settings.idle_floor_secs)))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    Ok(Some((CreatePolicy { expires_at, idle_after_secs }, store)))
}

/// `up`/`create --expire-in/--expire-at/--idle-after` on a provider the reaper never acts on
/// (RR2): the refusal, before anything is created — the same rule (and words) as `reap set`'s.
/// `provider` = where the pods would be created.
pub(crate) fn create_flags_refusal(cfg: &Config, provider: &str) -> Option<String> {
    let probe = Pod { provider: provider.to_string(), ..Default::default() };
    (!judge::provider_supported(&probe, runpod_v2(cfg))).then(|| {
        format!(
            "refusing --expire-in/--expire-at/--idle-after: pods on {} are never reaped — the reaper only acts on RunPod \
             (RUNPOD_API=v2) and Hetzner pods; nothing was created",
            if provider == "runpod" { "RunPod REST v1" } else { provider }
        )
    })
}

/// Record `policy` for a pod the moment its create returned (`up`/`create`, `--check`
/// replacements included). A failure is said, loudly — the pod exists without the policy the
/// operator asked for — never fatal to the create.
pub(crate) fn record_created(store: &Store, pod: &Pod, policy: CreatePolicy, now: u64, set_by: &str) {
    let rec = PodPolicy::new(pod, policy.expires_at, policy.idle_after_secs, now, set_by, None);
    let key = rec.key();
    match store.update(|s| {
        s.pods.insert(key.clone(), rec.clone());
        Ok(())
    }) {
        Ok(()) => {}
        Err(e) => eprintln!(
            "⚠ {} ({key}) was created but its reap policy could NOT be recorded ({e}) — run `arena pods reap set {} …` once fixed",
            pod.name, pod.name
        ),
    }
}

/// The selection for set/clear/pin: the fleet's listing (every provider's pods), the selector's
/// pick, and the full listing (for the duplicate / twin checks).
async fn pick(env: &ReapEnv<'_>, sel: &Select, verb: &str) -> Result<(Vec<Pod>, Vec<Pod>)> {
    let naming = Naming::from_config(env.cfg);
    let selector = Selector::parse(&sel.args(), &naming)?;
    if selector.is_unscoped() {
        anyhow::bail!("name the pods to {verb} (names, ids or a range like apple..mayor), or pass --all");
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || env.provider.list_pods()).await.context("listing pods")?;
    let picked: Vec<Pod> = selector.resolve(&naming, &pods)?.into_iter().map(|i| pods[i].clone()).collect();
    Ok((picked, pods))
}

/// Why `pod` can never have a reap policy (`None` = it can): off the list, a staff `@` box, a
/// `-old`/`-new` twin, a name two pods share, or a provider the reaper doesn't act on.
fn never_reaped(pod: &Pod, all: &[Pod], naming: &Naming, runpod_v2: bool) -> Option<String> {
    if !naming.reapable(&pod.name) {
        return Some(format!("{} is not one of this cohort's MACHINE_NAME_LIST machines (staff `@` box, twin or off-list)", pod.name));
    }
    if all.iter().filter(|p| p.name == pod.name).count() > 1 {
        return Some(format!("{} is a name two pods share — sort that out first", pod.name));
    }
    let twins = [format!("{}-old", pod.name), format!("{}-new", pod.name)];
    if all.iter().any(|p| twins.contains(&p.name)) {
        return Some(format!("{} has a -old/-new twin listed (a replace/migrate in progress)", pod.name));
    }
    if !judge::provider_supported(pod, runpod_v2) {
        return Some(format!(
            "{} is on {} — the reaper only acts on RunPod (RUNPOD_API=v2) and Hetzner pods",
            pod.name,
            if pod.provider == "runpod" { "RunPod REST v1" } else { &pod.provider }
        ));
    }
    None
}

fn runpod_v2(cfg: &Config) -> bool {
    matches!(arena_core::provider::RunpodApi::from_config(cfg), Ok(arena_core::provider::RunpodApi::V2))
}

/// Split the picked pods into those that may have a policy and the rest: a NAMED one that
/// can't is an error; under --all it's skipped with a count.
fn reapable_only(env: &ReapEnv<'_>, sel: &Select, picked: Vec<Pod>, all: &[Pod]) -> Result<Vec<Pod>> {
    let naming = Naming::from_config(env.cfg);
    let v2 = runpod_v2(env.cfg);
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for p in picked {
        match never_reaped(&p, all, &naming, v2) {
            None => ok.push(p),
            Some(why) if sel.args().names_pods() => anyhow::bail!("{why} — never reaped, so it can't have a reap policy"),
            Some(_) => skipped.push(p.name.clone()),
        }
    }
    if !skipped.is_empty() {
        (env.out)(&format!("note: {} pod(s) skipped — never reaped: {}", skipped.len(), skipped.join(", ")));
    }
    Ok(ok)
}

/// The end time and idle window a `reap set` gives one pod with `old` (its current policy).
fn set_values(ask: &SetAsk, old: Option<&PodPolicy>, now: u64, floor: u64, resolve_local: &dyn Fn(&str) -> Option<i64>) -> Result<(Option<u64>, Option<u64>)> {
    let mut expires = old.and_then(|p| p.expires_at);
    if let Some(d) = &ask.expire_in {
        let secs = core_reap::parse_dur(d).map_err(anyhow::Error::msg)?;
        expires = Some(now + core_reap::check_expire_in(secs).map_err(|e| anyhow::anyhow!("--expire-in {e}"))?);
    }
    if let Some(t) = &ask.expire_at {
        expires = Some(core_reap::parse_time(t, now, resolve_local).map_err(|e| anyhow::anyhow!("--expire-at: {e}"))?);
    }
    if let Some(d) = &ask.extend {
        let secs = core_reap::parse_dur(d).map_err(anyhow::Error::msg)?;
        core_reap::check_expire_in(secs).map_err(|e| anyhow::anyhow!("--extend {e}"))?;
        let from = expires.filter(|e| *e > now).unwrap_or(now);
        let at = from + secs;
        if at - now > core_reap::EXPIRE_MAX {
            anyhow::bail!("--extend: that ends more than {} from now", core_reap::fmt_dur(core_reap::EXPIRE_MAX));
        }
        expires = Some(at);
    }
    let mut idle = old.and_then(|p| p.idle_after_secs);
    if let Some(d) = &ask.idle_after {
        let secs = core_reap::parse_dur(d).map_err(anyhow::Error::msg)?;
        idle = Some(core_reap::check_idle_after(secs, floor).map_err(anyhow::Error::msg)?);
    }
    Ok((expires, idle))
}

/// `at` as the operator reads it: local, UTC and "in Xh Ym".
fn describe_time(at: u64, now: u64, tz: i64) -> String {
    format!(
        "{} {} ({} UTC, in {})",
        render::fmt_local(at, tz),
        render::fmt_tz(tz),
        arena_core::snapshot::rfc3339(at),
        render::fmt_span(at.saturating_sub(now))
    )
}

async fn handle_set(env: &ReapEnv<'_>, sel: &Select, ask: SetAsk, dry_run: bool) -> Result<()> {
    let settings = settings_for(env.cfg)?;
    let store = store_for(env.cfg)?;
    if ask.expire_in.is_none() && ask.expire_at.is_none() && ask.extend.is_none() && ask.idle_after.is_none() && ask.note.is_none() {
        anyhow::bail!("nothing to set: pass --expire-in/--expire-at/--extend and/or --idle-after (or --note)");
    }
    // The state must be readable before anything is listed (a corrupt one: said now).
    store.load()?;
    let (picked, all) = pick(env, sel, "set a reap policy on").await?;
    let pods = reapable_only(env, sel, picked, &all)?;
    if pods.is_empty() {
        (env.out)("(no pods to set a policy on)");
        return Ok(());
    }
    let now = (env.clock)();
    let note = ask.note.clone().map(|n| n.chars().filter(|c| !c.is_control()).take(200).collect::<String>());
    let expiry_touched = ask.expire_in.is_some() || ask.expire_at.is_some() || ask.extend.is_some();
    // Every value from the record as it is NOW, re-read under the state lock (RR7): another
    // `reap set` (an `--extend` from another terminal) that finished while this one was
    // listing the fleet is built on, never overwritten with what this one read before it.
    let decide = |s: &ReapState| -> std::result::Result<Vec<(Pod, Option<u64>, Option<u64>, bool)>, String> {
        let mut plan = Vec::new();
        for p in &pods {
            let old = s.pods.get(&core_reap::key(&p.provider, &p.id));
            let (exp, idle) = set_values(&ask, old, now, settings.idle_floor_secs, env.resolve_local).map_err(|e| e.to_string())?;
            if exp.is_none() && idle.is_none() {
                return Err(format!("{}: a policy needs an end time or an idle window", p.name));
            }
            plan.push((p.clone(), exp, idle, s.pinned(&p.name, &p.id, now)));
        }
        Ok(plan)
    };
    let plan = if dry_run {
        decide(&store.load()?).map_err(anyhow::Error::msg)?
    } else {
        store.update(|s| {
            let plan = decide(s)?;
            for (p, exp, idle, _) in &plan {
                let k = core_reap::key(&p.provider, &p.id);
                let rec = s.pods.entry(k).or_insert_with(|| PodPolicy::new(p, None, None, now, "set", None));
                rec.name = p.name.clone();
                if *exp != rec.expires_at || expiry_touched {
                    rec.first_due_seen = None;
                    rec.warned_soon = false;
                }
                if *idle != rec.idle_after_secs {
                    rec.warned_soon = false;
                }
                rec.expires_at = *exp;
                rec.idle_after_secs = *idle;
                rec.set_at = now;
                rec.set_by = "set".into();
                if note.is_some() {
                    rec.note.clone_from(&note);
                }
            }
            Ok(plan)
        })?
    };
    // What was saved, in words.
    for (p, exp, idle, pinned) in &plan {
        let mut parts = Vec::new();
        if let Some(e) = exp {
            parts.push(format!("expires {}", describe_time(*e, now, env.tz)));
        }
        if let Some(w) = idle {
            parts.push(format!("idle after {}", core_reap::fmt_dur(*w)));
        }
        let mut warn = Vec::new();
        match core_reap::lock_state(p) {
            Some(true) => warn.push("locked — never reaped while locked"),
            None => warn.push("its lock state isn't reported — never reaped until it is"),
            Some(false) => {}
        }
        if *pinned {
            warn.push("pinned — never reaped while pinned");
        }
        let warn = if warn.is_empty() { String::new() } else { format!(" (⚠ {})", warn.join("; ")) };
        (env.out)(&format!("{}{}: {}{warn}", if dry_run { "[dry-run] " } else { "" }, p.name, parts.join(", ")));
    }
    if dry_run {
        (env.out)("\nDry-run only — no policy recorded, nothing written to the pods.");
        return Ok(());
    }
    wrote(env, &store);
    // The participant notice, best effort.
    let state = store.load()?;
    for (p, ..) in &plan {
        let Some(rec) = state.pods.get(&core_reap::key(&p.provider, &p.id)) else { continue };
        write_notice(env, p, Some(rec)).await;
    }
    Ok(())
}

/// The command writing `pod`'s notice of its reap policy (`None`: no state here, or no policy
/// on the pod).
fn notice_command_for(cfg: &Config, pod: &Pod) -> Option<String> {
    let store = Store::new(arena_core::snapshot::state_dir_explicit(cfg).ok()?);
    let state = store.load_if_any().ok()??;
    let p = state.pods.get(&core_reap::key(&pod.provider, &pod.id)).filter(|p| !p.is_empty())?;
    Some(render::notice_write_command(&render::notice_text(&pod.name, p, real::tz())))
}

/// RR11: (re)write the participant notice (`~/.arena/NOTICE` and the interactive-only banner)
/// on `pod` at `target` when it has a reap policy — after setup has put its shell rc files in
/// their final form (a policy given at `up`/`create`; `pods setup`; a restart that reset the
/// container). `Ok(false)` = no policy, nothing written; `Err` = the write failed (said by the
/// caller, never fatal). The only warning the reaper otherwise gives is a `wall`, which
/// reaches nobody in a RunPod container (no utmp).
pub(crate) async fn reassert_notice(remote: &dyn Remote, cfg: &Config, pod: &Pod, target: &SshTarget) -> std::result::Result<bool, String> {
    let Some(cmd) = notice_command_for(cfg, pod) else { return Ok(false) };
    match remote.exec(target, &cmd, Some(arena_core::remote::PROBE_TIMEOUT)).await {
        Ok(o) if o.success => Ok(true),
        Ok(o) => Err(o.exit_label()),
        Err(e) => Err(arena_core::remote::describe_error(&e)),
    }
}

/// [`reassert_notice`] for each of `pods` (running, with an endpoint), one line for the lot.
pub(crate) async fn reassert_notices(remote: &dyn Remote, cfg: &Config, pods: &[Pod]) {
    let mut n = 0;
    for p in pods.iter().filter(|p| !arena_core::status::is_stopped(&p.status)) {
        let Ok(t) = SshTarget::from_pod(p, cfg) else { continue };
        match reassert_notice(remote, cfg, p, &t).await {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(why) => eprintln!("warning: {}: its reap notice wasn't written ({why})", p.name),
        }
    }
    if n > 0 {
        println!("Reap notice (the pod's end time / idle window) written on {n} pod(s) with a reap policy.");
    }
}

/// Write (`Some`) or remove (`None`) a pod's notice, best effort: a stopped pod or one with no
/// endpoint is skipped, a failure is a warning.
async fn write_notice(env: &ReapEnv<'_>, pod: &Pod, policy: Option<&PodPolicy>) {
    if arena_core::status::is_stopped(&pod.status) {
        return;
    }
    let Ok(t) = SshTarget::from_pod(pod, env.cfg) else { return };
    let cmd = match policy {
        Some(p) => render::notice_write_command(&render::notice_text(&pod.name, p, env.tz)),
        None => render::notice_clear_command(),
    };
    match env.remote.exec(&t, &cmd, Some(arena_core::remote::PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {}
        Ok(o) => eprintln!("warning: {}: the notice wasn't updated ({})", pod.name, o.exit_label()),
        Err(e) => eprintln!("warning: {}: the notice wasn't updated ({})", pod.name, arena_core::remote::describe_error(&e)),
    }
}

async fn handle_clear(env: &ReapEnv<'_>, sel: &Select, expire: bool, idle: bool, dry_run: bool) -> Result<()> {
    let store = store_for(env.cfg)?;
    store.load()?;
    let (picked, _) = pick(env, sel, "clear the reap policy of").await?;
    let (both, only_expire, only_idle) = (!expire && !idle, expire && !idle, idle && !expire);
    let what = if both || (expire && idle) {
        "policy"
    } else if only_expire {
        "end time"
    } else if only_idle {
        "idle window"
    } else {
        "policy"
    };
    // Which of the picked pods have a policy, and the change, from the record as it is NOW —
    // re-read under the state lock (RR7).
    let split = |s: &ReapState| -> (Vec<Pod>, Vec<Pod>) { picked.iter().cloned().partition(|p| s.pods.contains_key(&core_reap::key(&p.provider, &p.id))) };
    let (touched, untouched) = if dry_run {
        split(&store.load()?)
    } else {
        store.update(|s| {
            let (touched, untouched) = split(s);
            for p in &touched {
                let k = core_reap::key(&p.provider, &p.id);
                let Some(rec) = s.pods.get_mut(&k) else { continue };
                if both || expire {
                    rec.expires_at = None;
                    rec.first_due_seen = None;
                }
                if both || idle {
                    rec.idle_after_secs = None;
                }
                rec.warned_soon = false;
                if rec.is_empty() && rec.terminating.is_none() {
                    s.pods.remove(&k);
                }
            }
            Ok((touched, untouched))
        })?
    };
    for p in &untouched {
        (env.out)(&format!("{}: no reap policy", p.name));
    }
    for p in &touched {
        (env.out)(&format!("{}{}: {what} cleared", if dry_run { "[dry-run] " } else { "" }, p.name));
    }
    if dry_run || touched.is_empty() {
        return Ok(());
    }
    wrote(env, &store);
    let state = store.load()?;
    for p in &touched {
        let rec = state.pods.get(&core_reap::key(&p.provider, &p.id));
        write_notice(env, p, rec).await;
    }
    Ok(())
}

async fn handle_pin(env: &ReapEnv<'_>, sel: &Select, until: Option<String>, note: Option<String>) -> Result<()> {
    let store = store_for(env.cfg)?;
    let now = (env.clock)();
    let until = until.map(|t| core_reap::parse_time(&t, now, env.resolve_local)).transpose().map_err(|e| anyhow::anyhow!("--until: {e}"))?;
    let (picked, _) = pick(env, sel, "pin").await?;
    let note: Option<String> = note.map(|n| n.chars().filter(|c| !c.is_control()).take(200).collect());
    store.update(|s| {
        for p in &picked {
            s.pins.retain(|x| !(x.name == p.name || x.id.as_deref() == Some(p.id.as_str())));
            s.pins.push(Pin { name: p.name.clone(), id: Some(p.id.clone()), until, note: note.clone(), set_at: now });
        }
        Ok(())
    })?;
    for p in &picked {
        let till = until.map_or("until unpinned".to_string(), |u| format!("until {}", describe_time(u, now, env.tz)));
        (env.out)(&format!("{}: pinned {till} — never reaped meanwhile", p.name));
    }
    wrote(env, &store);
    Ok(())
}

/// The policy/pin names a token matches (full, bare or `@` name, or an id) — for the commands
/// that read only the state.
fn token_matches(naming: &Naming, token: &str, name: &str, id: Option<&str>) -> bool {
    id == Some(token) || name == token || name == arena_core::naming::canonical_name(naming.prefix, naming.list, token)
}

fn handle_unpin(env: &ReapEnv<'_>, sel: &Select) -> Result<()> {
    let store = store_for(env.cfg)?;
    let naming = Naming::from_config(env.cfg);
    let args = sel.args();
    if args.targets.is_empty() && !args.all {
        anyhow::bail!("name the pins to remove (names or ids), or pass --all");
    }
    let removed = store.update(|s| {
        let before = s.pins.len();
        let mut misses = Vec::new();
        if !args.all {
            for t in &args.targets {
                if !s.pins.iter().any(|p| token_matches(&naming, t, &p.name, p.id.as_deref())) {
                    misses.push(t.clone());
                }
            }
            if !misses.is_empty() {
                return Err(format!("no pin matches {}", misses.join(", ")));
            }
            s.pins.retain(|p| !args.targets.iter().any(|t| token_matches(&naming, t, &p.name, p.id.as_deref())));
        } else {
            s.pins.clear();
        }
        Ok(before - s.pins.len())
    })?;
    (env.out)(&format!("unpinned {removed} pin(s)"));
    wrote(env, &store);
    Ok(())
}

fn handle_status(env: &ReapEnv<'_>, sel: &Select, json: bool) -> Result<()> {
    let store = store_for(env.cfg)?;
    let state = store.load()?;
    let naming = Naming::from_config(env.cfg);
    let now = (env.clock)();
    let targets = &sel.targets;
    let wanted = |p: &PodPolicy| targets.is_empty() || targets.iter().any(|t| token_matches(&naming, t, &p.name, Some(&p.id)));
    if json {
        let mut shown = state.clone();
        shown.pods.retain(|_, p| wanted(p));
        (env.out)(&serde_json::to_string_pretty(&shown)?);
        return Ok(());
    }
    (env.out)(&format!("Reap state {} · {} attempt(s) in the last 24h", store.state_path().display(), state.attempts_today(now)));
    if let Some(h) = &state.hold {
        (env.out)(&format!("ON HOLD since {} ({}): {}", render::fmt_local(h.set_at, env.tz), if h.auto { "automatic" } else { "operator" }, h.reason));
    }
    let rows: Vec<Vec<String>> = state
        .pods
        .values()
        .filter(|p| wanted(p))
        .map(|p| {
            let last = p.last.as_ref().map_or("-".into(), |l| format!("{} ({} ago)", l.row.word(), render::fmt_span(now.saturating_sub(l.t))));
            let pinned = if state.pinned(&p.name, &p.id, now) { "pinned" } else { "-" };
            vec![p.name.clone(), p.key(), render::policy_label(p, now, env.tz), render::state_label(p, p.last.as_ref().map_or(Row::StreakShort, |l| l.row), now), pinned.into(), last, p.note.clone().unwrap_or_default()]
        })
        .collect();
    if rows.is_empty() {
        (env.out)("(no reap policies)");
    } else {
        let t = arena_core::table::render(&["NAME", "KEY", "POLICY", "STATE", "PIN", "LAST", "NOTE"], &[], &rows);
        t.lines().for_each(|l| (env.out)(l));
    }
    for p in &state.pins {
        let till = p.until.map_or("until unpinned".into(), |u| format!("until {}", render::fmt_local(u, env.tz)));
        (env.out)(&format!("pin: {} (id {}) {till}{}", p.name, p.id.as_deref().unwrap_or("-"), p.note.as_deref().map(|n| format!(" — {n}")).unwrap_or_default()));
    }
    for (k, t) in &state.reaped {
        let paths = [t.snapshot_dir.as_deref(), t.big_dir.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" + ");
        (env.out)(&format!(
            "reaped: {} ({k}) {}{}{}",
            t.name,
            render::fmt_local(t.at, env.tz),
            if t.recovered { " (recovered)" } else { "" },
            if paths.is_empty() { String::new() } else { format!(" — backup {paths}") }
        ));
    }
    Ok(())
}

fn handle_log(env: &ReapEnv<'_>, tail: usize, pod: Option<&str>, json: bool) -> Result<()> {
    let store = store_for(env.cfg)?;
    let naming = Naming::from_config(env.cfg);
    let lines = store.read_log().with_context(|| format!("reading {}", store.log_path().display()))?;
    let mut picked: Vec<String> = Vec::new();
    for l in lines {
        let parsed: Option<LogLine> = serde_json::from_str(&l).ok();
        if let Some(want) = pod {
            let name = parsed.as_ref().and_then(|p| p.pod.clone()).unwrap_or_default();
            if !token_matches(&naming, want, &name, None) {
                continue;
            }
        }
        picked.push(match (json, parsed) {
            (true, _) | (false, None) => l,
            (false, Some(p)) => format!(
                "{} {:<8} {:<16} {:<24} {}{}",
                render::fmt_local(p.t, env.tz),
                serde_json::to_value(p.mode).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
                p.pod.unwrap_or_else(|| "-".into()),
                p.row.word(),
                p.reasons.iter().map(|r| r.word()).collect::<Vec<_>>().join(","),
                p.trigger.map(|t| format!(" [{}]", t.word())).unwrap_or_default()
            ),
        });
    }
    let start = picked.len().saturating_sub(tail);
    if json {
        // One JSON document (RR9): the lines as an array — a line that isn't JSON as a string.
        let v: Vec<serde_json::Value> =
            picked[start..].iter().map(|l| serde_json::from_str(l).unwrap_or_else(|_| serde_json::Value::String(l.clone()))).collect();
        (env.out)(&serde_json::to_string_pretty(&v)?);
    } else {
        picked[start..].iter().for_each(|l| (env.out)(l));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------------------

/// One probe's outcome.
struct Probed {
    key: String,
    /// The pod's backup lock was busy: no observation this run (R14).
    busy: bool,
    facts: Result<ProbeFacts, ProbeError>,
    jobs: JobsOutcome,
    /// Control clock when it answered.
    at: u64,
}

/// What a probe needs.
#[derive(Clone)]
struct ProbeJob {
    key: String,
    pod: Pod,
    target: SshTarget,
    since: Since,
}

/// The reaper's run context.
struct RunCtx<'a, 'b> {
    env: &'a ReapEnv<'b>,
    settings: ReapSettings,
    store: Store,
    run_id: String,
    mode: Mode,
    naming: Naming<'a>,
    prefix: String,
    /// Where the per-pod backup locks live (`LOCAL_BACKUP_DIR`, when absolute).
    lock_dir: Option<String>,
    repo: String,
    log: std::sync::Mutex<Vec<String>>,
    notes: std::sync::Mutex<Vec<String>>,
    /// Notifications to send: (pod key or None for run-level, kind, line).
    notify: std::sync::Mutex<Vec<(Option<String>, NotifyKind, String)>>,
    attention: std::sync::atomic::AtomicBool,
}

impl RunCtx<'_, '_> {
    fn log(&self, kind: &str, pod: Option<&str>, key: Option<&str>, row: Row, reasons: &[Reason], trigger: Option<Trigger>, paths: Vec<String>) {
        let line = LogLine {
            t: (self.env.clock)(),
            run: self.run_id.clone(),
            mode: self.mode,
            kind: kind.into(),
            pod: pod.map(String::from),
            key: key.map(String::from),
            row,
            reasons: reasons.to_vec(),
            trigger,
            streak_obs: None,
            streak_secs: None,
            paths,
        };
        self.log.lock().unwrap().push(line.to_json());
    }

    fn note(&self, line: String) {
        self.notes.lock().unwrap().push(line);
    }

    fn queue(&self, key: Option<&str>, kind: NotifyKind, pod: Option<&str>, detail: &str) {
        let line = render::notify_line(&self.prefix, kind, pod, detail);
        self.notify.lock().unwrap().push((key.map(String::from), kind, line));
    }

    fn attention(&self) {
        self.attention.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn flush_log(&self) {
        let lines: Vec<String> = std::mem::take(&mut *self.log.lock().unwrap());
        if let Err(e) = self.store.append_log(&lines) {
            eprintln!("warning: can't write the reap log {}: {e}", self.store.log_path().display());
            self.attention();
        }
    }
}

/// Send the queued notifications that their rate limit allows (recorded in the state first),
/// each through `REAP_NOTIFY_CMD` (one line on stdin, 10 s, failures only logged). Unset: the
/// line goes to stderr (the cron log).
async fn send_notifications(ctx: &RunCtx<'_, '_>) {
    let queued: Vec<(Option<String>, NotifyKind, String)> = std::mem::take(&mut *ctx.notify.lock().unwrap());
    if queued.is_empty() {
        return;
    }
    let now = (ctx.env.clock)();
    let allowed = ctx.store.update(|s| {
        let mut out = Vec::new();
        for (key, kind, line) in &queued {
            let last = match key {
                Some(k) => s.pods.get(k).and_then(|p| p.notified.get(kind).copied()),
                None => s.run_notified.get(kind).copied(),
            };
            // A reap or a recovery is always said (the pod's record is gone anyway).
            let always = matches!(kind, NotifyKind::Reaped | NotifyKind::RecoveredReaped | NotifyKind::TerminateFailed | NotifyKind::HoldSet);
            if always || render::may_notify(last, now) {
                match key.as_ref().and_then(|k| s.pods.get_mut(k)) {
                    Some(p) => {
                        p.notified.insert(*kind, now);
                    }
                    None => {
                        s.run_notified.insert(*kind, now);
                    }
                }
                out.push(line.clone());
            }
        }
        Ok(out)
    });
    let lines = match allowed {
        Ok(l) => l,
        // Couldn't record the rate limit: say it all (better twice than never).
        Err(_) => queued.into_iter().map(|(_, _, l)| l).collect(),
    };
    for line in lines {
        notify_one(ctx.settings.notify_cmd.as_deref(), &line).await;
    }
}

/// One notification: `cmd` under `sh -c`, `line` on its stdin, killed after 10 s.
async fn notify_one(cmd: Option<&str>, line: &str) {
    use tokio::io::AsyncWriteExt;
    eprintln!("notify: {line}");
    let Some(cmd) = cmd else { return };
    let spawned = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: REAP_NOTIFY_CMD couldn't start: {e}");
            return;
        }
    };
    let work = async {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{line}\n").as_bytes()).await;
        }
        child.wait().await
    };
    match tokio::time::timeout(Duration::from_secs(core_reap::NOTIFY_TIMEOUT_SECS), work).await {
        Ok(Ok(st)) if st.success() => {}
        Ok(Ok(st)) => eprintln!("warning: REAP_NOTIFY_CMD exited {st}"),
        Ok(Err(e)) => eprintln!("warning: REAP_NOTIFY_CMD failed: {e}"),
        Err(_) => eprintln!("warning: REAP_NOTIFY_CMD timed out after {}s — killed", core_reap::NOTIFY_TIMEOUT_SECS),
    }
}

/// Take a pod's backup lock without waiting (beyond [`store::LOCK_GRACE`]): `Ok(Some(lock))`;
/// `Err(())` busy. With no usable lock dir (or a lock file that can't be opened) there's no
/// lock to take: `Ok(None)` — the probe goes ahead (the reap's backup doesn't: it needs one).
async fn try_pod_lock(lock_dir: Option<&str>, pod: &str) -> std::result::Result<Option<std::fs::File>, ()> {
    let Some(dir) = lock_dir else { return Ok(None) };
    let Ok(f) = arena_core::pull::open_pod_lock(dir, pod) else { return Ok(None) };
    let deadline = tokio::time::Instant::now() + store::LOCK_GRACE;
    loop {
        match arena_core::pull::try_lock(&f) {
            Ok(true) => return Ok(Some(f)),
            Ok(false) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            Ok(false) => return Err(()),
            Err(_) => return Ok(None),
        }
    }
}

/// Probe one pod (spec §4 step 8): its backup lock first (busy ⇒ no observation), then the
/// probe (≤ 90 s) and, if that parsed, the jobs listing (≤ 30 s). Read-only on the pod.
async fn probe_one(remote: Arc<dyn Remote>, job: ProbeJob, nonce: String, repo: String, lock_dir: Option<String>, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Probed {
    let lock = match try_pod_lock(lock_dir.as_deref(), &job.pod.name).await {
        Ok(l) => l,
        Err(()) => return Probed { key: job.key, busy: true, facts: Err(ProbeError::Failed), jobs: JobsOutcome::Failed, at: clock() },
    };
    let (facts, jobs) = probe_pod(remote.as_ref(), &job.target, &nonce, job.since, &repo, &job.pod).await;
    drop(lock);
    Probed { key: job.key, busy: false, facts, jobs, at: clock() }
}

/// The probe and the jobs listing on one pod (no locking — the caller's). The pod's listed
/// creation time goes to the probe: what its coverage checks reach back to (R2-1).
async fn probe_pod(remote: &dyn Remote, t: &SshTarget, nonce: &str, since: Since, repo: &str, pod: &Pod) -> (Result<ProbeFacts, ProbeError>, JobsOutcome) {
    let cmd = probe::probe_command(nonce, since, repo, &pod.provider, pod.created_at);
    let facts = match remote.exec(t, &cmd, Some(Duration::from_secs(core_reap::PROBE_BUDGET_SECS))).await {
        Ok(o) => probe::parse_probe(&o.stdout, nonce, since),
        Err(_) => Err(ProbeError::Failed),
    };
    if facts.is_err() {
        return (facts, JobsOutcome::Failed);
    }
    let jobs = match remote.exec(t, &arena_core::jobs::list_command(), Some(Duration::from_secs(core_reap::JOBS_BUDGET_SECS))).await {
        Ok(o) if o.success => match arena_core::jobs::parse_list(&o.stdout) {
            Ok(list) => JobsOutcome::Read {
                busy: list.iter().filter(|j| matches!(j.state, arena_core::jobs::JobState::Running | arena_core::jobs::JobState::Starting)).count() as u32,
            },
            Err(_) => JobsOutcome::Failed,
        },
        _ => JobsOutcome::Failed,
    };
    (facts, jobs)
}

/// The backup destination (G7): both roots absolute, marked, with the free-space floor; a
/// computable snapshot label; no `BACKUP_REMOTE_PATH`; rsync runnable. `Err` lists what's wrong.
#[derive(Debug, Clone)]
struct Dest {
    snap_root: String,
    big_root: String,
    label: String,
    threshold: String,
    budget: Duration,
}

async fn dest_check(cfg: &Config, settings: &ReapSettings, rsync: &str) -> std::result::Result<Dest, Vec<String>> {
    let mut bad = Vec::new();
    let snap_root = cfg.get("LOCAL_BACKUP_DIR").map(str::trim).filter(|d| !d.is_empty()).unwrap_or("./backup").to_string();
    let big_root = arena_core::pull::big_root(&snap_root, cfg.get("LOCAL_BACKUP_BIG_DIR"));
    for (root, tier) in [(&snap_root, arena_core::pull::Tier::Snapshot), (&big_root, arena_core::pull::Tier::Big)] {
        let p = Path::new(root);
        if !p.is_absolute() {
            bad.push(format!("{} root {root} isn't an absolute path", tier.word()));
            continue;
        }
        if let Err(e) = arena_core::pull::check_tier_marker(p, tier) {
            bad.push(e);
            continue;
        }
        match arena_core::pull::free_bytes(p) {
            Ok(free) if free >= settings.min_free_bytes => {}
            Ok(free) => bad.push(format!(
                "{root}: {:.1} GB free, under REAP_MIN_FREE_GB ({} GB)",
                free as f64 / 1e9,
                settings.min_free_bytes / 1_000_000_000
            )),
            Err(e) => bad.push(format!("{root}: free space unknown ({e})")),
        }
    }
    let label = match super::resolve_week_day(cfg, None, None) {
        Ok((w, d)) => format!("w{w}d{d}"),
        Err(_) => {
            bad.push("ARENA_START_DATE isn't set (the snapshot tier's wNdM label)".into());
            String::new()
        }
    };
    if cfg.get("BACKUP_REMOTE_PATH").is_some_and(|v| !v.trim().is_empty()) {
        bad.push("BACKUP_REMOTE_PATH is set — a reap needs the whole home backed up, not a part".into());
    }
    let budget = match arena_core::pull::backup_timeout(cfg.get("BACKUP_TIMEOUT_SECS")) {
        Ok(b) => b,
        Err(e) => {
            bad.push(e.to_string());
            Duration::ZERO
        }
    };
    match arena_core::remote::run_local(rsync, &["--version".to_string()], "rsync --version", Some(Duration::from_secs(10))).await {
        Ok(o) if o.success => {}
        _ => bad.push(format!("`{rsync} --version` doesn't run")),
    }
    let threshold = cfg.get("BACKUP_MAX_SIZE").filter(|s| !s.is_empty()).unwrap_or("50M").to_string();
    if bad.is_empty() {
        Ok(Dest { snap_root, big_root, label, threshold, budget })
    } else {
        Err(bad)
    }
}

/// The selection for `run`/`status`: names or ids against the policies (no listing needed);
/// none = every policy. A name that matches no policy is an error.
fn run_selection(naming: &Naming, sel: &Select, state: &ReapState) -> Result<Option<Vec<String>>> {
    let args = sel.args();
    if args.targets.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::new();
    for t in &args.targets {
        let hits: Vec<String> = state.pods.iter().filter(|(_, p)| token_matches(naming, t, &p.name, Some(&p.id))).map(|(k, _)| k.clone()).collect();
        if hits.is_empty() {
            anyhow::bail!("no pod with a reap policy matches `{t}` (see `arena pods reap status`)");
        }
        keys.extend(hits);
    }
    Ok(Some(keys))
}

async fn handle_run(env: &ReapEnv<'_>, sel: &Select, asked: Mode, max: Option<u32>, json: bool) -> Result<()> {
    let cfg = env.cfg;
    // G1: config.
    let settings = settings_for(cfg)?;
    let store = store_for(cfg)?;
    let naming = Naming::from_config(cfg);
    let prefix = prefix(cfg).to_string();
    // Apply needs an explicit yes (a terminal's prompt, or --yes): asked before anything else.
    if asked == Mode::Apply
        && !super::confirm(
            env.yes,
            &format!(
                "Reap: terminate up to {} due pod(s) this run, each after a verified, frozen backup (REAP_MAX_PER_RUN / \
                 --max)? To stop it mid-run: `arena pods reap hold` (or REAP_ENABLED=0 in the config file)",
                max.map_or(settings.max_per_run, |m| m.min(settings.max_per_run))
            ),
        )?
    {
        (env.out)(if json { r#"{"aborted":true}"# } else { "aborted." });
        return Ok(());
    }
    // G2: one run at a time.
    let run_lock = match store.try_run_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            let lock = store.dir.join(store::RUN_LOCK).display().to_string();
            if json {
                (env.out)(&serde_json::json!({ "skipped": "another run is in progress", "lock": lock }).to_string());
            } else {
                (env.out)(&format!("reap: another run is in progress ({lock}) — not running"));
            }
            return Ok(());
        }
        Err(e) => return Err(ReapExit { code: 3, why: format!("reap: {e}") }.into()),
    };
    // G3: the state, strictly.
    let state0 = match store.load() {
        Ok(s) => s,
        Err(e) => {
            state_unreadable_notice(&store, &settings, &prefix, &e, (env.clock)()).await;
            return Err(ReapExit { code: 1, why: format!("reap: {e} — nothing done (no probes, no writes)") }.into());
        }
    };
    let now = (env.clock)();
    // G4: the clock.
    let ntp = (env.ntp)();
    let backstep = state0.last_run_at.is_some_and(|l| now + core_reap::CLOCK_BACKSTEP < l);
    let clock_ok = ntp == Some(true) && !backstep;
    // G5/G6: hold, enabled.
    let demoted = match asked {
        Mode::Observe => None,
        _ if state0.hold.is_some() => Some("reaping is on hold".to_string()),
        Mode::Apply if !settings.enabled => Some("reaping disabled in config: REAP_ENABLED isn't 1".to_string()),
        _ if !clock_ok => Some("clock unverified".to_string()),
        _ => None,
    };
    let mode = if demoted.is_some() { Mode::Observe } else { asked };
    let lock_dir = cfg.get("LOCAL_BACKUP_DIR").map(str::trim).filter(|d| Path::new(d).is_absolute()).map(String::from);
    let ctx = RunCtx {
        env,
        settings: settings.clone(),
        store: store.clone(),
        run_id: format!("{now:x}-{}", (env.nonce)().get(..6).unwrap_or("0")),
        mode,
        naming,
        prefix: prefix.clone(),
        lock_dir,
        repo: arena_core::backup::repo_path(cfg),
        log: Default::default(),
        notes: Default::default(),
        notify: Default::default(),
        attention: Default::default(),
    };
    if let Some(d) = &demoted {
        ctx.note(format!("note: --{} asked, running as observe: {d}", if asked == Mode::Apply { "apply" } else { "rehearse" }));
    }
    if !clock_ok {
        let why = match (ntp, backstep) {
            (_, true) => "the clock went back since the last run",
            (Some(false), _) => "the clock isn't NTP-synchronised (timedatectl)",
            _ => "timedatectl couldn't say whether the clock is synchronised",
        };
        ctx.note(format!("clock unverified: {why} — observations only, no actions"));
        ctx.log("event", None, None, Row::ClockUnverified, &[], None, vec![]);
        ctx.queue(None, NotifyKind::ClockUnverified, None, why);
        ctx.attention();
    }
    if let Some(h) = &state0.hold {
        ctx.log("event", None, None, Row::Held, &[], None, vec![]);
        ctx.note(format!("ON HOLD since {}: {} — `arena pods reap release` to act again", render::fmt_local(h.set_at, env.tz), h.reason));
        if asked == Mode::Apply {
            ctx.attention();
        }
    }
    let wanted = run_selection(&ctx.naming, sel, &state0)?;

    // 4: the listing, every provider's own outcome.
    let results = env.provider.list_by_provider().await;
    let mut listing: BTreeMap<String, std::result::Result<Vec<Pod>, String>> = BTreeMap::new();
    for (name, r) in results {
        let r = r.map_err(|e| e.to_string());
        match listing.get(&name) {
            Some(Err(_)) => {}
            _ => {
                listing.insert(name, r);
            }
        }
    }
    let ok_pods: Vec<Pod> = listing.values().filter_map(|r| r.as_ref().ok()).flatten().cloned().collect();
    let look = |p: &str| -> Option<&[Pod]> { listing.get(p).and_then(|r| r.as_ref().ok()).map(Vec::as_slice) };

    // 5–6: reconcile with the listing, tidy, remember the run (saved before any probe: F4).
    let reconciled = store.update(|s| {
        let gone_pins = s.tidy(now);
        let events = judge::reconcile(s, &look, now);
        if clock_ok {
            s.last_run_at = Some(s.last_run_at.map_or(now, |l| l.max(now)));
        }
        Ok((events, gone_pins, s.clone()))
    });
    let (events, gone_pins, state) = match reconciled {
        Ok(v) => v,
        Err(e) => return Err(ReapExit { code: 3, why: format!("reap: can't save the state before acting ({e}) — nothing done") }.into()),
    };
    for p in gone_pins {
        ctx.note(format!("pin on {} ended", p.name));
    }
    for e in &events {
        ctx.log("event", Some(&e.name), Some(&e.key), e.row, &[], None, vec![]);
        match e.row {
            Row::RecoveredReaped => ctx.queue(None, NotifyKind::RecoveredReaped, Some(&e.name), "found gone with a reap of it pending (the run that began it was interrupted) — check its backup under LOCAL_BACKUP_DIR"),
            Row::StillBilling => {
                ctx.queue(None, NotifyKind::StillBilling, Some(&e.name), "still listed and billing after it was terminated");
                ctx.attention();
            }
            _ => {}
        }
    }
    if state.hold.is_some() && state0.hold.is_none() {
        ctx.queue(None, NotifyKind::HoldSet, None, "automatic hold: a reaped pod is still billing (provider inconsistency)");
        ctx.attention();
    }
    for (name, r) in &listing {
        if let Err(e) = r {
            let n = state.pods.values().filter(|p| &p.provider == name).count();
            ctx.note(format!("{name}: listing failed ({}) — its {n} pod(s) with a policy untouched", arena_core::fleet::clip(e, 120)));
        }
    }

    // 7: the gates before any SSH.
    let gate_ctx = GateCtx { naming: &ctx.naming, runpod_v2: runpod_v2(cfg), pins: &state.pins, listed: &ok_pods, now };
    let mut decided: BTreeMap<String, Decision> = BTreeMap::new();
    let mut jobs: Vec<ProbeJob> = Vec::new();
    for (k, pol) in &state.pods {
        if wanted.as_ref().is_some_and(|w| !w.contains(k)) {
            continue;
        }
        let listed = match listing.get(&pol.provider) {
            None => Listed::NotQueried,
            Some(Err(_)) => Listed::Failed,
            Some(Ok(pods)) => pods.iter().find(|p| p.id == pol.id).map_or(Listed::Gone, Listed::Pod),
        };
        match pre_ssh_gate(pol, listed, &gate_ctx) {
            Ok(pod) => match SshTarget::from_pod(pod, cfg) {
                Ok(target) => jobs.push(ProbeJob { key: k.clone(), pod: pod.clone(), target, since: judge::plan_since(pol, now) }),
                Err(_) => {
                    decided.insert(k.clone(), Decision::of(Row::NoEndpoint));
                }
            },
            Err(row) => {
                let mut d = Decision::of(row);
                if judge::overdue(pol, now) {
                    d.notify = match row {
                        Row::NotRunning => Some(NotifyKind::OverdueNotRunning),
                        Row::NoEndpoint => Some(NotifyKind::OverdueNoEndpoint),
                        _ => None,
                    };
                }
                decided.insert(k.clone(), d);
            }
        }
    }
    let unpoliced = ok_pods.iter().filter(|p| ctx.naming.reapable(&p.name) && !state.pods.contains_key(&core_reap::key(&p.provider, &p.id))).count();

    // 8: probe, at most 8 at once.
    let probed = probe_all(&ctx, jobs).await;

    // 9–10: classify and decide.
    let mut policies: BTreeMap<String, PodPolicy> = BTreeMap::new();
    let mut facts_of: HashMap<String, ProbeFacts> = HashMap::new();
    for p in probed {
        let Some(pol) = state.pods.get(&p.key) else { continue };
        if p.busy {
            decided.insert(p.key.clone(), Decision::of(Row::BackupBusy));
            continue;
        }
        let mut pol = pol.clone();
        let cctx = ClassifyCtx { provider: &pol.provider, listed_id: &pol.id, now: p.at, clock_ok, baseline: pol.proc_baseline.as_ref() };
        let obs = classify(pol.observations.last(), &p.facts, p.jobs, &cctx);
        pol.proc_baseline = judge::next_baseline(pol.proc_baseline.as_ref(), &p.facts);
        // The expiry is first "seen" only by a verified clock, through a probe that was read and
        // whose clock agreed with ours (Idle or Active — every other outcome is Unknown).
        let clock_fine = matches!(obs.verdict, Verdict::Idle | Verdict::Active);
        if clock_ok && clock_fine && pol.expires_at.is_some_and(|e| e <= p.at) && pol.first_due_seen.is_none() {
            pol.first_due_seen = Some(p.at);
        }
        if obs.verdict != Verdict::Idle && pol.idle_after_secs.is_some() && pol.expires_at.is_none_or(|e| e > p.at + core_reap::WARN_LEAD) {
            pol.warned_soon = false;
        }
        let verdict = obs.verdict;
        pol.observations.push(obs);
        let d = if verdict == Verdict::Idle {
            due_row(&pol, p.at, mode)
        } else {
            observation_row(pol.observations.last().expect("just pushed"), judge::overdue(&pol, p.at))
        };
        if let Ok(f) = p.facts {
            facts_of.insert(p.key.clone(), f);
        }
        decided.insert(p.key.clone(), d);
        policies.insert(p.key, pol);
    }

    // G7 for whatever is due; R29 caps for what may act.
    let due: Vec<String> = decided.iter().filter(|(_, d)| matches!(d.row, Row::Candidate | Row::WouldReap)).map(|(k, _)| k.clone()).collect();
    let mut dest: Option<Dest> = None;
    if !due.is_empty() {
        match dest_check(cfg, &settings, &env.rsync).await {
            Ok(d) => dest = Some(d),
            Err(bad) => {
                let why = bad.join("; ");
                ctx.note(format!("backup destination unhealthy: {why}"));
                ctx.queue(None, NotifyKind::DestUnhealthy, None, &arena_core::fleet::clip(&why, 300));
                ctx.attention();
                for k in &due {
                    if let Some(d) = decided.get_mut(k) {
                        d.row = Row::DestUnhealthy;
                    }
                }
            }
        }
    }
    let mut candidates: Vec<(String, Trigger, Option<u64>, u64)> = Vec::new();
    for (k, d) in &decided {
        if d.row == Row::Candidate {
            let pol = &policies[k];
            let span = judge::idle_streak(&pol.observations).map_or(0, |s| s.span());
            candidates.push((k.clone(), d.trigger.unwrap_or(Trigger::Idle), pol.expires_at, span));
        }
    }
    let ordered = order_candidates(candidates);
    let used_today = state.attempts_today(now);
    let budget = caps_left(&settings, max, 0, if mode == Mode::Apply { used_today } else { 0 }) as usize;
    let (acting, capped): (Vec<String>, Vec<String>) = (ordered.iter().take(budget).cloned().collect(), ordered.iter().skip(budget).cloned().collect());
    for k in &capped {
        if let Some(d) = decided.get_mut(k) {
            d.row = Row::Capped;
        }
    }

    // Notifications the decisions carry (overdue ones).
    for (k, d) in &decided {
        if let Some(kind) = d.notify {
            let name = state.pods.get(k).map(|p| p.name.clone()).unwrap_or_default();
            ctx.queue(Some(k), kind, Some(&name), &render::why_label(d.row, &d.reasons));
        }
    }

    // 11: merge this run's observations and decisions into the state (re-loaded strictly).
    let merge_now = (env.clock)();
    let merged = store.update(|s| {
        for (k, d) in &decided {
            let Some(rec) = s.pods.get_mut(k) else { continue };
            if let Some(p) = policies.get(k) {
                if let Some(o) = p.observations.last() {
                    rec.observations.push(o.clone());
                }
                if rec.first_due_seen.is_none() && rec.expires_at == p.expires_at {
                    rec.first_due_seen = p.first_due_seen;
                }
                if !p.warned_soon {
                    rec.warned_soon = false;
                }
                rec.proc_baseline.clone_from(&p.proc_baseline);
            }
            rec.last = Some(core_reap::LastDecision { t: merge_now, row: d.row, reasons: d.reasons.clone() });
        }
        s.tidy(merge_now);
        Ok(s.clone())
    });
    let state = match merged {
        Ok(s) => s,
        Err(e) => return Err(ReapExit { code: 3, why: format!("reap: can't save this run's observations ({e}) — nothing acted on") }.into()),
    };
    for (k, d) in &decided {
        let name = state.pods.get(k).map(|p| p.name.clone());
        ctx.log("eval", name.as_deref(), Some(k), d.row, &d.reasons, d.trigger, vec![]);
    }
    ctx.flush_log();

    // The one participant warning (apply mode only): entering the last hour.
    if mode == Mode::Apply {
        warn_soon(&ctx, &state, &decided, &ok_pods).await;
    }

    // 12: act.
    let mut outcomes: BTreeMap<String, Row> = BTreeMap::new();
    let mut reaped_any = false;
    if matches!(mode, Mode::Apply | Mode::Rehearse) {
        if let Some(dest) = dest.as_ref() {
            for k in &acting {
                let Some(pol) = policies.get(k).cloned() else { continue };
                let Some(pod) = ok_pods.iter().find(|p| core_reap::key(&p.provider, &p.id) == *k).cloned() else { continue };
                let trigger = decided.get(k).and_then(|d| d.trigger).unwrap_or(Trigger::Idle);
                let act = act_one(&ctx, dest, pol, pod, trigger, facts_of.get(k).map(|f| f.repo), clock_ok).await;
                match act {
                    Act::Done(row) => {
                        reaped_any |= row == Row::Reaped;
                        outcomes.insert(k.clone(), row);
                    }
                    Act::Abort(row, why) => {
                        outcomes.insert(k.clone(), row);
                        ctx.note(format!("reap stopped: {why}"));
                        ctx.attention();
                        break;
                    }
                }
                ctx.flush_log();
            }
        }
    }
    if reaped_any && env.sync_proxy {
        // Its report line on stderr under --json: stdout is the one JSON document.
        if json {
            super::sync_proxy_to_stderr(cfg, env.provider, "reap").await;
        } else {
            super::sync_proxy(cfg, env.provider, "reap").await;
        }
    }
    send_notifications(&ctx).await;
    ctx.flush_log();

    // 13: the table.
    let final_state = store.load().unwrap_or(state);
    let mut rows: Vec<PlanRow> = Vec::new();
    for (k, d) in &decided {
        let row = outcomes.get(k).copied().unwrap_or(d.row);
        let pol = policies.get(k).or_else(|| final_state.pods.get(k)).or_else(|| state0.pods.get(k));
        let Some(pol) = pol else { continue };
        let reasons = if outcomes.contains_key(k) { Vec::new() } else { d.reasons.clone() };
        rows.push(PlanRow {
            name: pol.name.clone(),
            key: k.clone(),
            policy: render::policy_label(pol, now, env.tz),
            state: render::state_label(pol, row, now),
            row,
            next: render::next_label(row).to_string(),
            why: render::why_label(row, &reasons),
        });
    }
    let mut footer: Vec<String> = std::mem::take(&mut *ctx.notes.lock().unwrap());
    if unpoliced > 0 {
        footer.push(format!("{unpoliced} cohort pod(s) without a reap policy (never probed)"));
    }
    let header = PlanHeader {
        now,
        tz: env.tz,
        mode,
        demoted: demoted.clone(),
        max_per_run: max.map_or(settings.max_per_run, |m| m.min(settings.max_per_run)),
        max_per_day: settings.max_per_day,
        used_today: final_state.attempts_today((env.clock)()),
        state_path: store.state_path().display().to_string(),
    };
    if json {
        let v = serde_json::json!({
            "mode": mode,
            "demoted": demoted,
            "clock_ok": clock_ok,
            "rows": rows,
            "notes": footer,
        });
        (env.out)(&serde_json::to_string_pretty(&v)?);
    } else {
        render::render_plan(&header, &rows, &footer).lines().for_each(|l| (env.out)(l));
    }
    drop(run_lock);
    let attention = ctx.attention.load(std::sync::atomic::Ordering::SeqCst)
        || outcomes.values().any(|r| matches!(r, Row::GitFailed | Row::RsyncFailed | Row::TerminateFailed | Row::DestUnhealthy | Row::FreezeFailed))
        || decided.values().any(|d| d.row == Row::DestUnhealthy);
    if attention {
        return Err(ReapExit { code: 3, why: "reap: something needs attention (see above and `arena pods reap log`)".into() }.into());
    }
    Ok(())
}

/// The state file couldn't be read: say so through the notify command, at most every 6 h
/// (a sidecar stamp, since the state itself can't hold it).
async fn state_unreadable_notice(store: &Store, settings: &ReapSettings, prefix: &str, e: &StoreError, now: u64) {
    let stamp = store.dir.join("reap-notify-state.stamp");
    let last = std::fs::read_to_string(&stamp).ok().and_then(|t| t.trim().parse::<u64>().ok());
    if render::may_notify(last, now) {
        let _ = std::fs::write(&stamp, now.to_string());
        let line = render::notify_line(prefix, NotifyKind::StateUnreadable, None, &arena_core::fleet::clip(&e.to_string(), 300));
        notify_one(settings.notify_cmd.as_deref(), &line).await;
    }
}

/// Probe every job, at most [`core_reap::PROBE_CONCURRENCY`] at once.
async fn probe_all(ctx: &RunCtx<'_, '_>, jobs: Vec<ProbeJob>) -> Vec<Probed> {
    let sem = Arc::new(tokio::sync::Semaphore::new(core_reap::PROBE_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    let mut keys = HashMap::new();
    for job in jobs {
        let (remote, clock, sem) = (ctx.env.remote.clone(), ctx.env.clock.clone(), sem.clone());
        let (nonce, repo, lock_dir) = ((ctx.env.nonce)(), ctx.repo.clone(), ctx.lock_dir.clone());
        let key = job.key.clone();
        let id = set
            .spawn(async move {
                let _permit = sem.acquire_owned().await;
                probe_one(remote, job, nonce, repo, lock_dir, clock).await
            })
            .id();
        keys.insert(id, key);
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next_with_id().await {
        match joined {
            Ok((_, p)) => out.push(p),
            // A crashed probe: no reply — `Unknown(ProbeFailed)`, never silently dropped.
            Err(e) => {
                if let Some(k) = keys.remove(&e.id()) {
                    out.push(Probed { key: k, busy: false, facts: Err(ProbeError::Failed), jobs: JobsOutcome::Failed, at: (ctx.env.clock)() });
                }
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// The one `wall` (apply mode only, best effort) as a pod enters its last hour before a reap:
/// an expiry within [`core_reap::WARN_LEAD`], or an idle streak within that of its window.
async fn warn_soon(ctx: &RunCtx<'_, '_>, state: &ReapState, decided: &BTreeMap<String, Decision>, pods: &[Pod]) {
    let now = (ctx.env.clock)();
    let mut warned = Vec::new();
    for (k, d) in decided {
        if !matches!(d.row, Row::StreakShort | Row::Scheduled | Row::ExpiryConfirming | Row::ExpiryWaitQuiet | Row::Candidate) {
            continue;
        }
        let Some(pol) = state.pods.get(k) else { continue };
        if pol.warned_soon {
            continue;
        }
        let soon = match (pol.expires_at, pol.idle_after_secs) {
            (Some(e), _) if e <= now + core_reap::WARN_LEAD => Some((Trigger::Expiry, e.saturating_sub(now))),
            (_, Some(w)) => judge::idle_streak(&pol.observations)
                .filter(|s| s.span() + core_reap::WARN_LEAD >= w)
                .map(|s| (Trigger::Idle, w.saturating_sub(s.span()))),
            _ => None,
        };
        let Some((trigger, left)) = soon else { continue };
        let Some(pod) = pods.iter().find(|p| core_reap::key(&p.provider, &p.id) == *k) else { continue };
        let Ok(t) = SshTarget::from_pod(pod, ctx.env.cfg) else { continue };
        let _ = ctx.env.remote.exec(&t, &render::wall_command(&pod.name, trigger, left.max(60)), Some(arena_core::remote::PROBE_TIMEOUT)).await;
        warned.push(k.clone());
    }
    if !warned.is_empty() {
        let _ = ctx.store.update(|s| {
            for k in &warned {
                if let Some(p) = s.pods.get_mut(k) {
                    p.warned_soon = true;
                }
            }
            Ok(())
        });
    }
}

/// How acting on one pod ended.
enum Act {
    /// Its row (Reaped, a failure, a refusal…); go on with the next.
    Done(Row),
    /// Stop acting this run (the state couldn't be saved: no write-ahead, no action).
    Abort(Row, String),
}

/// Clear a pod's pending terminate (after a failure that ends its attempt), and optionally
/// set its backoff / record an observation.
fn end_attempt(ctx: &RunCtx<'_, '_>, key: &str, retry_after: Option<u64>, obs: Option<Observation>) {
    let r = ctx.store.update(|s| {
        if let Some(p) = s.pods.get_mut(key) {
            p.terminating = None;
            if retry_after.is_some() {
                p.retry_after = retry_after;
            }
            if let Some(o) = &obs {
                p.observations.push(o.clone());
            }
        }
        Ok(())
    });
    if let Err(e) = r {
        eprintln!("warning: couldn't record the end of {key}'s reap attempt: {e}");
        ctx.attention();
    }
}

/// Spec §4 step 12 for one candidate: fresh look → write-ahead → backup → quiet → (rehearse
/// stops) → re-list + state re-read → ticket → terminate.
async fn act_one(ctx: &RunCtx<'_, '_>, dest: &Dest, mut pol: PodPolicy, pod: Pod, trigger: Trigger, repo: Option<RepoSite>, clock_ok: bool) -> Act {
    let env = ctx.env;
    let key = pol.key();
    let name = pol.name.clone();
    let Ok(target) = SshTarget::from_pod(&pod, env.cfg) else { return Act::Done(Row::NoEndpoint) };
    let cond = core_reap::identity_condition(&pod.provider, &pod.id);
    let Some(cond) = cond else { return Act::Done(Row::ProviderUnsupported) };
    // The pod's backup lock, for the whole attempt: our own re-probe, backup and post-backup
    // probe must not overlap another backup's SSH sessions (they'd read as activity), and the
    // */15 cron mustn't start one on a pod about to go.
    let lock = match try_pod_lock(Some(&dest.snap_root), &name).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            ctx.log("act", Some(&name), Some(&key), Row::BackupBusy, &[], Some(trigger), vec![]);
            return Act::Done(Row::BackupBusy);
        }
        Err(()) => {
            ctx.log("act", Some(&name), Some(&key), Row::BackupBusy, &[], Some(trigger), vec![]);
            return Act::Done(Row::BackupBusy);
        }
    };
    // a. The deciding look is taken immediately before the backup (RS4): one older than
    // OBS_FRESH (60 s) is taken again, under this pod's lock.
    let mut repo = repo;
    let mut now = (env.clock)();
    if pol.observations.last().is_none_or(|o| now.saturating_sub(o.t) > core_reap::OBS_FRESH) {
        let since = judge::plan_since(&pol, now);
        let nonce = (env.nonce)();
        let (facts, jobs) = probe_pod(env.remote.as_ref(), &target, &nonce, since, &ctx.repo, &pod).await;
        now = (env.clock)();
        let cctx = ClassifyCtx { provider: &pod.provider, listed_id: &pod.id, now, clock_ok, baseline: pol.proc_baseline.as_ref() };
        let obs = classify(pol.observations.last(), &facts, jobs, &cctx);
        repo = facts.as_ref().ok().map(|f| f.repo);
        pol.proc_baseline = judge::next_baseline(pol.proc_baseline.as_ref(), &facts);
        pol.observations.push(obs.clone());
        end_attempt(ctx, &key, None, Some(obs.clone()));
        let baseline = pol.proc_baseline.clone();
        let _ = ctx.store.update(|s| {
            if let Some(p) = s.pods.get_mut(&key) {
                p.proc_baseline.clone_from(&baseline);
            }
            Ok(())
        });
        if obs.verdict != Verdict::Idle {
            ctx.log("act", Some(&name), Some(&key), Row::ReprobeNotIdle, &obs.reasons, Some(trigger), vec![]);
            return Act::Done(Row::ReprobeNotIdle);
        }
    }
    let decision = due_row(&pol, now, ctx.mode);
    let proof = match TriggerProof::from_decision(&pol, &decision, now) {
        Ok(p) => p,
        Err(row) => {
            ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
            return Act::Done(row);
        }
    };
    let trigger = proof.trigger();
    // b. The write-ahead (apply only): durable before anything is done.
    let state_now = match ctx.store.load() {
        Ok(s) => s,
        Err(e) => return Act::Abort(Row::StateUnreadable, e.to_string()),
    };
    let run_gate = if ctx.mode == Mode::Apply {
        let inputs = RunInputs {
            mode: ctx.mode,
            enabled: ctx.settings.enabled,
            held: state_now.hold.is_some(),
            clock_ok,
            dest_ok: true,
            caps_left: caps_left(&ctx.settings, None, 0, state_now.attempts_today(now)),
        };
        match RunGate::check(&inputs) {
            Ok(g) => Some(g),
            Err(row) => {
                ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
                return Act::Done(row);
            }
        }
    } else {
        None
    };
    let wal = if ctx.mode == Mode::Apply {
        match store::write_ahead(&ctx.store, &key, &ctx.run_id, trigger, (env.clock)()) {
            Ok(w) => {
                ctx.log("act", Some(&name), Some(&key), Row::Candidate, &[], Some(trigger), vec![]);
                ctx.flush_log();
                Some(w)
            }
            Err(e) => return Act::Abort(Row::StateUnreadable, format!("the write-ahead for {name} couldn't be saved ({e})")),
        }
    } else {
        None
    };
    // c. The backup.
    let started = (env.clock)();
    let snapshot_dir = arena_core::pull::local_dest(&dest.snap_root, &dest.label, &name);
    let big_dir = arena_core::pull::big_dest_in(&dest.big_root, &name);
    let ev = backup(ctx, dest, &target, &pod, &cond, repo, started, &snapshot_dir, &big_dir, proof.container(), ctx.mode == Mode::Rehearse).await;
    let backup_proof = match BackupProof::check(&ev) {
        Ok(b) => b,
        Err(f) => {
            let (retry, obs) = match f.row {
                // A failed freeze too (R2-3): it would fail the same way next tick, after the
                // pod's whole backup again and another write-ahead against the daily cap.
                Row::GitFailed | Row::RsyncFailed | Row::FreezeFailed => (Some((env.clock)() + core_reap::BACKUP_FAIL_RETRY), None),
                // Files still changing: that's activity (the streak restarts).
                Row::VerifyPending if f.pending.is_some_and(|n| n > 0) => (None, Some(Observation {
                    t: (env.clock)(),
                    clock_ok,
                    container: Some(proof.container().to_string()),
                    pod_t: None,
                    verdict: Verdict::Active,
                    reasons: vec![Reason::Files],
                    cpu_usec: None,
                    net_bytes: None,
                    counters_t: None,
                    scan_from: None,
                })),
                _ => (None, None),
            };
            end_attempt(ctx, &key, retry, obs);
            match f.row {
                Row::FreezeFailed => {
                    ctx.queue(Some(&key), NotifyKind::FreezeFailed, Some(&name), "the verified backup couldn't be frozen into reaped/ — not reaped; retried in 6h");
                    ctx.attention();
                }
                Row::GitFailed => ctx.queue(Some(&key), NotifyKind::GitFailed, Some(&name), "the reap's git backup failed — not reaped; retried in 6h"),
                Row::RsyncFailed => ctx.queue(Some(&key), NotifyKind::RsyncFailed, Some(&name), "the reap's file backup failed — not reaped; retried in 6h"),
                Row::DestUnhealthy => ctx.queue(None, NotifyKind::DestUnhealthy, Some(&name), "backup destination unhealthy at the reap's backup"),
                _ => {}
            }
            ctx.log("act", Some(&name), Some(&key), f.row, &[], Some(trigger), vec![snapshot_dir, big_dir]);
            return Act::Done(f.row);
        }
    };
    // d. Quiet now: the same pod and container, and nothing changed since the DECIDING look's
    // scan started (RS4 — so the stretch between that look and the backup is covered too).
    let nonce = (env.nonce)();
    let (facts, jobs) = probe_pod(env.remote.as_ref(), &target, &nonce, Since::At(proof.scan_from()), &ctx.repo, &pod).await;
    let quiet_at = (env.clock)();
    let qctx = QuietCtx { provider: &pod.provider, id: &pod.id, container: proof.container(), now: quiet_at, baseline: pol.proc_baseline.as_ref() };
    let quiet = match quiet_now(&facts, jobs, &qctx) {
        Ok(q) => q,
        Err(reasons) => {
            end_attempt(ctx, &key, None, None);
            ctx.log("act", Some(&name), Some(&key), Row::PostBackupNotQuiet, &reasons, Some(trigger), vec![]);
            return Act::Done(Row::PostBackupNotQuiet);
        }
    };
    // e. A rehearsal stops here.
    let (Some(run_gate), Some(wal)) = (run_gate, wal) else {
        ctx.log("act", Some(&name), Some(&key), Row::Rehearsed, &[], Some(trigger), vec![backup_proof.snapshot_dir.clone(), backup_proof.big_dir.clone()]);
        return Act::Done(Row::Rehearsed);
    };
    // f. Re-list this provider; re-read the state.
    let results: Vec<(String, std::result::Result<Vec<Pod>, String>)> =
        env.provider.list_by_provider().await.into_iter().map(|(n, r)| (n, r.map_err(|e| e.to_string()))).collect();
    let listed_at = (env.clock)();
    let all_ok: Vec<Pod> = results.iter().filter_map(|(_, r)| r.as_ref().ok()).flatten().cloned().collect();
    // R2-5: any provider whose listing failed may hide a `-new` twin — no terminate.
    let partial = results.iter().any(|(_, r)| r.is_err());
    let listed = match results.iter().find(|(n, _)| *n == pod.provider).map(|(_, r)| r) {
        None => Listed::NotQueried,
        Some(Err(_)) => Listed::Failed,
        Some(Ok(pods)) => pods.iter().find(|p| p.id == pod.id).map_or(Listed::Gone, Listed::Pod),
    };
    let relist = match relist_verdict(listed, &all_ok, partial, &key, &name, listed_at, (env.clock)()) {
        Ok(r) => r,
        Err(row) => {
            if row == Row::GoneBeforeReap {
                let _ = ctx.store.update(|s| {
                    s.pods.remove(&key);
                    s.reaped.insert(key.clone(), tombstone(&name, trigger, &backup_proof, (env.clock)(), false));
                    Ok(())
                });
            } else {
                end_attempt(ctx, &key, None, None);
            }
            ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
            return Act::Done(row);
        }
    };
    let reread = match ctx.store.load() {
        Ok(s) => s,
        Err(e) => {
            end_attempt(ctx, &key, None, None);
            return Act::Abort(Row::StateUnreadable, e.to_string());
        }
    };
    let recheck = match recheck_state(&reread, &key, trigger, &pol.observations, (env.clock)()) {
        Ok(r) => r,
        Err(row) => {
            end_attempt(ctx, &key, None, None);
            ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
            return Act::Done(row);
        }
    };
    // RS8: the switch, re-read from the config FILE right now — `REAP_ENABLED=0` set during a
    // long backup stops this terminate (as `reap hold` does, through the re-read state).
    let run_gate = match run_gate.still(enabled_in_file(env), reread.hold.is_some()) {
        Ok(g) => g,
        Err(row) => {
            end_attempt(ctx, &key, None, None);
            ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
            return Act::Done(row);
        }
    };
    // g. The ticket — the only way to the terminate.
    let ticket = match ReapTicket::issue(run_gate, proof, wal, backup_proof.clone(), quiet, relist, recheck) {
        Ok(t) => t,
        Err(row) => {
            end_attempt(ctx, &key, None, None);
            ctx.log("act", Some(&name), Some(&key), row, &[], Some(trigger), vec![]);
            return Act::Done(row);
        }
    };
    let outcome = tokio::time::timeout(Duration::from_secs(120), ticket.terminate(env.provider)).await;
    drop(lock);
    let now = (env.clock)();
    let paths = vec![backup_proof.snapshot_dir.clone(), backup_proof.big_dir.clone()];
    match outcome {
        Ok(Ok(())) => {
            let saved = ctx.store.update(|s| {
                s.pods.remove(&key);
                s.reaped.insert(key.clone(), tombstone(&name, trigger, &backup_proof, now, false));
                Ok(())
            });
            if let Err(e) = saved {
                // I14: a failed post-terminate save — the write-ahead record still names the
                // pod (the next run finds it gone: recovered). Say so; try to hold.
                eprintln!("warning: {name} was terminated but the state couldn't be saved: {e}");
                let _ = ctx.store.set_hold(Some(&Hold { reason: format!("the state couldn't be saved after reaping {name}"), set_at: now, auto: true }));
                ctx.queue(None, NotifyKind::HoldSet, Some(&name), "terminated, but the state couldn't be saved — hold set");
                ctx.attention();
            }
            let branch = backup_proof.branch.as_deref().map(|b| format!(", branch {b}")).unwrap_or_default();
            ctx.queue(
                None,
                NotifyKind::Reaped,
                Some(&name),
                &format!(
                    "{} reap: terminated after a verified backup — {} + {}{branch}; revoke its key: arena keys revoke {name}",
                    trigger.word(),
                    paths[0],
                    paths[1]
                ),
            );
            ctx.log("act", Some(&name), Some(&key), Row::Reaped, &[], Some(trigger), paths);
            Act::Done(Row::Reaped)
        }
        Ok(Err(e)) if e.is_locked() => {
            end_attempt(ctx, &key, None, None);
            ctx.log("act", Some(&name), Some(&key), Row::TerminateRefusedLocked, &[], Some(trigger), vec![]);
            Act::Done(Row::TerminateRefusedLocked)
        }
        other => {
            // Outcome unknown (an error, or no answer): hold, keep the write-ahead (the next run
            // reconciles it), never retry.
            let why = match other {
                Ok(Err(e)) => arena_core::fleet::clip(&e.to_string(), 200),
                _ => "no answer within 120s".to_string(),
            };
            let _ = ctx.store.set_hold(Some(&Hold { reason: format!("terminating {name} failed: {why}"), set_at: now, auto: true }));
            ctx.queue(None, NotifyKind::TerminateFailed, Some(&name), &format!("terminate failed ({why}) — hold set; check the pod by hand"));
            ctx.log("act", Some(&name), Some(&key), Row::TerminateFailed, &[], Some(trigger), vec![]);
            ctx.attention();
            Act::Done(Row::TerminateFailed)
        }
    }
}

fn tombstone(name: &str, trigger: Trigger, b: &BackupProof, at: u64, recovered: bool) -> Tombstone {
    Tombstone {
        at,
        name: name.to_string(),
        trigger: Some(trigger),
        snapshot_dir: Some(b.snapshot_dir.clone()),
        big_dir: Some(b.big_dir.clone()),
        branch: b.branch.clone(),
        recovered,
        seen_after: 0,
    }
}

/// A finished local or remote step as a [`StepEnd`].
fn step_end(r: arena_core::Result<arena_core::ssh::SshOutput>) -> StepEnd {
    match r {
        Ok(o) => match o.code {
            Some(code) => StepEnd::Exit { code, stdout: o.stdout },
            None => StepEnd::Killed,
        },
        Err(arena_core::Error::Timeout { .. }) => StepEnd::Timeout,
        Err(_) => StepEnd::Error,
    }
}

/// The reap's backup (spec §4 12c), every pod-side step behind the identity guard `cond`:
/// free space and markers, the git push (unless the pod has no repo), the snapshot tier's
/// passes, the big tier, the big tier's dry-run verify — each within what's left of
/// `BACKUP_TIMEOUT_SECS`.
#[allow(clippy::too_many_arguments)]
async fn backup(
    ctx: &RunCtx<'_, '_>,
    dest: &Dest,
    t: &SshTarget,
    pod: &Pod,
    cond: &str,
    repo: Option<RepoSite>,
    started: u64,
    snapshot_dir: &str,
    big_dir: &str,
    container: &str,
    rehearsal: bool,
) -> BackupEvidence {
    use arena_core::pull::{self, PullConfig};
    let env = ctx.env;
    let mut ev = BackupEvidence {
        key: core_reap::key(&pod.provider, &pod.id),
        container: container.to_string(),
        started,
        finished: started,
        lock_held: true,
        dest_ok: true,
        git: None,
        snapshot: Vec::new(),
        big: StepEnd::Error,
        verify: StepEnd::Error,
        snapshot_dir: snapshot_dir.to_string(),
        big_dir: big_dir.to_string(),
        frozen: None,
    };
    let began = tokio::time::Instant::now();
    let left = |b: Duration| -> Duration { Duration::from_secs(b.saturating_sub(began.elapsed()).as_secs().max(1)) };
    // Git: a repo the probe saw absent has nothing to push (the tiers carry the whole home).
    if repo != Some(RepoSite::Absent) {
        let key = env.cfg.get("GIT_SSH_KEY_REMOTE");
        let msg = format!("arena reap backup {}", pod.name);
        let cmd = arena_core::replica::guarded(Some(cond), &arena_core::backup::backup_command(&ctx.repo, key, &msg));
        let r = env.remote.exec(t, &cmd, Some(left(super::BACKUP_TIMEOUT.min(dest.budget)))).await;
        ev.git = Some(step_end(r));
        // No rsync after a git step that didn't end with its sentinel (R32).
        if judge::git_verdict(ev.git.as_ref()).is_err() {
            ev.finished = (env.clock)();
            return ev;
        }
    }
    // Space and markers again, right before the first write here (RS9): a volume unmounted
    // since the run's check (or during the git push) must not take this pod's last copy.
    if dest_check(env.cfg, &ctx.settings, &env.rsync).await.is_err() {
        ev.dest_ok = false;
        ev.finished = (env.clock)();
        return ev;
    }
    let guard = Some(arena_core::replica::guarded_rsync_path(cond));
    let base = Path::new(&dest.snap_root);
    let small = PullConfig { rsync_path: guard.clone(), ..PullConfig::small_tier(dest.threshold.as_str()) };
    let snap = pull::snapshot_tier_for(base, &dest.label, &pod.name, &small);
    for d in [snapshot_dir, big_dir] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("warning: creating {d}: {e}");
            ev.dest_ok = false;
            return ev;
        }
    }
    for pass in snap.passes() {
        let args = pull::rsync_args(t, &pass, snapshot_dir);
        let end = step_end(arena_core::remote::run_local(&env.rsync, &args, &format!("reap rsync of {} [snapshot]", pod.name), Some(left(dest.budget))).await);
        let ok = matches!(end, StepEnd::Exit { code: 0, .. });
        ev.snapshot.push(end);
        if !ok {
            ev.finished = (env.clock)();
            return ev;
        }
    }
    // RS2: the reap's own big tier — dot-directories kept, only the documented caches left out.
    let big = PullConfig { rsync_path: guard, ..PullConfig::reap_tier() };
    ev.big = step_end(arena_core::remote::run_local(&env.rsync, &pull::rsync_args(t, &big, big_dir), &format!("reap rsync of {} [big]", pod.name), Some(left(dest.budget))).await);
    if matches!(ev.big, StepEnd::Exit { code: 0, .. }) {
        ev.verify = step_end(
            arena_core::remote::run_local(&env.rsync, &pull::verify_args(t, &big, big_dir), &format!("reap verify of {}", pod.name), Some(left(dest.budget))).await,
        );
    }
    ev.finished = (env.clock)();
    if judge::verify_verdict(&ev.verify).is_ok() {
        ev.frozen = Some(freeze(dest, pod, (env.clock)(), snapshot_dir, big_dir, rehearsal));
    }
    ev
}

/// RS1: the verified backup, frozen — hard-link copies (`cp -al`: same filesystem, no space) of
/// both tiers' dirs into `<root>/reaped/<name>/<UTC stamp>-<provider>-<id>[-rehearsal]/`
/// (`snapshot/`, `big/`), counted against the originals. Under this pod's lock, so nothing
/// writes the originals meanwhile. The live per-name dirs are a later same-named pod's to pull
/// into; rsync writes a changed file as a new inode, so the frozen links keep what was
/// verified. Nothing in arena writes into or deletes from `reaped/` — but a freeze that fails
/// removes the `<stamp>-<provider>-<id>` leaves it made itself, exactly those (R2-3).
fn freeze(dest: &Dest, pod: &Pod, now: u64, snapshot_dir: &str, big_dir: &str, rehearsal: bool) -> Frozen {
    use arena_core::pull::{link_tree, tree_count};
    let leaf = format!(
        "{}-{}-{}{}",
        arena_core::restore::stamp(now),
        arena_core::snapshot::path_component(&pod.provider),
        arena_core::snapshot::path_component(&pod.id),
        if rehearsal { "-rehearsal" } else { "" }
    );
    let leaf_in = |root: &str| -> PathBuf {
        Path::new(root).join(arena_core::restore::REAPED).join(arena_core::snapshot::path_component(&pod.name)).join(&leaf)
    };
    let (snap_leaf, big_leaf) = (leaf_in(&dest.snap_root), leaf_in(&dest.big_root));
    let (snap_to, big_to) = (snap_leaf.join("snapshot"), big_leaf.join("big"));
    // The leaves this attempt made (a leaf already there is never this attempt's: refused).
    let mut made: Vec<PathBuf> = Vec::new();
    let mut one = |from: &str, leaf: &Path, to: &Path| -> bool {
        if !made.iter().any(|m| m == leaf) {
            let fresh = leaf.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::create_dir(leaf));
            if let Err(e) = fresh {
                eprintln!("warning: freezing {from}: can't make {} ({e})", leaf.display());
                return false;
            }
            made.push(leaf.to_path_buf());
        }
        let src = Path::new(from);
        match (tree_count(src), link_tree(src, to)) {
            (Ok(a), Ok(b)) if a == b => true,
            (a, b) => {
                eprintln!("warning: freezing {from} into {}: {a:?} vs {b:?}", to.display());
                false
            }
        }
    };
    let matched = one(snapshot_dir, &snap_leaf, &snap_to) && one(big_dir, &big_leaf, &big_to);
    if !matched {
        // Only what this attempt made — never anything else under `reaped/`.
        for leaf in &made {
            if let Err(e) = std::fs::remove_dir_all(leaf) {
                eprintln!("warning: removing the partial frozen copy {}: {e}", leaf.display());
            }
        }
    }
    Frozen { snapshot_dir: snap_to.display().to_string(), big_dir: big_to.display().to_string(), matched }
}

/// The `ENDS` column for `pods list` (spec §3.4): `None` when the reaper has no state here (no
/// explicit `ARENA_STATE_DIR`, or no state file) — the column isn't shown; a state that can't be
/// read shows `?` everywhere with one warning.
pub(crate) fn ends_column(cfg: &Config, pods: &[Pod], now: u64, tz: i64) -> Option<Vec<String>> {
    let store = Store::new(arena_core::snapshot::state_dir_explicit(cfg).ok()?);
    let state = match store.load_if_any() {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("warning: the reaper's state can't be read ({e}) — ENDS shows ?");
            return Some(vec!["?".into(); pods.len()]);
        }
    };
    let naming = Naming::from_config(cfg);
    let v2 = runpod_v2(cfg);
    Some(
        pods.iter()
            .map(|p| {
                let pol = state.pods.get(&core_reap::key(&p.provider, &p.id));
                let pin = state.pins.iter().find(|x| x.holds(&p.name, &p.id, now));
                let inert = pol.and_then(|pol| judge::inert_why(pol, p, pods, v2));
                render::ends_label(pol, pin, p, &naming, inert.as_deref(), now, tz)
            })
            .collect(),
    )
}

/// `pods rename`: keep the record's name (and a pin by that id) in step with the provider, under
/// the state lock. Best effort: no state, or one that can't be read, is a warning at most.
pub(crate) fn renamed(cfg: &Config, provider: &str, id: &str, new_name: &str) {
    let Ok(dir) = arena_core::snapshot::state_dir_explicit(cfg) else { return };
    let store = Store::new(dir);
    if !store.state_path().exists() {
        return;
    }
    let k = core_reap::key(provider, id);
    let r = store.update(|s| {
        if let Some(p) = s.pods.get_mut(&k) {
            p.name = new_name.to_string();
        }
        for pin in s.pins.iter_mut().filter(|p| p.id.as_deref() == Some(id)) {
            pin.name = new_name.to_string();
        }
        Ok(())
    });
    if let Err(e) = r {
        eprintln!("warning: the reaper's record of {id} wasn't renamed ({e}) — it now stays inert until `arena pods reap set` again");
    }
}

/// `pods replace` / `migrate copy` / `migrate cutover` (RS5): the refusal when the auto-reaper
/// is reaping `pod` right now — its write-ahead record (`terminating`) is in the state the
/// config FILE names (whatever this shell exports). Copying from, or renaming, a pod the reaper
/// is about to terminate would race it; the reaper's own last re-listing also stops at a
/// `-new`/`-old` twin, this closes the stretch before one is listed. It fails CLOSED (R2-4):
/// where the reaper is configured (`REAP_ENABLED=1` in the config file, or a reap cron line), a
/// state this user can't read — root's 0700 state dir from an operator's shell, a corrupt file
/// — refuses, naming the path and why. No state file at all: no reap. The reaper not
/// configured: no check (nothing changes for anyone not using it).
pub(crate) fn reap_in_progress(cfg: &Config, pod: &Pod, verb: &str) -> Option<String> {
    let configured = cfg.file_value("REAP_ENABLED").map(str::trim) == Some("1") || super::reap_cron_installed();
    if !configured {
        return None;
    }
    let cant_tell = |why: String| {
        Some(format!(
            "refusing to {verb}: can't tell whether the auto-reaper is reaping {} ({why}) — run this as the reaper's user, \
             or `arena pods reap hold` and check `arena pods reap status` first",
            pod.name
        ))
    };
    let store = match arena_core::snapshot::state_dir_in_file(cfg) {
        Ok(dir) => Store::new(dir),
        Err(e) => return cant_tell(e),
    };
    let path = store.state_path();
    match std::fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return cant_tell(format!("{}: {e}", path.display())),
        Ok(_) => {}
    }
    let state = match store.load() {
        Ok(s) => s,
        Err(e) => return cant_tell(e.to_string()),
    };
    let t = state.pods.get(&core_reap::key(&pod.provider, &pod.id))?.terminating.as_ref()?;
    Some(format!(
        "refusing to {verb}: the auto-reaper is reaping {} (its {} reap, run {}, since {} UTC) — wait for it to end \
         (`arena pods reap status`); `arena pods reap hold` stops it before its terminate",
        pod.name,
        t.trigger.word(),
        t.run_id,
        render::fmt_local(t.at, 0)
    ))
}

/// `pods replace` / `migrate`: the reap policy stays with the old id — say so.
pub(crate) fn policy_not_carried_note(cfg: &Config, provider: &str, old_id: &str, new_id: &str) -> Option<String> {
    let store = Store::new(arena_core::snapshot::state_dir_explicit(cfg).ok()?);
    let state = store.load_if_any().ok()??;
    state
        .pods
        .contains_key(&core_reap::key(provider, old_id))
        .then(|| format!("note: reap policy not carried to {new_id}; re-run 'arena pods reap set'"))
}

/// `config check`'s reaper rows, and what blocks reaping (counted as missing only when
/// REAP_ENABLED=1). Local and read-only: the config, the backup roots' markers and free space,
/// `timedatectl`, `rsync --version`.
#[cfg(test)]
pub(crate) fn readiness_rows(cfg: &Config, rsync: &str) -> (Vec<String>, Vec<String>) {
    let on = cfg.get("REAP_ENABLED").map(str::trim) == Some("1");
    readiness_rows_for(cfg, rsync, on)
}

/// [`readiness_rows`], `full` or not (RR8). Full — the reaper is on, or its cron line is
/// installed (observe mode needs the same): every row, ✗ for each gap. Off (opt-in, the
/// default): ONE neutral `·` row with how many things to set up before switching it on, no ✗
/// unless a REAP_* value is set and invalid (a typo the operator should see either way), and
/// no outside command run (`timedatectl`, `rsync`). The blockers are counted either way.
pub(crate) fn readiness_rows_for(cfg: &Config, rsync: &str, full: bool) -> (Vec<String>, Vec<String>) {
    let (mut lines, mut blockers) = (Vec::new(), Vec::new());
    let enabled = cfg.get("REAP_ENABLED").map(str::trim) == Some("1");
    // "✗ means fail" (`config check`): a failed row is ✗ only where the check fails on it — a
    // value set but invalid (always), or a gap that blocks reaping while the reaper is on
    // (REAP_ENABLED=1: the caller counts its blocker missing). A gap while it only observes,
    // and advice (no blocker), are ⚠.
    let mut row = |ok: Option<bool>, what: &str, detail: String, gap: Gap| {
        let mark = match (ok, gap) {
            (Some(true), _) => "✓",
            (Some(false), Gap::Invalid(_)) => "✗",
            (Some(false), Gap::Blocks(_)) if enabled => "✗",
            (Some(false), _) => "⚠",
            (None, _) => "·",
        };
        lines.push(format!("  {mark} {what:<28} {detail}"));
        if ok == Some(false) {
            if let Gap::Blocks(b) | Gap::Invalid(Some(b)) = gap {
                blockers.push(b.to_string());
            }
        }
    };
    row(
        Some(true),
        "REAP_ENABLED",
        if enabled { "1 — `pods reap run --apply` acts".into() } else { "off (default) — runs only observe".into() },
        Gap::None,
    );
    let settings = match ReapSettings::from_config(cfg) {
        Ok(s) => {
            row(
                Some(true),
                "REAP_* settings",
                format!(
                    "caps {}/run {}/day · idle floor {} · free ≥ {} GB per tier · notify {}",
                    s.max_per_run,
                    s.max_per_day,
                    core_reap::fmt_dur(s.idle_floor_secs),
                    s.min_free_bytes / 1_000_000_000,
                    if s.notify_cmd.is_some() { "REAP_NOTIFY_CMD" } else { "the cron log only" }
                ),
                Gap::None,
            );
            Some(s)
        }
        // Set but invalid: ✗, and `config check` names the key (`REAP_… (why)`) itself.
        Err(e) => {
            row(Some(false), "REAP_* settings", e, Gap::Invalid(None));
            None
        }
    };
    match state_path(cfg) {
        Ok(p) => row(Some(true), "state (ARENA_STATE_DIR)", p.display().to_string(), Gap::None),
        Err(e) if is_invalid_state_dir(&e) => row(Some(false), "state (ARENA_STATE_DIR)", e, Gap::Invalid(Some("ARENA_STATE_DIR (explicit, absolute)"))),
        Err(e) => row(Some(false), "state (ARENA_STATE_DIR)", e, Gap::Blocks("ARENA_STATE_DIR (explicit, absolute)")),
    }
    let v2 = runpod_v2(cfg);
    row(
        Some(v2),
        "RUNPOD_API",
        if v2 { "v2 — RunPod pods report their lock".into() } else { "reaper needs RUNPOD_API=v2 (on v1 no RunPod pod is ever reaped)".into() },
        Gap::None,
    );
    let snap = cfg.get("LOCAL_BACKUP_DIR").map(str::trim).filter(|d| !d.is_empty()).unwrap_or("./backup").to_string();
    let big = arena_core::pull::big_root(&snap, cfg.get("LOCAL_BACKUP_BIG_DIR"));
    for (root, tier) in [(snap.clone(), arena_core::pull::Tier::Snapshot), (big, arena_core::pull::Tier::Big)] {
        let p = Path::new(&root);
        let what = format!("{} tier root", tier.word());
        if !p.is_absolute() {
            row(Some(false), &what, format!("{root} — must be absolute"), Gap::Blocks("absolute LOCAL_BACKUP_DIR"));
            continue;
        }
        match arena_core::pull::check_tier_marker(p, tier) {
            Err(e) => row(Some(false), &what, e, Gap::Blocks("the backup tier markers")),
            Ok(()) => {
                let free = arena_core::pull::free_bytes(p).map(|b| format!("{:.1} GB free", b as f64 / 1e9)).unwrap_or_else(|e| format!("free space unknown ({e})"));
                let low = settings.as_ref().is_some_and(|s| arena_core::pull::free_bytes(p).is_ok_and(|b| b < s.min_free_bytes));
                row(Some(!low), &what, format!("{root} — marked `{}`, {free}", tier.word()), Gap::Blocks("free space on the backup tiers"));
            }
        }
    }
    let start = cfg.get("ARENA_START_DATE").is_some_and(|d| arena_core::schedule::parse_ymd(d).is_some());
    row(Some(start), "ARENA_START_DATE", if start { "set".into() } else { "unset — the snapshot tier has no wNdM label".into() }, Gap::Blocks("ARENA_START_DATE"));
    let remote_path = cfg.get("BACKUP_REMOTE_PATH").is_some_and(|v| !v.trim().is_empty());
    row(
        Some(!remote_path),
        "BACKUP_REMOTE_PATH",
        if remote_path { "set — a reap needs the whole home backed up; unset it".into() } else { "unset (the whole home)".into() },
        Gap::Blocks("BACKUP_REMOTE_PATH unset"),
    );
    if !full {
        let invalid = settings.is_none();
        let n = blockers.len();
        let mut out: Vec<String> = if invalid { lines.into_iter().filter(|l| l.contains("REAP_* settings")).collect() } else { Vec::new() };
        out.push(format!(
            "  · {:<28} off (opt-in){}",
            "reaper",
            if n == 0 {
                String::new()
            } else {
                format!(" — {n} thing(s) to set up before switching it on (REAP_ENABLED=1 in the config file lists them)")
            }
        ));
        return (out, blockers);
    }
    let ntp = real::ntp();
    row(
        Some(ntp == Some(true)),
        "clock (timedatectl)",
        match ntp {
            Some(true) => "NTP-synchronised".into(),
            Some(false) => "NOT synchronised — runs observe only".into(),
            None => "can't tell (no timedatectl) — runs observe only".into(),
        },
        Gap::Blocks("an NTP-synchronised clock"),
    );
    let rsync_ok = std::process::Command::new(rsync).arg("--version").output().is_ok_and(|o| o.status.success());
    row(Some(rsync_ok), "rsync", if rsync_ok { "runs".into() } else { format!("`{rsync} --version` doesn't run") }, Gap::Blocks("rsync"));
    (lines, blockers)
}

/// What a failed row of [`readiness_rows_for`] means to `config check` ("✗ means fail").
#[derive(Clone, Copy)]
enum Gap {
    /// Nothing the check counts: a ✓ row's, or advice (⚠).
    None,
    /// Blocks reaping: ✗ and in the error list while the reaper is on, ⚠ while it observes.
    Blocks(&'static str),
    /// A value set but invalid: ✗ always (`config check` names the key itself); its blocker,
    /// if any, still counts while the reaper is on.
    Invalid(Option<&'static str>),
}

/// Whether [`state_path`]'s error is an invalid value (a relative `ARENA_STATE_DIR` in the file)
/// rather than a gap (unset there, or the environment's differing).
pub(crate) fn is_invalid_state_dir(e: &str) -> bool {
    e.starts_with("ARENA_STATE_DIR must be")
}

/// Where the reaper keeps its state, for `config check`.
pub(crate) fn state_path(cfg: &Config) -> std::result::Result<PathBuf, String> {
    arena_core::snapshot::state_dir_explicit(cfg).map(|d| d.join(store::STATE_FILE))
}

#[cfg(test)]
mod tests;
