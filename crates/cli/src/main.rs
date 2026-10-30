//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Mutating commands
//! **act by default but confirm first**: at a terminal they print what they'll do and
//! prompt `Proceed? [y/N]`; `--yes` skips the prompt; with no terminal they refuse
//! unless `--yes`. `--dry-run` previews without doing anything. This is deliberate —
//! the tool is developed against a live production account.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;

use arena_core::health::judge_deep_call;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::sshconfig::Audience;
use arena_core::volume::RepoSite;
use arena_core::{Config, PodSpec};

#[cfg(all(test, unix))]
mod admin_key_tests;
mod archive;
mod disk;
mod image;
mod ask;
#[cfg(test)]
mod endpoint_tests;

#[cfg(test)]
mod gpu_vm_setup_tests;
#[cfg(test)]
mod hetzner_cli_tests;
#[cfg(test)]
mod hetzner_setup_tests;
mod help;
mod jobs;
mod key_limits;
mod money;
mod offers;
mod reap;
mod resolve;
mod spend;
mod teardown;
mod up;
mod cli;
mod config_cmd;
mod cron;
mod keys;
mod plan;
mod pods;
mod proxy;
mod snapshot;
mod ssh_config;

use cli::{ApiCmd, BalanceCmd, Cmd, PlaceFlags, PodCmd, Select};
use config_cmd::{config_check, handle_config, hetzner_image_rows, ConfigCmd};
use cron::{handle_cron, resolve_week_day, CRONTAB};
use keys::{cohort_keys_file, cohort_keys_header, ensure_cohort_keys_file, handle_copy_keys, handle_keys, machine_key_names, openrouter_client, shared_name_holder, up_keys_stage, write_secret_file, KeyBook, KeySources, KEYS_DIR, OPENROUTER_KEYS_CSV};
use plan::handle_plan;
use pods::backup::{handle_full_backup, RSYNC};
use pods::baseline::handle_baseline;
use pods::branch::{handle_init_branches, set_branch_and_init, SetBranchRun};
use pods::copy::handle_copy;
use pods::create::{apply_spec_overrides, check_create_body, create_extra, create_policy_line, create_with_retry, hand_keys_over, image_flag, owner_plan, pin_vm_image, plan_create, print_create_body, replacement_rounds, resolve_explicit_names, resolve_want, sizing_flags, spec_with_overrides, warn_no_volume, CreateFailed, Made, OnMade, RecordingProvider, SpecOverrides, NO_STREAM};
use pods::health::handle_deep_test;
use pods::list::{list_with, ENRICH_TIMEOUT};
use pods::lock::{handle_lock, lock_why, print_would_refuse, LOCKED_REPLACEMENT_NOTE};
use pods::pull::{clear_stale_link, handle_pull, local_backup_dir, resolve_pull};
use pods::restore::{handle_restore, RestoreOpts};
use pods::run::{handle_run, misplaced_run_flags, pod_name_as_command};
use pods::selection::{cohort_only, hard_non_cohort_refusal, just_these, refresh_missing_endpoints, resolve_target_any, select, Selected, Unscoped, ENDPOINT_RETRY};
use pods::setup::{handle_setup, setup_config, stage_hetzner_script, unstage_hetzner_script};
use pods::{exec_each_pod, local_size, POD_COPY_TIMEOUT, TEST_TIMEOUT};
use proxy::{handle_proxy, proxy_deployable, sync_line, sync_proxy, ProxySync};
use snapshot::handle_snapshot;
use ssh_config::{handle_ssh_config, handle_ssh_config_admin, handle_ssh_config_install, handle_ssh_config_uninstall, ssh_config_after};
// Reached as `super::x`/`crate::x` by the modules that predate the split (new code imports from the module itself).
use cli::{Cli, SelectOpts};
use config_cmd::note_invalid_text;
use cron::{arena_cron_lines, char_named, cron_unsafe_char, reap_cron_installed, CRON_PATH_CHARS};
use keys::{copy_keys_command, drop_key_row, fleet_ssh_for, key_holders, keys_targets, rotate_key, write_openrouter_key, COPY_KEYS_TIMEOUT, OPENROUTER_KEYS_LINK};
use pods::backup::{failure_why, BackupFailed, Unpushed};
use pods::health::{health_cache_path, record_health};
use pods::list::enrich_best_effort;
use pods::pull::{absolute_path, pull_roots, take_pod_locks, Pulled, POD_LOCK_WAIT};
use pods::selection::say;
use pods::setup::repair_pod_keys;
use pods::{each_pod, PodCall, BACKUP_TIMEOUT};
use proxy::{proxy_sync_now, sync_proxy_to_stderr};
use ssh_config::ssh_config_lines;
#[cfg(test)]
use cli::{CohortScope, ReapAtCreate, SelectByFlag};
#[cfg(test)]
use config_cmd::{admin_key_rows, bad_hosts_section, balance_section, config_report, hetzner_rows};
#[cfg(test)]
use cron::{env_assignment, reap_cron_line, CronKind, CRON_BEGIN, CRON_END};
#[cfg(test)]
use keys::{keys_preview, keys_with, KeysAction, KeysCmd};
#[cfg(test)]
use pods::backup::{backup_fleet, copy_unpushed, fallback_lines, handle_backup, on_protected_line, on_protected_lines, telling_line, BackupTally};
#[cfg(test)]
use pods::branch::{discarded_note, handle_set_branch, not_pushed_note, push_first_note, restored_note, set_branch_reminder, unpushed_note, unredacted_note};
#[cfg(test)]
use pods::copy::{copy_to_pod, CopyPlan};
#[cfg(test)]
use pods::create::{CAPACITY_RETRY_SECS, MAX_CAPACITY_ATTEMPTS};
#[cfg(test)]
use pods::health::{deep_check_fleet, duplicate_names, render_deep_test};
#[cfg(test)]
use pods::list::probe_gpus;
#[cfg(test)]
use pods::lock::{lock_settles, take_would_refuse};
#[cfg(test)]
use pods::pull::{pull_label_and_targets, pull_locked, pull_pods, LockMode, PodLocks, PullRun};
#[cfg(test)]
use pods::restore::{restore_note, restore_notes, RestoreSource};
#[cfg(test)]
use pods::run::{render_run, run_failure, run_fleet, run_preview, RunResult};
#[cfg(test)]
use pods::selection::{non_cohort_line, split_cohort, take_said};
#[cfg(test)]
use pods::setup::HETZNER_SETUP;
#[cfg(test)]
use pods::{cp_timeout, BRANCH_TIMEOUT, CP_BASE_TIMEOUT, RUN_TIMEOUT_SECS};
#[cfg(test)]
use proxy::Written;
#[cfg(test)]
use ssh_config::ssh_config_check_row_as;

/// Ask the operator to confirm a mutating action. `--yes` proceeds without asking. At
/// an interactive terminal, prompts y/N (`Ok(false)` = they said no: "aborted.", exit 0).
/// With **no terminal** (cron/pipes) and no `--yes`, it *refuses* — automation must opt in
/// explicitly with `--yes`, so nothing mutates non-interactively by accident — and the
/// refusal is an error ([`NoTerminal`], exit status 2): it used to exit 0 after "aborted.",
/// so a script or cron job couldn't tell "refused" from "done" (finding #25).
fn confirm(assume_yes: bool, what: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("{what}");
        return Err(NoTerminal.into());
    }
    eprint!("{what}\nProceed? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// The refusal [`confirm`] fails with when there's no terminal to ask at and no `--yes`.
/// `main` exits with [`NO_TERMINAL_EXIT`] for it (after printing it), never 0.
#[derive(Debug)]
struct NoTerminal;

impl std::fmt::Display for NoTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refusing to proceed without a terminal — pass --yes to confirm non-interactively")
    }
}

impl std::error::Error for NoTerminal {}

/// The exit status for a [`NoTerminal`] refusal: 2, apart from a failed command's 1.
const NO_TERMINAL_EXIT: u8 = 2;

/// `update --check`'s answer when a newer release exists: exit [`UPDATE_AVAILABLE_EXIT`], so a
/// cron line or MOTD can tell it from "up to date" (0) and "couldn't check" (1).
#[derive(Debug)]
struct UpdateAvailable;

impl std::fmt::Display for UpdateAvailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a newer release is available: `arena update` installs it")
    }
}

impl std::error::Error for UpdateAvailable {}

/// The exit status for [`UpdateAvailable`].
const UPDATE_AVAILABLE_EXIT: u8 = 10;

/// This build's own `CHANGELOG.md`: what `arena update` compares the release's against.
const CHANGELOG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md"));

/// What `update --skip-attestation` says before it downloads anything, on stderr.
const SKIP_ATTESTATION_WARNING: &str = "\
⚠ --skip-attestation: the new binaries' build provenance will NOT be checked by this build.
⚠ It installs only binaries whose SHA-256 is one you gave: they must be ones you verified with
⚠ `gh attestation verify` (docs/REFERENCE.md), not digests someone sent you.";

/// `arena update` ([`arena_core::update`]): check, then — unless `--check` — download, check
/// (provenance, or the digests `skip_attestation` pins) and swap in the release's binaries
/// where this one is.
async fn handle_update(check_only: bool, dry_run: bool, skip_attestation: Vec<String>, yes: bool) -> Result<()> {
    use arena_core::{attest, update as up};
    // A malformed pin fails before anything is read.
    let pins = (!skip_attestation.is_empty()).then(|| up::parse_pins(&skip_attestation)).transpose().map_err(|e| anyhow::anyhow!("update: {}", up::printable(&e)))?;
    let installed = up::Version::parse(env!("ARENA_VERSION")).context("this build's version")?;
    let client = up::client();
    let found = up::check(&client, up::REPO_API, installed, CHANGELOG).await.map_err(|e| anyhow::anyhow!("update: {}", up::printable(&e)))?;
    if !found.newer {
        println!("arena {installed} is up to date (the latest release is {}).", found.release.tag);
        return Ok(());
    }
    println!("Update available: arena {installed} → {} ({})", found.release.version, found.release.tag);
    if check_only {
        println!("What changed, from the release's CHANGELOG.md (not verified: `arena update --dry-run` verifies it):\n");
        print!("{}", found.changes);
        return Err(UpdateAvailable.into());
    }
    // Where this binary really is (`/usr/local/bin/arena` is a link into ~/.cargo/bin on prod).
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).context("finding this binary")?;
    let dir = exe.parent().context("this binary has no directory")?;
    if exe.file_name().and_then(|n| n.to_str()) != Some("arena") {
        anyhow::bail!("this binary is {} — `arena update` replaces one named `arena` (and `arena-tui` beside it)", exe.display());
    }
    let target = env!("ARENA_TARGET");
    let github = attest::GitHub { client: &client, api: up::REPO_API };
    let pinned = pins.is_some();
    let provenance = match pins {
        Some(pins) => {
            eprintln!("\n{SKIP_ATTESTATION_WARNING}");
            up::Provenance::Pinned(pins)
        }
        None => up::Provenance::Verify { source: &github, expected: attest::RELEASE },
    };
    println!("\nDownloading {} for {target} …", found.release.tag);
    let staged = up::fetch(&client, &found.release, target, dir, &provenance).await.map_err(|e| anyhow::anyhow!("update: {}", up::printable(&e)))?;
    // The changelog is shown only now, with what can be said of it: before, it was the
    // release's word alone, and its "Action needed" lines are instructions to an operator.
    let changelog = up::changelog_provenance(staged.proof.as_ref(), found.changelog_sha256.as_deref()).map_err(|e| anyhow::anyhow!("update: {}", up::printable(&e)))?;
    match &staged.proof {
        Some(proof) => {
            println!("Both binaries match the release's SHA256SUMS, and one build-provenance attestation names both");
            println!("(Sigstore, verified here): {proof}.");
        }
        None => println!("Both binaries match the release's SHA256SUMS and the SHA-256s given (provenance NOT checked here)."),
    }
    println!("The new arena runs here.");
    println!("\nWhat changed, from the release's CHANGELOG.md ({changelog}):\n");
    print!("{}", found.changes);
    println!();
    if dry_run {
        println!("--dry-run: nothing installed.");
        return Ok(());
    }
    let what = format!(
        "Install arena {}{} in {} (the current arena and arena-tui become arena.prev and arena-tui.prev)?",
        found.release.version,
        if pinned { " (provenance NOT checked here)" } else { "" },
        dir.display()
    );
    if !confirm(yes, &what)? {
        println!("Not installed.");
        return Ok(());
    }
    staged.install().map_err(|e| anyhow::anyhow!("update: {}", up::printable(&e)))?;
    println!("Installed arena {} in {}.", found.release.version, dir.display());
    println!("Roll back: cd {} && mv -f arena.prev arena && mv -f arena-tui.prev arena-tui", dir.display());
    let stale = up::stale_processes(std::path::Path::new("/proc"), &dir.join("arena-tui"));
    if !stale.is_empty() {
        let started = |t: Option<std::time::SystemTime>| {
            t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or_else(|| "start unknown".into(), |d| format!("started {}", arena_core::snapshot::rfc3339(d.as_secs())))
        };
        let list: Vec<String> = stale.iter().map(|(pid, t)| format!("pid {pid} ({})", started(*t))).collect();
        println!("{} dashboard(s) still run the old build — restart them: {}", stale.len(), list.join(", "));
    }
    Ok(())
}

/// What `main` ends with for `result`: success, [`NO_TERMINAL_EXIT`] for a refused
/// confirmation, else 1. Pure, so the mapping is tested.
fn exit_code(result: &Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) if e.downcast_ref::<NoTerminal>().is_some() => std::process::ExitCode::from(NO_TERMINAL_EXIT),
        // The reaper's own statuses: 2 config invalid, 3 needs attention (spec §4 step 13).
        Err(e) if e.downcast_ref::<reap::ReapExit>().is_some() => {
            std::process::ExitCode::from(e.downcast_ref::<reap::ReapExit>().map_or(1, |r| r.code))
        }
        Err(e) if e.downcast_ref::<UpdateAvailable>().is_some() => std::process::ExitCode::from(UPDATE_AVAILABLE_EXIT),
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

#[derive(Subcommand)]
enum MigrateCmd {
    /// Build `<name>-new` and sync the pod's files onto it
    ///
    /// Repeatable. Never renames or touches the proxy: the participant keeps using `<name>` and
    /// can SSH the new pod directly to test it.
    #[command(verbatim_doc_comment)]
    Copy {
        /// Machine name (or id) to migrate.
        target: String,
        /// GPU type, or a comma list of fallbacks (default: same as the source)
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (default: same as the source).
        #[arg(long)]
        gpus: Option<u32>,
        /// Cloud tier COMMUNITY/SECURE (default: same as the source) — or a comma list.
        #[arg(long)]
        cloud: Option<String>,
        /// Container disk GB (default: same as the source).
        #[arg(long)]
        disk: Option<u32>,
        /// Persistent volume GB (default: same as the source).
        #[arg(long)]
        volume: Option<u32>,
        /// Docker image (default: same as the source, else config IMAGE).
        #[arg(long)]
        image: Option<String>,
        /// Start sshd on boot (for a bare `--image` that doesn't run it)
        #[arg(long)]
        bootstrap: bool,
        #[command(flatten)]
        place: PlaceFlags,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Final sync, swap names, re-point the proxy, verify
    ///
    /// `<name>` becomes `<name>-old` and `<name>-new` becomes `<name>`. If the new pod isn't
    /// reachable through the proxy, it all reverts automatically. `<name>-old` is kept until
    /// `migrate finish`.
    #[command(verbatim_doc_comment)]
    Cutover {
        /// Machine name (or id) being migrated.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Don't touch the proxy at all (just swap names); you run `arena proxy apply`.
        #[arg(long)]
        skip_proxy: bool,
        /// Skip the final sync (for an original that's stopped or unreachable)
        ///
        /// Changes on the original since the last `migrate copy` stay behind on it.
        #[arg(long)]
        no_final_sync: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Terminate the parked `<name>-old` once the new pod is good
    ///
    /// Syncs the proxy first and refuses until `<name>`'s port routes to the live pod. Refuses a
    /// STOPPED `-old` without --discard-stopped: what changed on it since the last sync would be
    /// lost.
    #[command(verbatim_doc_comment)]
    Finish {
        /// Machine name (or id) that was migrated.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Terminate a stopped `-old` anyway, losing whatever wasn't synced from it
        #[arg(long)]
        discard_stopped: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Roll a cutover back to the original pod
    ///
    /// Swaps the names and the proxy back. Work done on the new pod since the cutover is NOT
    /// moved back: it stays on `<name>-new`.
    #[command(verbatim_doc_comment)]
    Revert {
        /// Machine name (or id) to roll back.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Don't touch the proxy (just swap names back).
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Show the migration pair and what each step does next
    Status {
        /// Machine name (or id).
        target: String,
    },
}

/// Ctrl+C for one command, latched: once pressed it stays pressed, and every phase of the
/// command is handed the same one. `tokio::signal::ctrl_c()` alone only fires for a SIGINT
/// that arrives after *that* listener started, so a Ctrl+C taken by one phase (`up`'s
/// create retry wait) was invisible to the next (`up`'s pipelines, which then went on
/// terminating and recreating pods under `--check` after the operator had said stop).
///
/// Armed lazily — the first time something waits on it — so until then SIGINT keeps its
/// default (end the process), exactly as before. Once armed it stays armed for the process
/// (tokio's handler can't be removed); from then on a Ctrl+C is only ever recorded here,
/// which is why each phase after that must look at this latch rather than a fresh listener.
#[derive(Clone)]
struct Interrupt {
    pressed: Arc<tokio::sync::watch::Sender<bool>>,
    armed: Arc<std::sync::atomic::AtomicBool>,
}

impl Interrupt {
    /// The terminal's Ctrl+C (SIGINT), not listened for until first waited on.
    fn ctrl_c() -> Self {
        Self {
            pressed: Arc::new(tokio::sync::watch::channel(false).0),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// One nothing presses but [`Interrupt::press`] — no SIGINT handler is installed.
    #[cfg(test)]
    fn manual() -> Self {
        let i = Self::ctrl_c();
        i.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        i
    }

    #[cfg(test)]
    fn press(&self) {
        self.pressed.send_replace(true);
    }

    /// Start recording Ctrl+C (once). The SIGINT listener is registered right here, not on
    /// a spawned task's first poll, so no press slips between arming and listening.
    fn arm(&self) {
        if self.armed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        use tokio::signal::unix::{signal, SignalKind};
        // No handler (can't happen on unix): SIGINT keeps ending the process.
        let Ok(mut sigint) = signal(SignalKind::interrupt()) else { return };
        let pressed = self.pressed.clone();
        tokio::spawn(async move {
            if sigint.recv().await.is_some() {
                pressed.send_replace(true);
            }
        });
    }

    fn is_set(&self) -> bool {
        *self.pressed.borrow()
    }

    /// Resolves once Ctrl+C has been pressed — at once if it already was, in any phase.
    fn wait(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        self.arm();
        let mut rx = self.pressed.subscribe();
        async move {
            // `wait_for` looks at the current value first. It only errs once the latch
            // itself is gone, and then nothing can press it any more.
            if rx.wait_for(|p| *p).await.is_err() {
                std::future::pending::<()>().await
            }
        }
    }
}

/// `--max-price`: a positive, finite $/h (a leading `$` is fine).
fn parse_max_price(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.trim().trim_start_matches('$').parse().map_err(|_| format!("`{s}` is not a $/h price (e.g. 0.50)"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!("must be a positive $/h (got {s})"))
    }
}

/// Decide how `create`/`up` place pods, before anything is listed or created. Returns the
/// overrides the single-spec path creates from, and: `None` = one GPU, one cloud and no
/// price cap: today's single-spec path, unchanged (same output, same `--keep-trying`).
/// `Some` = ordered placement over several options, with the spec base (config + every
/// other override) the options are layered on.
///
/// A list that collapses to one option (`--gpu A4000,a4000`, `--cloud community,`) is
/// "single" because it names one option — so the single path must create *that* option:
/// `--gpu`/`--cloud` come back as the parsed values, never the raw list string, which
/// would otherwise reach the provider as a GPU id `A4000,a4000` / tier `COMMUNITY,`.
///
/// `--keep-trying` waits on *one* pool; with several options the retry window
/// (`--retry-mins`) is the waiting mechanism — each round re-tries every option — so the
/// combination is refused rather than given a third meaning.
fn placement_request(
    cfg: &Config,
    provider_name: &str,
    ov: SpecOverrides,
    max_price: Option<f64>,
    order: arena_core::placement::Order,
    keep_trying: bool,
) -> Result<(SpecOverrides, Option<(arena_core::placement::Request, PodSpec)>)> {
    let base = spec_with_overrides(cfg, &SpecOverrides { gpu: None, cloud: None, ..ov.clone() });
    let req = arena_core::placement::Request::from_flags(ov.gpu.as_deref(), ov.cloud.as_deref(), &base, max_price, order)?
        .with_near(arena_core::near::Near::for_provider(cfg, provider_name)?, provider_name, &base);
    // Near the proxy turns one pool into two (there, then anywhere); `--keep-trying` waits on
    // one, so it keeps today's single path — anywhere — and says so.
    let single = arena_core::placement::Request { near: None, ..req.clone() };
    if keep_trying && req.near.is_some() && single.is_single() {
        let near = req.near.map(|n| n.region.label()).unwrap_or_default();
        eprintln!("note: --keep-trying waits on one pool, so this run doesn't try {near} first (NEAR) — --retry-mins N keeps that");
    }
    let req = if keep_trying && single.is_single() { single } else { req };
    if req.is_single() {
        // Only a flag that was given is replaced (an absent one keeps "what config says",
        // exactly as today); the parsed GPU is already resolved, and resolving is idempotent.
        let single = SpecOverrides {
            gpu: ov.gpu.as_ref().map(|_| req.gpus[0].clone()),
            cloud: ov.cloud.as_ref().map(|_| req.clouds[0].clone()),
            ..ov
        };
        return Ok((single, None));
    }
    req.validate_for(provider_name)?;
    if keep_trying {
        anyhow::bail!(
            "--keep-trying waits on one GPU/cloud; with several options or --max-price use \
             --retry-mins N instead (each round re-tries every option)"
        );
    }
    Ok((ov, Some((req, base))))
}

/// Upper bound on each read-only catalog lookup before placement. The HTTP client has no
/// timeout of its own; a stalled price API must not hang a create that doesn't need it
/// (it falls back to the preset estimates instead).
const PRICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn bounded_catalog<F>(fetch: F) -> arena_core::Result<Vec<arena_core::provider::runpod::GpuType>>
where
    F: std::future::Future<Output = arena_core::Result<Vec<arena_core::provider::runpod::GpuType>>>,
{
    match arena_core::ratelimit::bounded(PRICE_TIMEOUT, fetch).await {
        Ok(r) => r,
        Err(_) => Err(arena_core::Error::provider(format!("timed out after {}s", PRICE_TIMEOUT.as_secs()))),
    }
}

/// RunPod's REST v2 GPU catalog for one tier (read-only, bounded by [`PRICE_TIMEOUT`]),
/// fetched at most once per tier per run: the `--gpu` check and the placement plan's price
/// book share the answer (or the failure — see [`arena_core::provider::runpod_v2::CatalogCache`]).
///
/// Stock is counted over the hosts a create would accept: the config's CUDA floor goes along
/// as `minCudaVersion` (one floor per run, so the cache key needn't carry it).
async fn v2_catalog(cfg: &Config, key: &str, cloud: &str) -> std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String> {
    use arena_core::provider::runpod_v2;
    static CACHE: std::sync::OnceLock<runpod_v2::CatalogCache> = std::sync::OnceLock::new();
    // A floor that isn't a version isn't sent to the read-only catalog (the create refuses it
    // with a clear error; a junk query parameter would only fail the `--gpu` check obscurely).
    let floor = PodSpec::from_config(cfg).cuda_floor().filter(|f| arena_core::pod::cuda_key(f).is_some());
    CACHE
        .get_or_init(runpod_v2::CatalogCache::default)
        .get_or_fetch(key, cloud, |tier| bounded_catalog(runpod_v2::fetch_gpu_types(key, tier, floor.as_deref())))
        .await
}

/// The tier whose v2 catalog answers questions that aren't about a tier (the `--gpu`
/// check, `arena offers runpod`): the configured `CLOUD_TYPE`, or COMMUNITY. Every tier's catalog
/// lists every GPU, so this only picks which tier's stock a reader sees — and with the
/// configured tier, a plain `create` reuses the check's fetch for its prices.
fn v2_default_cloud(cfg: &Config) -> &'static str {
    arena_core::provider::runpod_v2::normalize_cloud(&PodSpec::from_config(cfg).cloud_type).unwrap_or("COMMUNITY")
}

/// RunPod's live GPU catalog, for checking `--gpu` (read-only; each fetch bounded by
/// [`PRICE_TIMEOUT`]): on `RUNPOD_API=v2` the REST v2 catalog — the one v2's create
/// validates `gpu.id` against — and nothing else (no GraphQL, see `runpod_v2`'s GraphQL
/// inventory); otherwise the GraphQL `gpuTypes`, the full catalog whatever the tier (a
/// superset of what create accepts, so checking against it never refuses a creatable id).
/// `Err` says why there's no catalog (no key, unreachable, empty).
async fn live_gpu_catalog(cfg: &Config) -> std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String> {
    use arena_core::provider::{runpod, RunpodApi};
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Err("no RUNPOD_API_KEY".into());
    };
    if matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V2)) {
        let cloud = v2_default_cloud(cfg);
        return v2_catalog(cfg, key, cloud).await.map_err(|e| format!("v2 catalog ({cloud}): {e}"));
    }
    match bounded_catalog(runpod::fetch_gpu_types(key)).await {
        Ok(types) if !types.is_empty() => Ok(types),
        Ok(_) => Err("GraphQL catalog: empty".to_string()),
        Err(e) => Err(format!("GraphQL catalog: {e}")),
    }
}

/// What to do with a `--gpu` value given the catalog fetch's outcome. Pure (the policy is
/// table-tested): with a catalog, every token must be a RunPod GPU — the value comes back
/// as exact ids, or the error names the bad tokens with "did you mean"
/// ([`arena_core::gpu::check_gpu_flag_with`]) — or a type the catalog lacks that arena has
/// seen (`seen`: the account's pods, `RUNPOD_EXTRA_GPU_TYPES`; and arena's own list), with a
/// warning each; without a catalog, the value passes through unchanged with a warning —
/// being unable to *check* must not block a create.
fn gpu_flag_or_warning(
    raw: String,
    catalog: std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String>,
    seen: &[String],
) -> Result<(String, Vec<String>)> {
    match catalog {
        Ok(types) => {
            let checked = arena_core::gpu::check_gpu_flag_with(&raw, &types, seen)?;
            let warnings = checked
                .unlisted
                .iter()
                .map(|id| format!("warning: --gpu `{id}` isn't in RunPod's public catalog — trying it; RunPod's create decides"))
                .collect();
            Ok((checked.ids, warnings))
        }
        Err(why) => {
            let warning =
                format!("warning: couldn't check --gpu `{raw}` against RunPod's live GPU catalog ({why}) — using it unchecked");
            Ok((raw, vec![warning]))
        }
    }
}

/// `--gpu` as `create`/`up`/`offers`/`replace`/`migrate` should use it: on RunPod, checked
/// against the live catalog first — a typo (or a card without an alias) used to go to the
/// provider as an invalid id and fail late ("no known price", a 400 at create). Other
/// providers name GPUs their own way (Vast matches loosely; Hetzner has none): passed
/// through. Read-only, before anything is listed or created.
async fn checked_gpu(cfg: &Config, provider_name: &str, gpu: Option<String>) -> Result<Option<String>> {
    use arena_core::provider::{runpod, RunpodApi};
    let Some(raw) = gpu else { return Ok(None) };
    if provider_name == "shadeform" {
        // Shadeform answers an unknown GPU name with an empty catalog (read as "no stock"
        // forever): every token must map to one of its types, checked here before anything.
        for token in arena_core::placement::split_list(&raw) {
            arena_core::provider::shadeform::shadeform_gpu_type(&arena_core::gpu::resolve(&token))?;
        }
        return Ok(Some(raw));
    }
    if provider_name != "runpod" {
        return Ok(Some(raw));
    }
    let catalog = live_gpu_catalog(cfg).await;
    let types = catalog.as_ref().ok().cloned();
    // A type the catalog lacks may be one arena has seen: the config's list first, then — only
    // when that isn't enough, so a plain `--gpu A4000` costs no extra request — the account's pods.
    let extra = offers::extra_gpu_types(cfg);
    let (gpu, warnings) = match gpu_flag_or_warning(raw.clone(), catalog.clone(), &extra) {
        Ok(ok) => ok,
        Err(e) => {
            let pods = pod_gpu_types(cfg).await;
            if pods.is_empty() {
                return Err(e);
            }
            gpu_flag_or_warning(raw, catalog, &[extra, pods].concat())?
        }
    };
    for w in warnings {
        eprintln!("{w}");
    }
    // REST v1 only: its create validates against an OpenAPI enum that lags the catalog.
    let v1 = matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V1));
    if let (true, Some(types), Some(key)) = (v1, types, cfg.get("RUNPOD_API_KEY").filter(|k| !k.is_empty())) {
        if let Ok(Ok(creatable)) = arena_core::ratelimit::bounded(PRICE_TIMEOUT, runpod::fetch_creatable_gpu_ids(key)).await {
            v1_creatable_check(&gpu, &types, &creatable)?;
        }
    }
    Ok(Some(gpu))
}

/// The GPU types on the account's RunPod pods (v2 with a key; read-only, bounded by
/// [`PRICE_TIMEOUT`]); none when they can't be read.
async fn pod_gpu_types(cfg: &Config) -> Vec<String> {
    use arena_core::provider::{runpod_v2, RunpodApi};
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|k| !k.trim().is_empty()) else { return Vec::new() };
    if !matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V2)) {
        return Vec::new();
    }
    match arena_core::ratelimit::bounded(PRICE_TIMEOUT, runpod_v2::fetch_pod_gpu_types(key)).await {
        Ok(Ok(types)) => types.into_iter().map(|(id, _)| id).collect(),
        _ => Vec::new(),
    }
}

/// RunPod REST v1 validates a create's `gpuTypeIds` against its OpenAPI enum, which lags the
/// live catalog: live (finding #22), `PRO 6000 MIG 24GB` passed the catalog check, ranked
/// first in `offers`, and its create was a `400 … schema requirements`. So on v1, with the
/// enum at hand, a `--gpu` id outside it is refused before anything is listed or created —
/// saying how to get back to v2 (the default), whose create takes it. An empty enum
/// (couldn't be read) refuses nothing: placement then skips an option the API rejects. Pure.
fn v1_creatable_check(ids: &str, catalog: &[arena_core::provider::runpod::GpuType], creatable: &[String]) -> Result<()> {
    if creatable.is_empty() {
        return Ok(());
    }
    let bad: Vec<String> = arena_core::placement::split_list(ids)
        .into_iter()
        .filter(|id| !creatable.iter().any(|c| c == id))
        .map(|id| match catalog.iter().find(|g| g.id == id && !g.display_name.is_empty()) {
            Some(g) => format!("`{}` ({id})", g.display_name),
            None => format!("`{id}`"),
        })
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "--gpu {}: in RunPod's catalog, but REST v1's create API rejects it (not in its gpuTypeIds list) — \
         v2's create takes it: {}, or pick another --gpu (`arena offers runpod` marks what v1 can't create)",
        bad.join(", "),
        arena_core::provider::RUNPOD_V1_REMEDY
    )
}

/// The prices placement can see (read-only), plus notes on where they came from. RunPod:
/// the live catalog — on `RUNPOD_API=v2` one `/v2/catalog/gpus` per requested tier (its stock
/// is per tier; each tier fetched once per run, see [`v2_catalog`]) and no GraphQL; on v1 the
/// GraphQL `gpuTypes` (Phase 0.D) — backed by the preset estimates. Vast: the cheapest offer
/// fitting `base` (disk, CUDA floor) per GPU option, one marketplace search per card, each
/// bounded by [`PRICE_TIMEOUT`]. Shadeform: the cheapest in-stock catalog entry per option
/// (allowed clouds, CUDA floor, `SHADEFORM_MAX_PRICE`), from one catalog read. Hetzner quotes
/// nothing.
async fn fetch_price_book(
    cfg: &Config,
    provider_name: &str,
    req: &arena_core::placement::Request,
    base: &PodSpec,
) -> Result<(arena_core::placement::PriceBook, Vec<String>)> {
    use arena_core::placement::PriceBook;
    use arena_core::provider::{runpod, vast, RunpodApi};
    let clouds = &req.clouds;
    if provider_name == "vast" {
        if cfg.get("VAST_API_KEY").is_none_or(str::is_empty) {
            let note = "no VAST_API_KEY — vast options are unpriced (--max-price can't check them)";
            return Ok((PriceBook::unpriced(), vec![note.into()]));
        }
        // Built as create builds it (`provider::build`), so the plan quotes only hosts create
        // would rent: per tier (`--cloud datacenter,verified`, already Vast's tiers here —
        // `plan_placement`), the default verified-only unless VAST_ALLOW_UNVERIFIED.
        let vast = vast::VastProvider::from_config(cfg)?;
        let mut book = PriceBook::unpriced();
        let mut notes = Vec::new();
        for cloud in clouds {
            let spec = PodSpec { gpu_count: req.gpu_count, cloud_type: cloud.clone(), ..base.clone() };
            let tier = vast.tier_for(&spec)?;
            // Near the proxy: the same search in the region's countries too, at the same time,
            // for the near options' prices.
            let near_spec = req.near.map(|n| PodSpec { region: Some(n.region), ..spec.clone() });
            let near_quotes = async {
                match &near_spec {
                    Some(s) => Some(vast.quote(&req.gpus, s, PRICE_TIMEOUT).await),
                    None => None,
                }
            };
            let (quotes, near_quotes) = tokio::join!(vast.quote(&req.gpus, &spec, PRICE_TIMEOUT), near_quotes);
            let (mut tiered, more) = vast::price_book(&quotes, req.gpu_count, tier);
            notes.extend(more);
            if let (Some(near), Some(near_quotes)) = (req.near, near_quotes) {
                let (with_near, more) = vast::with_near_prices(tiered, &near_quotes, req.gpu_count, near.region, tier);
                tiered = with_near;
                notes.extend(more);
            }
            book = book.merge(tiered);
        }
        return Ok((book, notes));
    }
    if provider_name == "shadeform" {
        if cfg.get("SHADEFORM_API_KEY").is_none_or(str::is_empty) {
            let note = "no SHADEFORM_API_KEY — shadeform options are unpriced (--max-price can't check them)";
            return Ok((PriceBook::unpriced(), vec![note.into()]));
        }
        // One catalog read for every option; built as create builds it, so the plan only
        // quotes clouds, images and prices create would take.
        let spec = PodSpec { gpu_count: req.gpu_count, ..base.clone() };
        let sf = arena_core::provider::shadeform::ShadeformProvider::from_config(cfg)?;
        let quotes = sf.quote(&req.gpus, &spec, PRICE_TIMEOUT).await;
        return Ok(arena_core::provider::shadeform::price_book(&quotes, req.gpu_count, sf.opts()));
    }
    if provider_name != "runpod" {
        let note = format!(
            "{provider_name} quotes no price before create — options are unpriced, tried as listed \
             (--max-price can't check them)"
        );
        return Ok((PriceBook::unpriced(), vec![note]));
    }
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Ok((PriceBook::runpod(Vec::new()), vec!["no RUNPOD_API_KEY — prices are preset estimates (~)".into()]));
    };
    let mut catalogs = Vec::new();
    let mut errs = Vec::new();
    if RunpodApi::from_config(cfg)? == RunpodApi::V2 {
        for cloud in clouds {
            match v2_catalog(cfg, key, cloud).await {
                Ok(types) => catalogs.push((Some(cloud.clone()), types)),
                Err(e) => errs.push(format!("v2 catalog ({cloud}): {e}")),
            }
        }
    } else {
        match bounded_catalog(runpod::fetch_gpu_types(key)).await {
            Ok(types) if !types.is_empty() => catalogs.push((None, types)),
            Ok(_) => errs.push("GraphQL catalog: empty".into()),
            Err(e) => errs.push(format!("GraphQL catalog: {e}")),
        }
    }
    let mut notes = Vec::new();
    if catalogs.is_empty() {
        notes.push(format!("live prices unavailable ({}) — using preset estimates (~)", errs.join("; ")));
    } else if !errs.is_empty() {
        notes.push(format!("partly live ({})", errs.join("; ")));
    }
    Ok((PriceBook::runpod(catalogs), notes))
}

/// Price and order the options for `req` on `provider_name` — read-only. The one planner
/// behind `arena offers`, the create/up dry-runs and their confirm prompt. `base` is the spec
/// the options are layered on (config + overrides): a marketplace's price depends on more
/// than the GPU — Vast prices the disk asked for and only offers hosts above the CUDA floor.
async fn plan_placement(
    cfg: &Config,
    provider_name: &str,
    req: &arena_core::placement::Request,
    base: &PodSpec,
) -> Result<arena_core::placement::OptionPlan> {
    req.validate_for(provider_name)?;
    // Vast: `--cloud` (or config's CLOUD_TYPE, a RunPod tier) as Vast's own tiers, so each
    // option names the hosts its create rents on.
    let mut tier_note = None;
    let vast_req;
    let req = if provider_name == "vast" {
        let (clouds, note) = arena_core::provider::vast::plan_tiers(&req.clouds, !arena_core::provider::vast::unverified_allowed(cfg)?)?;
        tier_note = note;
        vast_req = arena_core::placement::Request { clouds, ..req.clone() };
        &vast_req
    } else {
        req
    };
    // Near the proxy (`NEAR`) here too, so every caller of the shared planner — `offers`
    // included — shows the order create/up would run. No caller plans a request that left
    // NEAR out on purpose: `--keep-trying` without a list takes the single path, which never
    // comes here (nor does `single_request`'s quote).
    let configured = arena_core::near::Near::for_provider(cfg, provider_name)?;
    let req = &req.clone().with_near(configured, provider_name, base);
    let (book, mut notes) = fetch_price_book(cfg, provider_name, req, base).await?;
    if let (Some(n), None, "runpod") = (configured, req.near, provider_name) {
        notes.push(if base.api_extra.as_ref().is_some_and(arena_core::near::runpod_pins_location) {
            format!("near ({}): off — CREATE_EXTRA_JSON / --api-json already says where (data centres, a network volume or countries)", n.region.label())
        } else {
            format!(
                "near ({}): RunPod's COMMUNITY cloud can't be placed by data centre (its hosts have none) — near \
                 applies to SECURE options (--cloud community,secure; NEAR=any turns this note off)",
                n.region.label()
            )
        });
    }
    let mut plan = arena_core::placement::plan_options(req, provider_name, &book);
    plan.notes.splice(0..0, notes.into_iter().chain(tier_note));
    Ok(plan)
}

/// The single-spec path's one option, as a request: the spec's GPU, count and tier.
fn single_request(spec: &PodSpec) -> arena_core::placement::Request {
    use arena_core::placement::{Order, Request};
    Request {
        gpus: vec![spec.gpu_type.clone()],
        clouds: vec![spec.cloud_type.trim().to_uppercase()],
        gpu_count: spec.gpu_count,
        max_price: None,
        order: Order::Listed,
        near: None,
    }
}

/// The single-spec path's one option, as a plan — what an `up --check` replacement creates
/// from when no `--gpu`/`--cloud` list or `--max-price` was given: the same GPU, count and
/// tier the first create used. Unpriced (that path never quoted one).
fn single_option_plan(provider_name: &str, spec: &PodSpec) -> arena_core::placement::OptionPlan {
    arena_core::placement::plan_options(&single_request(spec), provider_name, &arena_core::placement::PriceBook::unpriced())
}

/// Print the `create`/`up` balance preflight ([`money::balance_preflight`]) — on the
/// single-spec path with the spec's live price read alongside ([`single_spec_quote`]).
async fn print_balance_preflight(
    provider: &dyn Provider,
    cfg: &Config,
    bal: &money::BalanceCtx,
    new_pods: usize,
    options: Option<&arena_core::placement::OptionPlan>,
    spec: &PodSpec,
) {
    let quote = single_spec_quote(cfg, provider.name(), spec);
    money::balance_preflight(provider, cfg, bal, new_pods, options, spec, Some(quote)).await.iter().for_each(|l| println!("{l}"));
}

/// The single spec's live price for the `create`/`up` preflight: its one option priced the
/// way placement prices it ([`fetch_price_book`] — RunPod's catalog, v2's per tier (cached for
/// the run) or v1's GraphQL; Vast's cheapest matching offer), within the preflight's bound
/// ([`arena_core::balance::INLINE_TIMEOUT`]); `None` when nothing answered in time. Live, the
/// preflight otherwise stood the A4000 preset ($0.32) in for a $0.17 community card and
/// warned. In this crate's unit tests nothing is fetched: a stray key in a test config must
/// never reach the network (tests hand the preflight a quote of their own).
fn single_spec_quote<'a>(cfg: &'a Config, provider_name: &'a str, spec: &'a PodSpec) -> money::SpecQuote<'a> {
    Box::pin(async move {
        if cfg!(test) {
            return None;
        }
        let req = single_request(spec);
        let fetch = fetch_price_book(cfg, provider_name, &req, spec);
        let (book, _) = arena_core::ratelimit::bounded(arena_core::balance::INLINE_TIMEOUT, fetch).await.ok()?.ok()?;
        Some(arena_core::placement::plan_options(&req, provider_name, &book))
    })
}

/// `provider.describe(spec)` — what the one-GPU path's dry run and confirm say a create is —
/// with a MIG slice named as `pods list`, READY and the summary name it (`1×B300 MIG 34GB`,
/// the option label) instead of by its catalog id (`NVIDIA B300 SXM6 AC MIG 1g.34gb x1`, live
/// G #17). Anything else is the provider's own description, unchanged.
fn describe_create(provider: &dyn Provider, spec: &PodSpec) -> String {
    let desc = provider.describe(spec);
    let mig = spec.gpu_type.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("mig"));
    let id = format!("{} x{}", spec.gpu_type, spec.gpu_count);
    match single_option_plan(provider.name(), spec).options.first() {
        Some(o) if mig && desc.contains(&id) => desc.replacen(&id, &o.label, 1),
        _ => desc,
    }
}

/// The single-spec path's `[created]` line: the pod, and — as the placement path's says it —
/// what it was created on (`1×RTX 3070 COMMUNITY`, the option's label and tier) and its price
/// when the provider reported one. Pure.
fn created_line(pod: &arena_core::Pod, on: Option<&str>) -> String {
    let mut line = format!("[created] {} id={}", pod.name, pod.id);
    if let Some(on) = on {
        line.push_str(&format!(" on {on}"));
    }
    if let Some(c) = pod.cost_per_hr.filter(|c| c.is_finite() && *c > 0.0) {
        line.push_str(&format!(" ({}/h)", arena_core::fleet::fmt_money(arena_core::fleet::currency_symbol(&pod.provider), c)));
    }
    line
}

/// The spec the first create would use: the first name, on placement's first option (or the
/// single spec) — what [`print_create_body`] shows. As `create_pods`/`spec_for` build it.
fn first_create_spec(base: &PodSpec, options: Option<&arena_core::placement::OptionPlan>, name: &str) -> PodSpec {
    match options.and_then(|plan| plan.options.first()) {
        Some(first) => arena_core::placement::spec_for(base, name, first),
        None => {
            let mut spec = base.clone();
            spec.name = name.to_string();
            spec.env.push(("MACHINE_NAME".into(), name.to_string()));
            spec
        }
    }
}

/// The line above an option table: provider, GPUs per pod, order, cap.
fn plan_header(plan: &arena_core::placement::OptionPlan) -> String {
    use arena_core::placement::Order;
    let order = match plan.order {
        Order::Cheapest => "cheapest first",
        Order::Listed => "as listed",
    };
    let cap = plan
        .max_price
        .map(|c| format!(", max {}/h per pod", arena_core::fleet::fmt_money("$", c)))
        .unwrap_or_default();
    format!("Placement options on {} ({} GPU(s) per pod, {order}{cap}):", plan.provider, plan.gpu_count)
}

/// The create/up dry-run for several options: the option table and which names would be
/// attempted, in what way.
fn print_placement_preview(
    provider: &dyn Provider,
    plan: &arena_core::placement::OptionPlan,
    base: &PodSpec,
    names: &[String],
) {
    println!("{}", plan_header(plan));
    print!("{}", arena_core::placement::render_plan(plan));
    let Some(first) = plan.options.first() else {
        println!("[dry-run] nothing would be attempted for {} — no option left to try", names.join(", "));
        return;
    };
    println!(
        "[dry-run] would place {} pod(s) on {}: each name tries options 1→{} in order, one create \
         at a time, keeping the first that has capacity:\n  {}",
        names.len(),
        provider.name(),
        plan.options.len(),
        names.join(", ")
    );
    println!("[dry-run] option 1 is: {}", provider.describe(&arena_core::placement::spec_for(base, "", first)));
}

/// The `[y/N]` text for a multi-option create (the option table is printed just above it).
/// Refuses when the cap left nothing to try.
fn placement_prompt(
    provider: &dyn Provider,
    plan: &arena_core::placement::OptionPlan,
    base: &PodSpec,
    names: &[String],
    then: &str,
) -> Result<String> {
    println!("{}", plan_header(plan));
    print!("{}", arena_core::placement::render_plan(plan));
    let Some(first) = plan.options.first() else {
        anyhow::bail!("nothing to create: no option left to try (see the table above, or `arena offers`)");
    };
    Ok(format!(
        "Will create up to {} pod(s) on {}, each name trying the {} option(s) above in order, one \
         create at a time (option 1: {}){then}:\n  {}",
        names.len(),
        provider.name(),
        plan.options.len(),
        provider.describe(&arena_core::placement::spec_for(base, "", first)),
        names.join(", ")
    ))
}

/// Ordered placement for `names` (see `arena_core::placement::place`): progress lines as it
/// goes, then the per-name summary. Same contract as [`create_with_retry`] (Ctrl+C between
/// rounds via the command's `interrupt`, left pressed for what runs next): every pod made,
/// or — on an auth/other failure — those pods inside [`CreateFailed`], so the caller still
/// syncs the proxy for them. Each [`Made`] carries the option it landed on.
#[allow(clippy::too_many_arguments)]
async fn place_names(
    provider: &dyn Provider,
    cfg: &Config,
    names: Vec<String>,
    topup_target: usize,
    base: &PodSpec,
    plan: &arena_core::placement::OptionPlan,
    retry_mins: u64,
    retry_secs: u64,
    interrupt: &Interrupt,
    on_made: OnMade<'_>,
) -> std::result::Result<Vec<Made>, CreateFailed> {
    use arena_core::placement::{self, End, Outcome, Progress, Rounds, TopUp};
    let rounds = Rounds {
        window: std::time::Duration::from_secs(retry_mins * 60),
        every: std::time::Duration::from_secs(retry_secs.max(1)),
    };
    // -n/-a: retry rounds never top up past the provider-scoped total that was confirmed.
    let topup = (topup_target > 0).then(|| TopUp {
        target: topup_target,
        provider: provider.name().to_string(),
        prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
    });
    let mut show = |p: &Progress| {
        if p.is_created() {
            println!("{}", p.line());
        } else {
            eprintln!("{}", p.line());
        }
        // Hand the pod on the moment it exists (`up` starts its pipeline now).
        if let Progress::Attempt { pod: Some(pod), index, .. } = p {
            on_made(&Made { pod: (*pod).clone(), at: tokio::time::Instant::now(), option: Some(*index) });
        }
    };
    let start = tokio::time::Instant::now();
    let run =
        placement::place(provider, base, &names, plan, rounds, topup.as_ref(), || interrupt.wait(), &mut show).await;
    print!("\n{}", placement::render_summary(plan, &run));
    let (placed, total) = (run.created.len(), names.len());
    // Each pod with its option and create time, from the attempt that made it.
    let made: Vec<Made> = run
        .created
        .iter()
        .map(|pod| {
            let created = run.log.iter().flat_map(|l| &l.attempts).find_map(|a| match &a.outcome {
                Outcome::Created { pod_id } if *pod_id == pod.id => Some((a.option, a.at)),
                _ => None,
            });
            Made {
                pod: pod.clone(),
                at: start + created.map_or(Duration::ZERO, |(_, at)| at),
                option: created.map(|(k, _)| k),
            }
        })
        .collect();
    match run.end {
        End::Filled => println!("Placed {placed} of {total}."),
        End::Exhausted => eprintln!(
            "[stop] no capacity on any option right now (placed {placed}/{total}). \
             Re-run with --retry-mins N to wait for it to free up."
        ),
        End::WindowElapsed => eprintln!("retry window ({retry_mins}m) elapsed — placed {placed} of {total}"),
        End::Interrupted => eprintln!("interrupted — stopping retries with {placed} of {total}"),
        End::Aborted { name, error } => {
            // Every option refused (RunPod v2 403s) is about the key's access, not a bad key.
            let what = if error.kind() == Some(arena_core::ProviderErrorKind::Denied) {
                "access refused"
            } else {
                "authentication failed"
            };
            let error = anyhow::anyhow!("{what} creating {name}: {error}");
            return Err(CreateFailed::new(made, error));
        }
        End::Failed { name, error } => {
            eprintln!("placed {placed}/{total} before failure");
            let error = match name {
                Some(name) => anyhow::anyhow!("creating {name}: {error}"),
                None => anyhow::Error::new(error),
            };
            return Err(CreateFailed::new(made, error));
        }
    }
    Ok(made)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let result = run().await;
    unstage_hetzner_script(&std::env::temp_dir());
    if let Err(e) = &result {
        match e.downcast_ref::<reap::ReapExit>() {
            // Already said in the reaper's own words.
            Some(r) => eprintln!("{}", r.why),
            // As `fn main() -> Result` printed it (the error and its causes).
            None => eprintln!("Error: {e:?}"),
        }
    }
    exit_code(&result)
}

async fn run() -> Result<()> {
    let (cli, provider_named) = help::parse();
    // Before the config: an update needs none, and must work where it's broken.
    if let Cmd::Update { check, dry_run, skip_attestation } = cli.cmd {
        return handle_update(check, dry_run, skip_attestation, cli.yes).await;
    }
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;

    // `config check` must work even when a provider key is missing (that's what it's
    // for), so build the provider lazily — only for commands that actually talk to one.
    let provider = match cli.cmd {
        // The archive builds its own Hetzner client (its own token), and its cron path none.
        Cmd::Config(_) | Cmd::Cron(_) | Cmd::Tui | Cmd::Update { .. } | Cmd::Gpus { .. } | Cmd::Offers { .. } | Cmd::Api(_) | Cmd::Archive(_) | Cmd::Image(_) => None,
        // Fleet-wide: every command spans all configured providers (create still targets
        // --provider). One configured backend behaves like that single provider.
        _ => Some(arena_core::provider::build_fleet(&cli.provider, &cfg, true)?),
    };
    // How every pod-SSH path reaches pods — one for the whole run: real ssh/scp here,
    // `FakeRemote` in tests. Each call carries its own budget (see the `*_TIMEOUT`s).
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::SshRemote);

    match cli.cmd {
        Cmd::Tui => launch_tui(&cli.provider, &cli.config),
        Cmd::Update { .. } => unreachable!("handled before the config is read"),
        Cmd::Plan(c) => handle_plan(c, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Config(ConfigCmd::Check) => config_check(&cfg, &cli.provider, &[hetzner_image_rows(&cfg).await, disk::check_rows(&cfg)].concat()),
        Cmd::Config(c) => handle_config(c, &cfg, &cli.provider, &cli.config),
        Cmd::Cron(c) => handle_cron(c, &cfg, &cli.config, cli.yes, CRONTAB, std::env::var("HOME").ok().as_deref()).await,
        Cmd::Archive(c) => archive::handle_archive(c, &cfg, remote.as_ref(), cli.yes).await,
        Cmd::Image(c) => image::handle_image(c, &cfg, remote, cli.yes, Interrupt::ctrl_c()).await,
        Cmd::Pods(p) => handle_pods(p, provider.unwrap().as_ref(), remote, &cfg, cli.yes).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::SshConfig { proxy, admin, out, install, uninstall, dry_run } => {
            let home = std::env::var("HOME").ok();
            let audience = if admin { Audience::Admin } else { Audience::Participants };
            match (install, uninstall) {
                (Some(path), _) => {
                    let p = provider.unwrap();
                    handle_ssh_config_install(p.as_ref(), &cfg, audience, path.as_deref(), cli.yes, dry_run, home.as_deref()).await
                }
                (None, Some(path)) => handle_ssh_config_uninstall(&cfg, audience, path.as_deref(), cli.yes, dry_run, home.as_deref()),
                (None, None) if admin => handle_ssh_config_admin(provider.unwrap().as_ref(), &cfg, out.as_deref()).await,
                (None, None) => handle_ssh_config(provider.unwrap().as_ref(), &cfg, proxy, out.as_deref()).await,
            }
        }
        Cmd::Snapshot { json, public, out, no_probe } => {
            // The real probe unless `--no-probe`; tests hand in a scripted one.
            let reach = (!no_probe).then(|| {
                Arc::new(arena_core::snapshot::SshPortProbe::default()) as Arc<dyn arena_core::snapshot::Reach>
            });
            handle_snapshot(provider.unwrap().as_ref(), &cfg, json, public, out.as_deref(), reach).await
        }
        Cmd::Keys(k) => handle_keys(k, provider.unwrap().as_ref(), remote, &cfg, cli.yes).await,
        Cmd::Teardown { check, json } => teardown::handle_teardown(provider.unwrap().as_ref(), &cfg, check, json).await,
        Cmd::Balance { cmd, json, check, no_live, verbose } => {
            let bal = money::BalanceCtx::from_config(&cfg);
            let now = arena_core::snapshot::unix_now();
            if let Some(BalanceCmd::Set { account, usd }) = cmd {
                print!("{}", money::balance_set(&cfg, &bal, account, usd, now)?);
                return Ok(());
            }
            let flags = money::BalanceFlags { json, check, no_live, verbose };
            let out = money::balance_report(provider.unwrap().as_ref(), &cfg, &bal, flags, now).await?;
            out.stderr.iter().for_each(|l| eprintln!("{l}"));
            print!("{}", out.stdout);
            if out.code != 0 {
                use std::io::Write;
                std::io::stdout().flush()?;
                std::process::exit(out.code);
            }
            Ok(())
        }
        Cmd::Spend { cmd, by, since, csv, json } => {
            let ctx = spend::SpendCtx::from_config(&cfg);
            let now = arena_core::snapshot::unix_now();
            let fleet = provider.unwrap();
            if let Some(spend::SpendCmd::Assign { target, owner, clear }) = cmd {
                let owner = if clear { None } else { owner.as_deref() };
                print!("{}", spend::spend_assign(fleet.as_ref(), &cfg, &ctx, &target, owner, now).await?);
                return Ok(());
            }
            let keys = openrouter_client(&cfg);
            let flags = spend::SpendFlags { by_machine: by == "machine", since, csv, json };
            let out = spend::spend_report(fleet.as_ref(), &cfg, &ctx, keys.as_ref().map(|k| k as &dyn arena_core::openrouter::KeyApi), &flags, now).await?;
            out.stderr.iter().for_each(|l| eprintln!("{l}"));
            print!("{}", out.stdout);
            Ok(())
        }
        Cmd::Gpus { json } => offers::handle_gpus(&cfg, &cli.provider, json).await,
        Cmd::Api(c) => handle_api(c, &cfg).await,
        Cmd::Offers { view, gpu, cloud, max_price, gpus, server_type, order, all, verbose, json } => {
            let flags = offers::Flags { gpu, cloud, max_price, gpus, server_type, order };
            let view = if all { Some("all") } else { view.as_deref() };
            offers::handle_offers(&cfg, provider_named.then_some(cli.provider.as_str()), view, flags, json, verbose).await
        }
    }
}

/// `arena api get <provider> <path>`: one read-only GET (see [`arena_core::apiget`]). The
/// path is checked before the key is even read; the URL printed to stderr carries no key.
async fn handle_api(cmd: ApiCmd, cfg: &Config) -> Result<()> {
    use arena_core::apiget;
    match cmd {
        ApiCmd::Get { provider, path, raw, limits } => {
            let base = apiget::api_base(&provider, cfg)?;
            let url = apiget::resolve_url(base.url, &path)?;
            apiget::refuse_raw(&base, &path, raw)?;
            let key = cfg
                .get(base.key)
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("{} isn't set (see `arena config check`)", base.key))?;
            eprintln!("GET {url} ({}{})", base.provider, if raw { ", raw" } else { ", secrets redacted" });
            let (status, body) = apiget::get_for(&base, url.clone(), key).await?;
            if limits {
                // Counts only — the headers carry no secret, and the body isn't printed.
                println!("GET {} → HTTP {status}", url.path());
                for line in apiget::limits_report(&url) {
                    println!("{}", apiget::scrub(&line, key));
                }
                return Ok(());
            }
            let out = apiget::render_for(&base, status, &body, raw, key)?;
            if !out.is_empty() {
                println!("{out}");
            }
            Ok(())
        }
    }
}

/// Launch the interactive dashboard, handing it the same provider/config the CLI was
/// invoked with (the TUI reads these from `ARENA_PROVIDER`/`ARENA_CONFIG`). We prefer
/// the `arena-tui` sitting next to this binary (so a workspace install is consistent),
/// falling back to `arena-tui` on `PATH`. On Unix we `exec`-replace this process so the
/// dashboard owns the terminal directly.
fn launch_tui(provider: &str, config: &std::path::Path) -> Result<()> {
    use std::process::Command;

    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("arena-tui")));
    let program = match sibling {
        Some(p) if p.exists() => p.into_os_string(),
        _ => std::ffi::OsString::from("arena-tui"),
    };

    let mut cmd = Command::new(&program);
    cmd.env("ARENA_PROVIDER", provider).env("ARENA_CONFIG", config);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec only returns on failure.
        Err(anyhow::anyhow!(
            "could not launch {}: {}",
            program.to_string_lossy(),
            cmd.exec()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().context("launching arena-tui")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Expand a leading `~/` to `$HOME` for local filesystem ops (the nginx config path uses
/// `~` for the remote `$HOME`; local deploy needs a real path).
fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(h) => format!("{}/{rest}", h.trim_end_matches('/')),
            Err(_) => p.to_string(),
        },
        None => p.to_string(),
    }
}

/// Local date/time from the system clock — honors the box's timezone + DST (cron does
/// too), so a window of "00:00–06:00" means local night. Returns
/// `(today_days, minutes_since_local_midnight, "YYYY-MM-DD", "TZ")`.
fn local_now() -> Result<(i64, u32, String, String)> {
    let out = std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M %Z")
        .output()
        .context("running `date` to read local time")?;
    anyhow::ensure!(out.status.success(), "`date` command failed");
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let ymd = parts.next().context("date: missing day")?.to_string();
    let hm = parts.next().context("date: missing time")?;
    let tz = parts.next().unwrap_or("").to_string();
    let (y, m, d) = arena_core::schedule::parse_ymd(&ymd).context("date: unparseable day")?;
    let today_days = arena_core::schedule::days_from_civil(y, m, d);
    let minutes = arena_core::plan::parse_hm(hm).context("date: unparseable time")?;
    Ok((today_days, minutes, ymd, tz))
}

async fn handle_pods(
    cmd: PodCmd,
    provider: &dyn Provider,
    // How we reach pods (one for the whole command): `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
) -> Result<()> {
    handle_pods_with(cmd, provider, remote, cfg, yes, Interrupt::ctrl_c()).await
}

/// [`handle_pods`] with the command's Ctrl+C handed in — one latch for every phase of a
/// `create`/`up` (tests press a manual one).
async fn handle_pods_with(
    cmd: PodCmd,
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
    interrupt: Interrupt,
) -> Result<()> {
    handle_pods_bal(cmd, provider, remote, cfg, yes, interrupt, &money::BalanceCtx::from_config(cfg)).await
}

/// [`handle_pods_with`] with the balance reads handed in (`pods list`'s footer, the
/// `create`/`up` preflight) — the real ones, or a fake in tests.
async fn handle_pods_bal(
    cmd: PodCmd,
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
    interrupt: Interrupt,
    bal: &money::BalanceCtx,
) -> Result<()> {
    // Every create, stop, start and terminate below is recorded in the spend ledger as it
    // succeeds (`arena spend`), with `--owner` for the instances `create`/`up` make; a
    // `replace`/`migrate` keeps the owner of the machine it replaces.
    let tracking = spend::SpendCtx::from_config(cfg);
    let owner = match &cmd {
        PodCmd::Create { owner, .. } | PodCmd::Up { owner, .. } => owner.owner.as_deref(),
        _ => None,
    };
    let replaces = matches!(cmd, PodCmd::Replace { .. } | PodCmd::Migrate { .. });
    let tracked = spend::Tracked::new(provider, &tracking, spend::owner_rule(owner, replaces));
    let provider: &dyn Provider = &tracked;
    match cmd {
        PodCmd::List { json, probe, no_probe } => {
            // Probe GPU by default for the human table; JSON stays fast/scriptable unless
            // asked. `--no-probe` always wins.
            let (out, warnings) = list_with(provider, &remote, cfg, bal, json, !no_probe && (probe || !json)).await?;
            warnings.iter().for_each(|w| eprintln!("{w}"));
            print!("{out}");
        }

        PodCmd::Create { names, count, add, gpu, gpus, server_type, cloud, max_price, order, disk, volume, image, bootstrap, api_json, reap, owner, skip_proxy, dry_run, keep_trying, retry_mins, retry_secs } => {
            // A reap policy asked for: resolved (one end time for every pod) and its state
            // checked before anything is created; then every create records it.
            let reaping = reap.resolve(cfg, provider.name())?;
            let recorder;
            let provider: &dyn Provider = match &reaping {
                Some((policy, store)) => {
                    recorder = RecordingProvider { inner: provider, policy: *policy, store: store.clone(), set_by: "create" };
                    &recorder
                }
                None => provider,
            };
            if let (Some((policy, _)), true) = (&reaping, dry_run) {
                println!("[dry-run] each pod would get a reap policy: {}", create_policy_line(policy));
            }
            // Bad extra JSON (or a field arena sets itself) fails before any API call.
            let api_extra = create_extra(provider, cfg, api_json.as_deref())?;
            // Sizing flags the create target can't honour fail here too (`--gpu` on Hetzner,
            // `--server-type` elsewhere) — refused, never silently dropped.
            let server_type = sizing_flags(provider.name(), gpu.as_deref(), gpus, server_type)?;
            let (image, vm_image) = image_flag(provider.name(), image)?;
            // A typo'd --gpu fails here, before anything is listed or created.
            let gpu = checked_gpu(cfg, provider.name(), gpu).await?;
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra, server_type, vm_image, ..Default::default() };
            // Several --gpu/--cloud options or a --max-price → ordered placement; otherwise
            // the single-spec path below, unchanged. Decided before anything is listed.
            let (ov, placing) = placement_request(cfg, provider.name(), ov, max_price, order, keep_trying)?;
            // Explicit names take a different path than the -n/-a top-up: create exactly
            // those (minus any that already exist), no name allocation.
            let mut topup_target = 0usize; // provider-scoped total for the -n/-a retry loop
            let names = if !names.is_empty() {
                if count.is_some() || add.is_some() {
                    anyhow::bail!("pass explicit names OR -n/-a, not both");
                }
                resolve_explicit_names(provider, cfg, &names).await?
            } else {
                let plan = plan_create(provider, cfg, resolve_want(count, add)?).await?;
                topup_target = plan.target;
                plan.names
            };
            if names.is_empty() {
                eprintln!("nothing to create (target already met or no new names)");
                return Ok(());
            }
            let (mut spec, options) = match placing {
                Some((req, base)) => {
                    let plan = plan_placement(cfg, provider.name(), &req, &base).await?;
                    bal.remember_gpu_prices(&plan);
                    (base, Some(plan))
                }
                None => (spec_with_overrides(cfg, &ov), None),
            };
            pin_vm_image(provider.name(), cfg, &mut spec).await?;
            check_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]))?;
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = describe_create(provider, &spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
                }
                print_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]));
                warn_no_volume(provider, &spec);
                print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
                owner_plan(&tracking, &owner, &names, true)?;
                println!("\nDry-run only — no pods created (this is a preview).");
                return Ok(());
            }
            let then = if skip_proxy { "" } else { ", then sync the proxy" };
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), describe_create(provider, &spec), names.join(", ")
                ),
            };
            // The account balance for these pods, next to the prompt: advisory (it never
            // blocks, and `--yes` changes nothing), quiet if the balance can't be read.
            print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
            let handing = owner_plan(&tracking, &owner, &names, false)?;
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            hand_keys_over(&tracking, cfg, &owner, &handing).await?;
            // Retry rounds only retry the confirmed names still missing; -n/-a also stop once
            // the provider-scoped total is met (topup_target == 0 marks the explicit case).
            let outcome = match &options {
                Some(plan) => {
                    place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, NO_STREAM)
                        .await
                }
                None => {
                    create_with_retry(
                        provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, NO_STREAM,
                    )
                    .await
                }
            };
            let (created, error) = match outcome {
                Ok(created) => (created, None),
                Err(CreateFailed { created, error }) => (created, Some(error)),
            };
            // A price the create answered with is remembered for the balance (a later
            // listing without one — RunPod v1's — still prices the pod).
            bal.remember_prices(created.iter().map(|m| &m.pod));
            // Fresh pods rarely have an SSH endpoint yet, so this mostly reports them as
            // "not forwarded yet" — but it keeps every create ending with the proxy in step
            // (and a provider that hands out the IP at create, like Hetzner, is wired now).
            // Also after a create that failed part-way: the pods made before it exist.
            if !skip_proxy && !created.is_empty() {
                sync_proxy(cfg, provider, "create").await;
            }
            if let Some(e) = error {
                return Err(e);
            }
        }

        PodCmd::Up { names, count, add, gpu, gpus, server_type, cloud, max_price, order, disk, volume, image, bootstrap, api_json, reap, owner, dry_run, no_wait, keep_trying, retry_mins, retry_secs, no_setup, check, check_attempts, retry_bad_hosts, lock, timeout, endpoint_timeout, interval } => {
            // As `create`: the reap policy is resolved once, before anything is created, and
            // recorded for each pod (replacements included) the moment its create returns.
            let reaping = reap.resolve(cfg, provider.name())?;
            let recorder;
            let provider: &dyn Provider = match &reaping {
                Some((policy, store)) => {
                    recorder = RecordingProvider { inner: provider, policy: *policy, store: store.clone(), set_by: "up" };
                    &recorder
                }
                None => provider,
            };
            if let (Some((policy, _)), true) = (&reaping, dry_run) {
                println!("[dry-run] each pod would get a reap policy: {}", create_policy_line(policy));
            }
            let api_extra = create_extra(provider, cfg, api_json.as_deref())?;
            // --lock where pods can't be locked fails now, not after pods were made.
            if lock {
                let target = arena_core::Pod { provider: provider.name().to_string(), ..Default::default() };
                provider.lock_support(&target).map_err(|e| anyhow::anyhow!("--lock: {}", lock_why(e)))?;
            }
            let server_type = sizing_flags(provider.name(), gpu.as_deref(), gpus, server_type)?;
            let (image, vm_image) = image_flag(provider.name(), image)?;
            let gpu = checked_gpu(cfg, provider.name(), gpu).await?;
            // The machines earlier `up --check`s found bad: a marketplace (Vast) skips their
            // offers; RunPod can't be told, so a pod it places on one is rejected (`up`).
            let avoid_machines = if retry_bad_hosts {
                Vec::new()
            } else {
                arena_core::snapshot::avoided_machines(&up::remembered_bad_hosts(cfg, arena_core::snapshot::unix_now()).0, "vast")
            };
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra, avoid_machines, server_type, vm_image };
            // Several --gpu/--cloud options or a --max-price → ordered placement (as `create`).
            let (ov, placing) = placement_request(cfg, provider.name(), ov, max_price, order, keep_trying)?;
            // Like `create`: explicit names take the direct path; -n/-a top up by count.
            let explicit = !names.is_empty();
            if explicit && (count.is_some() || add.is_some()) {
                anyhow::bail!("pass explicit names OR -n/-a, not both");
            }
            let mut topup_target = 0usize; // provider-scoped total for the -n/-a retry loop
            let names = if explicit {
                resolve_explicit_names(provider, cfg, &names).await?
            } else {
                let plan = plan_create(provider, cfg, resolve_want(count, add)?).await?;
                topup_target = plan.target;
                plan.names
            };
            if names.is_empty() {
                eprintln!("nothing to create (target already met or no free names)");
                return Ok(());
            }

            // The pipelines' config is checked *before* creating (billing) pods, not after: a
            // malformed SETUP_TIMEOUT_SECS / MIN_DRIVER_VERSION fails here.
            let setup_timeouts = if no_setup {
                arena_core::setup::SetupTimeouts::default()
            } else {
                arena_core::setup::SetupTimeouts::from_config(cfg, None)?
            };
            let policy = if check { Some(arena_core::health::HealthPolicy::from_config(cfg)?) } else { None };
            // --endpoint-timeout / ENDPOINT_TIMEOUT_MINS, checked before anything bills.
            let endpoint_mins = arena_core::pipeline::endpoint_timeout_mins(cfg, endpoint_timeout.map(u64::from))?;
            let (mut spec, options) = match placing {
                Some((req, base)) => {
                    let plan = plan_placement(cfg, provider.name(), &req, &base).await?;
                    bal.remember_gpu_prices(&plan);
                    (base, Some(plan))
                }
                None => (spec_with_overrides(cfg, &ov), None),
            };
            pin_vm_image(provider.name(), cfg, &mut spec).await?;
            check_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]))?;
            let keys = up_keys_stage(cfg, KEYS_DIR, !no_setup);
            let per_pod =
                up::pipeline_text(!no_setup, check.then_some(check_attempts), keys.is_some(), lock, (endpoint_mins, check_attempts));
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = describe_create(provider, &spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
                }
                print_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]));
                warn_no_volume(provider, &spec);
                print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
                owner_plan(&tracking, &owner, &names, true)?;
                let retry = if retry_mins > 0 { format!(" (retrying up to {retry_mins}m for capacity)") } else { String::new() };
                println!(
                    "\nDry-run only — no pods created (preview){retry}: would create the above, then {per_pod}. \
                     Each pod waits up to {timeout}s for its endpoint."
                );
                return Ok(());
            }
            // Provisioning settings too: a config without ARENA_REPO_* must fail before the
            // create, not strand the new pods after it. Likewise the hetzner bare-VM script's
            // temp file (an unwritable temp dir): staged after the create, its failure ended
            // `up` with pods billing and no pipeline, proxy sync or summary for them. Creates
            // go to the primary backend, so its name says whether a hetzner pod can result.
            let setup = if no_setup {
                None
            } else {
                let (scfg, tokens) =
                    setup_config(cfg, None, None, false, false).context("provisioning settings (or pass --no-setup)")?;
                let hetzner_script = stage_hetzner_script(arena_core::setup::is_bare_vm(provider.name()))?;
                Some((up::SetupStage { scfg, timeouts: setup_timeouts, hetzner_script }, tokens))
            };

            let then = format!(
                "{}, then {per_pod}",
                if retry_mins > 0 { format!(", retrying up to {retry_mins}m") } else { String::new() },
            );
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, &then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), describe_create(provider, &spec), names.join(", ")
                ),
            };
            // The account balance for these pods, next to the prompt: advisory (it never
            // blocks, and `--yes` changes nothing), quiet if the balance can't be read.
            print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
            let handing = owner_plan(&tracking, &owner, &names, false)?;
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            // The handed-over keys are in the CSV before the pipelines copy any.
            let keys = if hand_keys_over(&tracking, cfg, &owner, &handing).await? { up_keys_stage(cfg, KEYS_DIR, !no_setup) } else { keys };
            // What the operator confirmed: a name of these that gets no pod is in the summary
            // (and the exit status) too, not silently missing from it.
            let confirmed = names.clone();
            if no_wait {
                // No pipelines: create (retrying if asked), sync the proxy for what was made.
                let outcome = match &options {
                    Some(plan) => {
                        place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, NO_STREAM)
                            .await
                    }
                    None => {
                        create_with_retry(
                            provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, NO_STREAM,
                        )
                        .await
                    }
                };
                let (created, error) = match outcome {
                    Ok(created) => (created, None),
                    Err(CreateFailed { created, error }) => (created, Some(error)),
                };
                // As `create`: the prices the creates answered with are remembered for the
                // balance — those made before a failure too.
                bal.remember_prices(created.iter().map(|m| &m.pod));
                if !created.is_empty() {
                    sync_proxy(cfg, provider, "up").await;
                }
                if let Some(e) = error {
                    return Err(e);
                }
                if created.is_empty() {
                    anyhow::bail!("no pods were created — nothing to bring up");
                }
                println!("\n--no-wait: not polling. Run `arena proxy apply` once endpoints are assigned.");
                return Ok(());
            }

            // One independent pipeline per pod (see `up`), each started the moment its create
            // returns — while placement may still be retrying for the other names (finding
            // #14: a `--retry-mins` window used to hold every placed pod idle, billing, until it
            // ended). Replacements draw from the confirmed options — on the single-spec path,
            // that one spec as an option.
            let plan_for_run = options.clone().unwrap_or_else(|| single_option_plan(provider.name(), &spec));
            let setup = setup.map(|(stage, tokens)| {
                println!("{tokens}");
                stage
            });
            match &keys {
                Some(k) if k.csv.is_empty() => println!(
                    "API keys: none generated in {KEYS_DIR}/ — each pod gets the backup archive's access once it's set up"
                ),
                Some(k) => println!("API keys: {} — copied to each pod once it's set up", k.csv.join(", ")),
                None if setup.is_some() => println!(
                    "API keys: none generated in {KEYS_DIR}/ — skipping (run `arena keys gen --all` or drop in \
                     <provider>_api_keys.csv, then `pods copy-keys`)."
                ),
                None => {}
            }
            let run = up::UpRun {
                provider,
                remote: remote.clone(),
                cfg,
                options: plan_for_run,
                base: spec.clone(),
                rounds: replacement_rounds(keep_trying, retry_mins, retry_secs),
                timeout: Duration::from_secs(timeout),
                interval: Duration::from_secs(interval.max(1)),
                proxy: proxy_deployable(cfg).await,
                setup,
                check: policy.map(|policy| up::CheckStage {
                    policy,
                    attempts: check_attempts,
                    cmd: arena_core::health::deep_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env"))),
                }),
                keys,
                lock,
                retry_bad_hosts,
                balance: Some(bal),
                reap_notice: reaping.is_some(),
                placements: check_attempts,
                endpoint_timeout: Duration::from_secs(endpoint_mins * 60),
            };
            println!(
                "\nEach pod is brought up on its own the moment it's created (endpoint wait ≤{timeout}s per pod, one up \
                 without an endpoint for {endpoint_mins} min is replaced; Ctrl+C stops — nothing is terminated by it)…"
            );
            let (made_tx, made_rx) = tokio::sync::mpsc::unbounded_channel::<Made>();
            let creating = async {
                // Owned here, so the channel closes — and the pipelines can finish — once the
                // create phase is over.
                let made_tx = made_tx;
                let on_made = move |m: &Made| {
                    // Its price, for the balance, the moment it exists (as a replacement's,
                    // in `up::UpRun`): the pipeline after it may run for many minutes.
                    bal.remember_prices(std::iter::once(&m.pod));
                    let _ = made_tx.send(m.clone());
                };
                match &options {
                    Some(plan) => {
                        place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, &on_made)
                            .await
                    }
                    None => {
                        create_with_retry(
                            provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, &on_made,
                        )
                        .await
                    }
                }
            };
            let (outcome, mut rows) =
                tokio::join!(creating, up::run_up_streaming(&run, made_rx, interrupt.wait(), &up::console));
            // A create that failed part-way still had its pods brought up (and the proxy synced
            // by the pipelines' final sync); the failure is reported after their summary.
            let (created, error) = match outcome {
                Ok(created) => (created, None),
                Err(CreateFailed { created, error }) => (created, Some(error)),
            };
            if created.is_empty() {
                if let Some(e) = error {
                    return Err(e);
                }
                anyhow::bail!("no pods were created — nothing to bring up");
            }
            let made: Vec<String> = created.iter().map(|m| m.pod.name.clone()).collect();
            rows.extend(up::not_created_rows(&confirmed, &made, interrupt.is_set()));
            let concluded = up::conclude(&rows, &up::console);
            if let Some(e) = error {
                return Err(e);
            }
            concluded?;
        }

        PodCmd::Stop { sel, wipe_ok, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("stop")).await?;
            // Every selected pod that's billing is stopped — not just RUNNING: RunPod v2
            // reports real states, and a PROVISIONING/STARTING pod bills too (one status rule
            // for this, the $/H column and the fleet total: `status::is_billing`). A selected
            // pod that isn't billing (already stopped, or a state we don't know) is said, not
            // hidden, and left alone.
            let billing = |p: &arena_core::Pod| arena_core::status::is_billing(&p.status);
            for p in sel.pods.iter().filter(|p| !billing(p)) {
                eprintln!("skip {} — not running or starting ({})", p.name, p.status);
            }
            let billing_pods: Vec<&arena_core::Pod> = sel.pods.iter().filter(|p| billing(p)).collect();
            // A billing pod whose backend has no stop (Shadeform: it bills until deleted) is
            // skipped and said — before the --wipe-ok gate, which would only mislead — and the
            // rest are stopped: one overflow VM must not veto `stop --all` (review SF3). Only a
            // selection with nothing else to stop is refused outright. Afterwards the exit is
            // non-zero while a skipped one still bills (see `no_stop_left`).
            let (no_stop, pods) = split_no_stop(provider, billing_pods);
            if pods.is_empty() && !no_stop.is_empty() {
                anyhow::bail!("{}", no_stop_refusal(&no_stop, "stop"));
            }
            for (p, why) in &no_stop {
                eprintln!("skip {} — {why}", p.name);
            }
            if pods.is_empty() {
                println!("(no running or starting pods to stop)");
                return Ok(());
            }
            // What each stop costs (a RunPod pod keeps no data across stop → start), and
            // which pods would lose everything — those need --wipe-ok, even with --yes.
            let mut fates = Vec::with_capacity(pods.len());
            for p in &pods {
                fates.push(disk_fate(provider, p).await);
            }
            let repo = arena_core::backup::repo_path(cfg);
            // A locked pod refuses a stop (RunPod itself does): the whole batch is refused up
            // front, before anything stops, rather than half-run.
            let locked = arena_core::lock::refusal("stop", &pods, "");
            if let (Some(r), false) = (&locked, dry_run) {
                anyhow::bail!("{r}");
            }
            // Where each pod's repo really is — asked only of pods with a volume, each
            // bounded, all at once.
            let sites = repo_sites(&remote, cfg, &pods, &fates, &repo).await;
            let lines = stop_lines(&pods, &fates, &sites);
            let blocked: Vec<&str> = pods
                .iter()
                .zip(fates.iter().zip(&sites))
                .filter(|(_, (f, site))| f.needs_wipe_ok(site))
                .map(|(p, _)| p.name.as_str())
                .collect();
            if dry_run {
                println!("[dry-run] would stop {} pod(s):\n{}", pods.len(), lines.join("\n"));
                if let Some(r) = &locked {
                    print_would_refuse(r);
                }
                if !blocked.is_empty() && !wipe_ok {
                    println!("(would be refused without --wipe-ok: {})", blocked.join(", "));
                }
                if !no_stop.is_empty() {
                    println!("(would skip, still billing — no stop on their provider: {})", names_of(&no_stop).join(", "));
                }
                println!("\nDry-run only — would stop {} pod(s).", pods.len());
                return Ok(());
            }
            if !blocked.is_empty() && !wipe_ok {
                anyhow::bail!("{}", wipe_refusal(DiskOp::Stop, false, &blocked, &repo));
            }
            if !confirm(yes, &format!("Will stop {} running/starting pod(s):\n{}", pods.len(), lines.join("\n")))? {
                println!("aborted.");
                return Ok(());
            }
            // The fleet provider routes each stop to the backend that owns the pod id. A Hetzner
            // VM shuts down gracefully; one that had to be powered off after all is said.
            let (mut ok, total) = (0, pods.len());
            for p in &pods {
                match provider.stop_pod_outcome(&p.id).await {
                    Ok(outcome) => {
                        println!("[stopped] {}", p.name);
                        if let Some(note) = outcome.note() {
                            eprintln!("  ⚠ {}: {note}", p.name);
                        }
                        ok += 1;
                    }
                    Err(e) => eprintln!("[FAILED] {}: {}", p.name, arena_core::lock::explain(&p.name, &e)),
                }
            }
            println!("\nstopped {ok}/{total}");
            if ok < total {
                anyhow::bail!("{} pod(s) failed to stop", total - ok);
            }
            if let Some(left) = no_stop_left(&no_stop) {
                anyhow::bail!("{left}");
            }
        }

        PodCmd::InitBranches { sel, cohort, week, day, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let sel = cohort_only(cfg, sel, cohort);
            handle_init_branches(remote, cfg, week, day, &sel, dry_run, yes, RSYNC).await?;
        }

        PodCmd::Pull { label, dir, max_size, remote_path, no_git, no_big, target_flag, cohort, sel, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let (label, sel, note) = resolve_pull(provider, cfg, label, sel, &target_flag).await?;
            if let Some(note) = &note {
                eprintln!("{note}");
            }
            let mut sel = cohort_only(cfg, sel, cohort);
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            handle_pull(&remote, cfg, label, &dir, max_size, remote_path, no_git, no_big, &sel, dry_run, yes, RSYNC).await?;
        }

        PodCmd::Baseline { target, week, dir, replace, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            handle_baseline(provider, &remote, cfg, &target, week, &dir, replace, dry_run, yes, RSYNC, now).await?;
        }

        PodCmd::Restore { target, from, path, dir, timeout, overwrite_newer, with_git, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let timeout = timeout.map_or(arena_core::restore::DEFAULT_TIMEOUT, Duration::from_secs);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let opts = RestoreOpts { from, path, dir, timeout, overwrite_newer, with_git, dry_run, yes };
            handle_restore(provider, &remote, cfg, &target, opts, RSYNC, now).await?;
        }

        PodCmd::CopyKeys { sel, keys_dir, hf_token, cc_token, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_copy_keys(remote, cfg, &keys_dir, hf_token, cc_token, &sel, dry_run, yes).await?;
        }

        PodCmd::Cp { file, dest, recursive, sel, timeout, dry_run } => {
            let timeout = timeout.map(Duration::from_secs);
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_copy(remote, cfg, &file, dest.as_deref(), recursive, &sel, timeout, dry_run, yes).await?;
        }

        PodCmd::Restart { target, wipe_ok, no_setup, skip_proxy, dry_run } => {
            let opts = RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes };
            handle_restart(provider, remote, cfg, &target, opts, RESTART_SETTLE, KEYS_DIR).await?;
        }

        PodCmd::Start { no_setup, skip_proxy, dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("start")).await?;
            let opts = RestartOpts { wipe_ok: false, no_setup, skip_proxy, dry_run, yes };
            handle_start(provider, remote, cfg, &sel, opts, RESTART_SETTLE, KEYS_DIR).await?;
        }

        PodCmd::Rename { target, new_name, from_prefix, skip_proxy, dry_run } => {
            let or = openrouter_client(cfg);
            let book = KeyBook::new(OPENROUTER_KEYS_CSV, or.as_ref());
            let request = match (from_prefix, target, new_name) {
                (Some(from), _, _) => RenameRequest::FromPrefix(from),
                (None, Some(old), Some(new)) => RenameRequest::One { old, new },
                _ => anyhow::bail!("pass `<old> <new>` or `--from-prefix <prefix>`"),
            };
            handle_rename(provider, remote, cfg, &book, request, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Lock { dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("lock")).await?;
            handle_lock(provider, &sel, true, dry_run, yes).await?;
        }

        PodCmd::Unlock { dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("unlock")).await?;
            handle_lock(provider, &sel, false, dry_run, yes).await?;
        }

        PodCmd::Reimage { sel, image, wipe_ok, no_setup, skip_proxy, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("reimage")).await?;
            let opts = RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes };
            handle_reimage(provider, remote, cfg, &sel, image, opts, RESTART_SETTLE, KEYS_DIR).await?;
        }

        PodCmd::Replace { target, gpu, gpus, cloud, disk, volume, image, place, keep_old, skip_proxy, dry_run } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap: false, api_extra: None, ..Default::default() };
            let opts = ReplaceOpts { keep_old, skip_proxy, dry_run, yes };
            handle_replace(remote, provider, cfg, &target, &ov, &place, opts, &interrupt, bal).await?;
        }

        PodCmd::Migrate { cmd } => match cmd {
            MigrateCmd::Copy { target, gpu, gpus, cloud, disk, volume, image, bootstrap, place, dry_run } => {
                let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra: None, ..Default::default() };
                handle_migrate_copy(remote, cfg, &target, &ov, &place, dry_run, yes, &interrupt, bal).await?;
            }
            MigrateCmd::Cutover { target, yes: y, skip_proxy, no_final_sync, dry_run } => {
                let opts = CutoverOpts { skip_proxy, no_final_sync, dry_run, yes: yes || y };
                handle_migrate_cutover(remote, provider, cfg, &target, opts).await?;
            }
            MigrateCmd::Finish { target, yes: y, discard_stopped, dry_run } => {
                let opts = FinishOpts { discard_stopped, dry_run, yes: yes || y };
                handle_migrate_finish(remote.as_ref(), provider, cfg, &target, opts).await?;
            }
            MigrateCmd::Revert { target, yes: y, skip_proxy, dry_run } => {
                handle_migrate_revert(remote.as_ref(), provider, cfg, &target, skip_proxy, dry_run, yes || y).await?;
            }
            MigrateCmd::Status { target } => {
                handle_migrate_status(cfg, &target).await?;
            }
        },

        PodCmd::Terminate { target, all, unlock, revoke_key, skip_proxy, dry_run } => {
            let or = openrouter_client(cfg);
            let book = KeyBook::new(OPENROUTER_KEYS_CSV, or.as_ref());
            let target = match (all, target) {
                (true, _) => None,
                (false, Some(t)) => Some(t),
                (false, None) => anyhow::bail!("specify a pod (name or id) to terminate, or pass --all"),
            };
            let opts = TerminateOpts { unlock, revoke_key, skip_proxy, dry_run, yes };
            handle_terminate(provider, cfg, &book, target.as_deref(), opts).await?;
        }

        PodCmd::Backup { sel, cohort, no_pull, dry_run, message } => {
            // Resolved once, before the prompt (a typo fails before asking), and shared by the
            // git push and the pull.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let mut sel = cohort_only(cfg, sel, cohort);
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            handle_full_backup(remote, cfg, &sel, no_pull, message, dry_run, yes, RSYNC).await?;
        }
        PodCmd::Setup { sel, dry_run, force, hf_token, cc_token, zsh_install, no_vscode, timeout } => {
            // Budgets first: a malformed SETUP_TIMEOUT_SECS fails before anything else.
            let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, timeout)?;
            // Likewise the VS Code warm-up's settings (VSCODE_PREINSTALL / VSCODE_EXTENSIONS).
            arena_core::vscode::expected_extensions(cfg)?;
            let vscode = !no_vscode && arena_core::vscode::preinstall_enabled(cfg.get("VSCODE_PREINSTALL"))?;
            // Resolved before the prompt, so a typo fails before asking and the prompt names
            // exactly the pods that will be provisioned.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            if !dry_run
                && !sel.pods.is_empty()
                && !confirm(
                    yes,
                    &format!(
                        "Will provision {} pod(s) over SSH (deploy key, ~/.name, repo{}): {}",
                        sel.pods.len(),
                        if vscode { ", VS Code warm-up" } else { "" },
                        sel.names()
                    ),
                )?
            {
                println!("aborted.");
                return Ok(());
            }
            handle_setup(remote, cfg, !dry_run, force, hf_token, cc_token, zsh_install, no_vscode, timeouts, &sel, KEYS_DIR, Some(provider))
                .await?;
        }
        PodCmd::SetBranch { branch, sel, cohort, hard, dry_run, init_branches } => {
            let sel = sel.args();
            if let Some(why) = hard_non_cohort_refusal(hard, cohort, sel.names_pods()) {
                anyhow::bail!("{why}");
            }
            let sel = select(provider, cfg, &sel, Unscoped::Refuse("switch")).await?;
            let sel = cohort_only(cfg, sel, cohort);
            // The day `--init-branches` would init, before anything switches: a missing start
            // date fails here, not after the reset.
            if init_branches {
                resolve_week_day(cfg, None, None)?;
            }
            let how = SetBranchRun { branch: &branch, hard, dry_run, yes, init_branches };
            set_branch_and_init(remote, cfg, &sel, &how, RSYNC).await?;
        }
        PodCmd::Run { sel, command, timeout, dry_run, background } => {
            if let Some(why) = misplaced_run_flags(&command) {
                anyhow::bail!("{why}");
            }
            let cmd = command.join(" ");
            let budget = Duration::from_secs(timeout);
            let mut sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let names: Vec<(&str, &str)> = sel.pods.iter().map(|p| (p.name.as_str(), short_of(cfg, &p.name))).collect();
            if let Some(why) = pod_name_as_command(&command, &names) {
                anyhow::bail!("{why}");
            }
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            if background {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let id = arena_core::jobs::JobId::generate(now, &cmd);
                let ask = &mut |what: &str| confirm(yes, what);
                jobs::handle_start(remote, cfg, &sel, &cmd, id, dry_run, ask, &mut |l| println!("{l}")).await?;
            } else {
                handle_run(remote, cfg, &sel, &cmd, budget, dry_run, yes, /*confirm*/ true, /*compact*/ false).await?;
            }
        }
        PodCmd::Jobs { kill, sel } => {
            // Ids are checked before anything is listed: one goes into a remote path.
            let (targets, only) = arena_core::jobs::split_job_arg(&sel.targets).map_err(anyhow::Error::msg)?;
            let kill = kill.as_deref().map(arena_core::jobs::JobId::parse).transpose().map_err(anyhow::Error::msg)?;
            if let (Some(k), Some(o)) = (&kill, &only) {
                if k != o {
                    anyhow::bail!("--kill {k} and job {o}: name one job");
                }
            }
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::All).await?;
            match kill {
                Some(id) => {
                    let ask = &mut |what: &str| confirm(yes, what);
                    jobs::handle_kill(remote, cfg, &sel, &id, ask, &mut |l| println!("{l}")).await?
                }
                None => jobs::handle_list(remote, cfg, &sel, only.as_ref(), &mut |l| println!("{l}")).await?,
            }
        }
        PodCmd::Logs { tail, follow, sel } => {
            let (targets, job) = arena_core::jobs::split_job_arg(&sel.targets).map_err(anyhow::Error::msg)?;
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::All).await?;
            // Ctrl+C is only taken over once following starts; until then it ends the command.
            let stop = || interrupt.wait();
            let tail = usize::try_from(tail).unwrap_or(usize::MAX);
            jobs::handle_logs(remote, cfg, &sel, job.as_ref(), tail, follow, jobs::FOLLOW_INTERVAL, stop, &mut |l| println!("{l}"))
                .await?;
        }
        PodCmd::Reap(cmd) => {
            let env = reap::ReapEnv {
                provider,
                remote: remote.clone(),
                cfg,
                yes,
                clock: reap::real::clock(),
                ntp: &reap::real::ntp,
                tz: reap::real::tz(),
                rsync: RSYNC.to_string(),
                nonce: reap::real::nonce(),
                resolve_local: &reap::real::resolve_local,
                out: &|l: &str| println!("{l}"),
                sync_proxy: true,
                config_file: cfg.source().map(std::path::Path::to_path_buf),
            };
            reap::handle_reap(cmd, &env).await?;
        }
        PodCmd::Ask { ai, model, effort, timeout, dry_run, question, cohort, sel } => {
            // Read-only on the pods, but it sends what it reads off this machine: `ASK_AI=off`
            // (or a malformed value) refuses before anything is listed or read.
            let (targets, question) = ask::split_question(&sel.targets, question).map_err(anyhow::Error::msg)?;
            let pick = arena_core::ask::ai::pick(cfg, ai, model.as_deref(), effort).map_err(anyhow::Error::msg)?;
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::Refuse("ask about")).await?;
            let sel = cohort_only(cfg, sel, cohort);
            let run = ask::AskRun { pick, timeout: Duration::from_secs(timeout), dry_run, question };
            let env = arena_core::ask::AiEnv::from_process();
            let stop = || ask::stop_signals(interrupt.wait());
            ask::handle_ask(remote, cfg, &sel, run, &env, stop, &mut |l| println!("{l}"), &mut |l| eprintln!("{l}")).await?;
        }
        PodCmd::Idle { hours, json, sel } => {
            // Read-only: no confirm. The report prints commands; it never runs them.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            money::handle_idle(provider, remote, cfg, &sel, hours, json, &mut |l| println!("{l}")).await?;
        }
        PodCmd::Test { deep: true, sel, json, verbose, cohort } => {
            // Read-only: no confirm. Config first: a malformed MIN_DRIVER_VERSION fails before
            // the fleet is even listed. Fleet-wide = this cohort's pods: the check runs CUDA, NCCL
            // and a download on each GPU, never on a staff box's (the daily cron line's run).
            arena_core::health::HealthPolicy::from_config(cfg)?;
            let sel = cohort_only(cfg, select(provider, cfg, &sel.args(), Unscoped::All).await?, cohort);
            handle_deep_test(provider, remote, cfg, &sel, json, verbose).await?;
        }
        PodCmd::Test { deep: false, sel, cohort, .. } => {
            // Read-only: no confirm, compact one-line-per-pod output.
            let sel = cohort_only(cfg, select(provider, cfg, &sel.args(), Unscoped::All).await?, cohort);
            handle_run(
                remote,
                cfg,
                &sel,
                "python -c 'import torch; print(torch.__version__)' 2>&1 || python3 -c 'import torch; print(torch.__version__)'",
                TEST_TIMEOUT,
                false,
                yes,
                false,
                true,
            )
            .await?;
        }
    }
    Ok(())
}

/// `pods reimage`: swap image + re-seed keys in place — a fresh container, so its disk is
/// wiped — then bring the pods back usable like a restart does ([`come_back`]): settled
/// endpoints, setup again, the proxy.
///
/// Gated exactly like restart/stop (live finding #2: a reimage wiped participants' files
/// with only `-y`, and `--all -y` would wipe the cohort): each pod's fate is judged by where
/// its repo really is ([`repo_sites`]), and a pod whose work isn't on a confirmed volume is
/// refused without `--wipe-ok`; the dry run says so. And it re-runs setup afterwards (live
/// finding #8: a reimaged pod was left with no `~/.name`, the public git remote and no
/// deploy key — backups then silently couldn't push).
#[allow(clippy::too_many_arguments)]
async fn handle_reimage(
    provider: &dyn Provider,
    // How the reimaged pods are set up again: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    // The pods to reimage, resolved by the caller (which demands names or --all).
    sel: &Selected,
    image: Option<String>,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes } = opts;
    let pods = &sel.pods;
    if pods.is_empty() {
        println!("(no pods to reimage)");
        return Ok(());
    }
    // A reimage resets the container — exactly what a lock is for ("cannot be stopped or
    // reset"). Refused here, up front, rather than left to an API refusal nobody has
    // verified for the reimage PATCH.
    let refs: Vec<&arena_core::Pod> = pods.iter().collect();
    let locked = arena_core::lock::refusal("reimage", &refs, "");
    if let (Some(r), false) = (&locked, dry_run) {
        anyhow::bail!("{r}");
    }
    let image = image.unwrap_or_else(|| PodSpec::from_config(cfg).image);
    if image.is_empty() {
        anyhow::bail!("no image: pass --image or set RUNPOD_DOCKER_IMAGE");
    }
    let pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    if pubkeys.is_empty() {
        anyhow::bail!("no readable public keys from SHARED_SSH_KEY_PATH/GIT_SSH_KEY_LOCAL/ADMIN_SSH_KEY_PATH — refusing to lock pods out");
    }
    // Setup is what brings a reimaged pod back: make sure it can run before wiping one (and a
    // malformed SETUP_TIMEOUT_SECS fails now, as in `up`).
    let setup = !no_setup;
    let timeouts = if setup {
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("re-running setup after the reimage needs the setup config — fix it, or pass --no-setup")?;
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    } else {
        arena_core::setup::SetupTimeouts::default()
    };
    // What the reimage costs each pod, by where its repo really is (asked of pods with a
    // volume only, bounded, all at once).
    let mut fates = Vec::with_capacity(pods.len());
    for p in pods {
        fates.push(reimage_fate(provider, p).await);
    }
    let repo = arena_core::backup::repo_path(cfg);
    let sites = repo_sites(&remote, cfg, &refs, &fates, &repo).await;
    let lines: Vec<String> = pods
        .iter()
        .zip(fates.iter().zip(&sites))
        .map(|(p, (f, site))| format!("  {} (id={}, {}): {}", p.name, p.id, p.provider, f.describe(DiskOp::Reimage, site)))
        .collect();
    let blocked: Vec<&str> = pods
        .iter()
        .zip(fates.iter().zip(&sites))
        .filter(|(_, (f, site))| f.needs_wipe_ok(site))
        .map(|(p, _)| p.name.as_str())
        .collect();
    let then = afterwards_line(ContainerReset::Yes, setup, skip_proxy);
    println!("image: {image}\nkeys:  {}", pubkeys.iter().map(|k| k.split_whitespace().last().unwrap_or("?")).collect::<Vec<_>>().join(", "));
    if dry_run {
        println!("[dry-run] would reimage {} pod(s):\n{}\n  {then}", pods.len(), lines.join("\n"));
        if let Some(r) = &locked {
            print_would_refuse(r);
        }
        if !blocked.is_empty() && !wipe_ok {
            print_would_refuse(&wipe_refusal(DiskOp::Reimage, false, &blocked, &repo));
        }
        return Ok(());
    }
    if !blocked.is_empty() && !wipe_ok {
        anyhow::bail!("{}", wipe_refusal(DiskOp::Reimage, false, &blocked, &repo));
    }
    if !confirm(yes, &format!("Will reimage {} pod(s) onto {image}:\n{}\n  {then}", pods.len(), lines.join("\n")))? {
        println!("aborted.");
        return Ok(());
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut failed = Vec::new();
    for pod in pods {
        let mut env = match provider.pod_spec(&pod.id).await {
            Ok(spec) => spec.env,
            Err(e) => {
                eprintln!("[failed] {}: reading current env: {e}", pod.name);
                failed.push(pod.name.clone());
                continue;
            }
        };
        env.retain(|(k, _)| k != "PUBLIC_KEY" && k != "MACHINE_NAME");
        env.push(("MACHINE_NAME".into(), pod.name.clone()));
        env.push(("PUBLIC_KEY".into(), pubkeys.join("\n")));
        match arena_core::retry::retrying(&policy, || provider.reimage_pod(&pod.id, &image, &env)).await {
            Ok(()) => println!("[reimaged] {}", pod.name),
            Err(e) => {
                eprintln!("[failed] {}: {}", pod.name, arena_core::lock::explain(&pod.name, &e));
                failed.push(pod.name.clone());
            }
        }
    }
    // The reimaged pods come back on a new SSH port: settle, set up, sync (nothing to do
    // if every reimage failed).
    let done: Vec<arena_core::Pod> = pods.iter().filter(|p| !failed.contains(&p.name)).cloned().collect();
    let back = Comeback { setup, sync: !skip_proxy, settle, timeouts, reset: ContainerReset::Yes };
    let outcome = come_back(provider, remote, cfg, &done, back, keys_dir, "reimage").await;
    if !failed.is_empty() {
        if let Err(e) = &outcome {
            eprintln!("{e:#}");
        }
        anyhow::bail!("{} pod(s) failed to reimage: {}", failed.len(), failed.join(" "));
    }
    outcome
}

/// `pods start`: start the selected STOPPED pods again ([`Provider::start_pod`]), then bring
/// them back usable ([`come_back`]): a RunPod/Vast pod's container disk went at the stop,
/// so it's set up again (unless `--no-setup`) once its endpoint settles, and the proxy is
/// synced. A Hetzner VM kept its disk: only the sync. Pods that aren't stopped are said and
/// left alone. A start the provider refuses (e.g. its host has no free GPU left) is
/// reported per pod and fails the exit; the others carry on.
/// The pods among `pods` whose backend has no stop at all ([`Provider::stop_support`] —
/// Shadeform: an instance bills until it's deleted, so there's nothing to stop or start), each
/// with why; then the rest. Pure (no request): `pods stop`/`start` skip the first and act on the
/// second (review SF3).
fn split_no_stop<'a>(provider: &dyn Provider, pods: Vec<&'a arena_core::Pod>) -> (Vec<(&'a arena_core::Pod, String)>, Vec<&'a arena_core::Pod>) {
    let mut no_stop = Vec::new();
    let mut rest = Vec::new();
    for p in pods {
        match provider.stop_support(p) {
            Ok(()) => rest.push(p),
            Err(e) => no_stop.push((
                p,
                match e {
                    arena_core::Error::NotImplemented(m) => m,
                    other => other.to_string(),
                },
            )),
        }
    }
    (no_stop, rest)
}

fn names_of(no_stop: &[(&arena_core::Pod, String)]) -> Vec<String> {
    no_stop.iter().map(|(p, _)| p.name.clone()).collect()
}

/// The refusal when everything `pods stop`/`start` (`what`) would act on has no stop.
fn no_stop_refusal(no_stop: &[(&arena_core::Pod, String)], what: &str) -> String {
    let why = no_stop.first().map(|(_, w)| w.as_str()).unwrap_or_default();
    format!("can't {what} {} — {why}", names_of(no_stop).join(", "))
}

/// The exit of a `pods stop` that skipped pods with no stop (review SF3): **non-zero**, after
/// every other pod was stopped — what was asked ("stop these") isn't done while one of them
/// still bills, and a script or the operator reading the exit must see that, as for a stop that
/// failed. `None` when nothing was skipped.
fn no_stop_left(no_stop: &[(&arena_core::Pod, String)]) -> Option<String> {
    (!no_stop.is_empty()).then(|| {
        format!(
            "{} pod(s) still billing — their provider has no stop: {} (back up, then `arena pods terminate`)",
            no_stop.len(),
            names_of(no_stop).join(", ")
        )
    })
}

async fn handle_start(
    provider: &dyn Provider,
    // How the started pods are set up again: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let RestartOpts { no_setup, skip_proxy, dry_run, yes, .. } = opts;
    // A pod whose backend has no stop is never stopped, so there's nothing to start: said and
    // skipped — a selection of only such pods is refused (review SF3: never a veto on the rest).
    let (no_stop, startable) = split_no_stop(provider, sel.pods.iter().collect());
    if startable.is_empty() && !no_stop.is_empty() {
        anyhow::bail!("{}", no_stop_refusal(&no_stop, "start"));
    }
    for (p, why) in &no_stop {
        eprintln!("skip {} — {why}", p.name);
    }
    for p in startable.iter().filter(|p| !arena_core::status::is_stopped(&p.status)) {
        eprintln!("skip {} — not stopped ({})", p.name, p.status);
    }
    let pods: Vec<arena_core::Pod> = startable.iter().filter(|p| arena_core::status::is_stopped(&p.status)).map(|p| (*p).clone()).collect();
    if pods.is_empty() {
        println!("(no stopped pods to start)");
        return Ok(());
    }
    let setup = !no_setup;
    let timeouts = if setup && pods.iter().any(|p| provider.restart_wipes_container_disk(p)) {
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("setting the started pods up again needs the setup config — fix it, or pass --no-setup")?;
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    } else {
        arena_core::setup::SetupTimeouts::default()
    };
    let lines: Vec<String> = pods
        .iter()
        .map(|p| {
            let comes = if provider.restart_wipes_container_disk(p) {
                "comes back as a fresh image (its disk went at the stop); only a /workspace volume kept its files"
            } else {
                "disk kept"
            };
            format!("  {} (id={}, {}): {comes}", p.name, p.id, p.provider)
        })
        .collect();
    let wiped = pods.iter().any(|p| provider.restart_wipes_container_disk(p));
    let then = afterwards_line(if wiped { ContainerReset::Yes } else { ContainerReset::No }, setup, skip_proxy);
    if dry_run {
        println!("[dry-run] would start {} pod(s):\n{}\n  {then}", pods.len(), lines.join("\n"));
        return Ok(());
    }
    if !confirm(yes, &format!("Will start {} stopped pod(s):\n{}\n  {then}", pods.len(), lines.join("\n")))? {
        println!("aborted.");
        return Ok(());
    }
    let (mut started, mut failed) = (Vec::new(), Vec::new());
    for p in &pods {
        match provider.start_pod(&p.id).await {
            Ok(()) => {
                println!("[started] {}", p.name);
                started.push(p.clone());
            }
            Err(e) => {
                eprintln!("[FAILED] {}: {}", p.name, arena_core::lock::explain(&p.name, &e));
                failed.push(p.name.clone());
            }
        }
    }
    let (fresh, kept): (Vec<arena_core::Pod>, Vec<arena_core::Pod>) =
        started.into_iter().partition(|p| provider.restart_wipes_container_disk(p));
    let back = Comeback { setup, sync: !skip_proxy, settle, timeouts, reset: ContainerReset::Yes };
    let outcome = come_back(provider, remote, cfg, &fresh, back, keys_dir, "start").await;
    if !kept.is_empty() && fresh.is_empty() && !skip_proxy {
        settled_sync(cfg, provider, &kept, settle, "start").await;
    }
    if !failed.is_empty() {
        if let Err(e) = &outcome {
            eprintln!("{e:#}");
        }
        anyhow::bail!(
            "{} pod(s) failed to start: {} — still stopped (a host with no free GPU left can't start it: \
             retry later, or terminate it and create a new one)",
            failed.len(),
            failed.join(" ")
        );
    }
    outcome
}

/// `pods terminate`'s switches, passed through to [`handle_terminate`] as one value.
#[derive(Debug, Clone, Copy, Default)]
struct TerminateOpts {
    unlock: bool,
    revoke_key: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
}

/// `pods terminate <target>` / `--all`, optionally `--revoke-key`. Revocation runs only for
/// pods that actually terminated, after the terminates and the proxy sync — a failed revoke
/// is reported per machine and fails the exit, but never stands in the way of a terminate.
/// Keys go by machine *name*, so a name still held by a pod that is NOT gone (the twin of a
/// double create removed by id, or one whose terminate failed) keeps its key and row: a pod
/// still running keeps a working key.
///
/// A locked pod ([`arena_core::lock`]) is refused — the whole command, before anything is
/// terminated — unless `--unlock`, which unlocks each locked pod right before its terminate
/// (and says so in the confirm text). One whose unlock fails isn't terminated; one unlocked
/// whose terminate then fails is reported as left UNLOCKED. A pod locked since the listing
/// (the API refuses its terminate as locked) is, under `--unlock`, unlocked and retried once.
///
/// With `--all`, `--unlock` lifts only this cohort's locks ([`Naming::is_cohort`]): a staff or
/// personal box on the same account (an `@` list entry, or a pod off the list) is often
/// locked precisely so that a fleet-wide command can't take it — RunPod's own refusal kept it
/// alive through `terminate --all` — and the end-of-program one-liner mustn't undo that. Such
/// a pod is KEPT (not unlocked, not terminated) and the exit fails; naming it as the one
/// target (`pods terminate <it> --unlock`) is the operator choosing to.
async fn handle_terminate(
    provider: &dyn Provider,
    cfg: &Config,
    book: &KeyBook<'_>,
    // One pod (the shared single-pod matcher), or `None` = the whole fleet (`--all`).
    target: Option<&str>,
    opts: TerminateOpts,
) -> Result<()> {
    let TerminateOpts { unlock, revoke_key, skip_proxy, dry_run, yes } = opts;
    // Before anything is listed or touched: revoking needs the provisioning API.
    if revoke_key && book.api.is_none() {
        anyhow::bail!(
            "--revoke-key needs OPENROUTER_PROVISIONING_KEY (keys are found and deleted through the \
             provisioning API) — set it, or drop --revoke-key and `arena keys revoke` later"
        );
    }
    let naming = Naming::from_config(cfg);
    let policy = arena_core::retry::RetryPolicy::default();
    // The whole listing is kept: it says which names other pods still hold.
    let listed = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pods: Vec<arena_core::Pod> = match target {
        None => {
            let mut all = listed.clone();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            all
        }
        // One pod, matched like every other command's targets (the shared selector's
        // single-pod rule): a typo names the closest pods, a range/`all` is refused, and a
        // name two pods share is refused — pass the id.
        Some(t) => vec![listed[arena_core::selector::resolve_one(&naming, &listed, t)?].clone()],
    };
    if pods.is_empty() {
        println!("(no pods to terminate)");
        return Ok(());
    }
    // Whose lock `--unlock` may lift: the one named pod, or (`--all`) a cohort pod's.
    let may_unlock = |p: &arena_core::Pod| target.is_some() || naming.is_cohort(&p.name);
    let foreign_locked: Vec<String> =
        pods.iter().filter(|p| arena_core::lock::is_locked(p) && !may_unlock(p)).map(|p| p.name.clone()).collect();
    let kept_why = |names: &[String]| {
        format!(
            "{} KEPT, not terminated: locked, and not this cohort's (`{}-…`) — `--all --unlock` lifts only the \
             cohort's locks (a staff box's lock is what protects it); if it really should go, name it: \
             `arena pods terminate {} --unlock`",
            names.join(", "),
            naming.prefix,
            names.first().map_or("<name>", String::as_str)
        )
    };
    let (kept, pods): (Vec<arena_core::Pod>, Vec<arena_core::Pod>) =
        pods.into_iter().partition(|p| unlock && arena_core::lock::is_locked(p) && !may_unlock(p));
    let kept_names: Vec<String> = kept.iter().map(|p| p.name.clone()).collect();
    if pods.is_empty() {
        // Only kept pods were selected: nothing to do, and the operator asked for more.
        if dry_run {
            print_would_refuse(&kept_why(&kept_names));
            return Ok(());
        }
        anyhow::bail!("{}", kept_why(&kept_names));
    }
    let label = |p: &arena_core::Pod| format!("{} (id={}, {})", p.name, p.id, p.provider);
    let refs: Vec<&arena_core::Pod> = pods.iter().collect();
    let locked: Vec<&str> = arena_core::lock::locked(refs.iter().copied()).iter().map(|p| p.name.as_str()).collect();
    let refused = if unlock {
        None
    } else {
        let mut extra = " — or pass --unlock to lift the lock as part of the terminate".to_string();
        if !foreign_locked.is_empty() {
            extra.push_str(&format!(
                " (with --all it unlocks only the cohort's: {} would be KEPT, locked — not this cohort's)",
                foreign_locked.join(", ")
            ));
        }
        arena_core::lock::refusal("terminate", &refs, &extra)
    };
    let then_keys = if revoke_key {
        format!(", revoke the OpenRouter key(s) named after each and drop their rows from {}", book.csv.display())
    } else {
        String::new()
    };
    let then_sync = if skip_proxy { "" } else { ", then sync the proxy" };
    if dry_run {
        let leaving: Vec<&str> = pods.iter().map(|p| p.id.as_str()).collect();
        let staying = |p: &arena_core::Pod| shared_name_holder(&listed, &leaving, &p.name).map(|o| o.id.clone());
        for p in &pods {
            let how = if unlock && arena_core::lock::is_locked(p) { "unlock, then terminate" } else { "terminate" };
            println!("[dry-run] would {how} {}", label(p));
            if revoke_key {
                match staying(p) {
                    Some(id) => println!("[dry-run]   key kept: pod id {id} (not being terminated) is also named {}", p.name),
                    None => println!(
                        "[dry-run]   then revoke OpenRouter key(s) named {} and drop its row from {}",
                        machine_key_names(&naming, &p.name).join(" / "),
                        book.csv.display()
                    ),
                }
            }
        }
        if let Some(r) = &refused {
            print_would_refuse(r);
        }
        for p in &kept {
            println!("[dry-run] would KEEP {} — locked, not this cohort's", label(p));
        }
        if !kept.is_empty() {
            print_would_refuse(&format!("{} — the exit fails", kept_why(&kept_names)));
        }
        println!("\nDry-run only — would terminate {} pod(s){then_keys}{then_sync} (preview).", pods.len());
        return Ok(());
    }
    if let Some(r) = refused {
        anyhow::bail!("{r}");
    }
    // `--unlock` is named in the prompt, with the pods it unlocks — a lock is lifted only on
    // that explicit yes.
    let unlocking = if unlock && !locked.is_empty() {
        format!("UNLOCK {} (locked), then ", locked.join(", "))
    } else {
        String::new()
    };
    let keeping = if kept.is_empty() {
        String::new()
    } else {
        format!(" KEEPING {} (locked, not this cohort's — not unlocked).", kept_names.join(", "))
    };
    let what = match target {
        None => format!("Will {unlocking}TERMINATE ALL {} pod(s) — irreversible{then_keys}{then_sync}.{keeping}", pods.len()),
        Some(_) => format!("Will {unlocking}TERMINATE {} — irreversible{then_keys}{then_sync}.", label(&pods[0])),
    };
    if !confirm(yes, &what)? {
        println!("aborted.");
        return Ok(());
    }
    // The fleet provider routes each terminate (and unlock) to the backend that owns the pod.
    let mut gone: Vec<&arena_core::Pod> = Vec::new();
    for p in &pods {
        let mut unlocked = false;
        if unlock && arena_core::lock::is_locked(p) {
            if let Err(e) = arena_core::retry::retrying(&policy, || provider.set_locked(&p.id, false)).await {
                let why = format!("{}: unlocking failed, so it was NOT terminated: {e}", p.name);
                if target.is_some() {
                    anyhow::bail!("{why}");
                }
                eprintln!("[FAILED] {why}");
                continue;
            }
            println!("[unlocked] {}", p.name);
            unlocked = true;
        }
        let outcome: std::result::Result<(), String> = match provider.terminate_pod(&p.id).await {
            Ok(()) => Ok(()),
            // Locked since the listing (which said unlocked, or didn't say): under --unlock
            // that is the lock the operator asked to lift — once, then one more terminate.
            // (Not a pod `--all --unlock` may not unlock: that one reads as the hint.)
            Err(e) if e.is_locked() && unlock && !unlocked && may_unlock(p) => {
                match arena_core::retry::retrying(&policy, || provider.set_locked(&p.id, false)).await {
                    Err(ue) => Err(format!(
                        "{}: locked since it was listed, and unlocking failed, so it was NOT terminated: {ue}",
                        p.name
                    )),
                    Ok(()) => {
                        println!("[unlocked] {} (locked since it was listed)", p.name);
                        unlocked = true;
                        provider.terminate_pod(&p.id).await.map_err(|e| format!("{}: {}", p.name, arena_core::lock::explain(&p.name, &e)))
                    }
                }
            }
            Err(e) => Err(format!("{}: {}", p.name, arena_core::lock::explain(&p.name, &e))),
        };
        match outcome {
            Ok(()) => {
                println!("[terminated] {}", if target.is_some() { label(p) } else { p.name.clone() });
                gone.push(p);
            }
            Err(mut why) => {
                if unlocked {
                    why.push_str(&format!(
                        " — it is now UNLOCKED: `arena pods lock {}` to protect it again, or retry the terminate",
                        p.name
                    ));
                }
                // One named pod: its failure is the command's error (nothing else to do).
                if target.is_some() {
                    anyhow::bail!("{why}");
                }
                eprintln!("[FAILED] {why}");
            }
        }
    }
    for p in &kept {
        eprintln!("[KEPT] {} — locked, not this cohort's: not unlocked, not terminated", p.name);
    }
    if target.is_none() {
        println!("\nterminated {}/{}", gone.len(), pods.len() + kept.len());
    }
    // Sync whatever did go (the merge only drops what the providers confirm gone).
    if !gone.is_empty() && !skip_proxy {
        sync_proxy(cfg, provider, "terminate").await;
    }
    let mut revoked = KeyRevoke::default();
    if revoke_key && !gone.is_empty() {
        // Each machine name once (two pods sharing a name are one key), and none that a pod
        // still standing holds — compared against the pre-terminate listing.
        let leaving: Vec<&str> = gone.iter().map(|p| p.id.as_str()).collect();
        let (mut seen, mut machines): (Vec<&str>, Vec<String>) = (Vec::new(), Vec::new());
        for p in &gone {
            if seen.contains(&p.name.as_str()) {
                continue;
            }
            seen.push(&p.name);
            match shared_name_holder(&listed, &leaving, &p.name) {
                Some(other) => println!(
                    "[keys] = {}: key and CSV row kept — pod id {} still has that name (not terminated)",
                    p.name, other.id
                ),
                None => machines.push(p.name.clone()),
            }
        }
        if !machines.is_empty() {
            revoked = revoke_machine_keys(book, &naming, &machines, &spend::SpendCtx::from_config(cfg)).await;
        }
    }
    let mut problems = Vec::new();
    if gone.len() < pods.len() {
        problems.push(format!("{} pod(s) failed to terminate", pods.len() - gone.len()));
    }
    if !kept.is_empty() {
        problems.push(kept_why(&kept_names));
    }
    if !revoked.failed.is_empty() {
        problems.push(format!(
            "revoking the OpenRouter key failed for {} — the pod(s) ARE terminated; retry with `arena keys revoke {}`",
            revoked.failed.join(", "),
            revoked.failed.join(" ")
        ));
    }
    if let Some((names, why)) = &revoked.stale_rows {
        problems.push(format!(
            "the OpenRouter key(s) of {} ARE revoked (or were already gone), but their rows couldn't be \
             removed from {} ({why}) — delete those rows by hand (they hold dead keys)",
            names.join(", "),
            book.csv.display()
        ));
    }
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("; "));
    }
    Ok(())
}

/// What [`revoke_machine_keys`] couldn't do.
#[derive(Debug, Default)]
struct KeyRevoke {
    /// Machines whose key(s) couldn't be revoked (or listed); their CSV rows are kept.
    failed: Vec<String>,
    /// Machines whose keys ARE revoked but whose rows couldn't be removed from the CSV, and
    /// why — a local-file problem, not something `keys revoke` can retry.
    stale_rows: Option<(Vec<String>, String)>,
}

/// Revoke every OpenRouter key that is each of `machines`' (pods just terminated; see
/// [`machine_key_names`]) and drop their rows from the keys CSV. One full key listing for
/// the batch; each key deleted at most once. A machine with no key is said, not failed, and
/// its row (a dead key, if any) is dropped. A row whose key couldn't be deleted stays, as
/// the record that it still works.
async fn revoke_machine_keys(book: &KeyBook<'_>, naming: &Naming<'_>, machines: &[String], tracking: &spend::SpendCtx) -> KeyRevoke {
    let mut out = KeyRevoke::default();
    let Some(api) = book.api else {
        out.failed = machines.to_vec(); // the caller checked up front
        return out;
    };
    let listed_at = arena_core::snapshot::unix_now();
    let keys = match api.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            for m in machines {
                eprintln!("[keys] ✗ {m}: listing OpenRouter keys failed: {e}");
            }
            out.failed = machines.to_vec();
            return out;
        }
    };
    // Each key's usage before any is deleted: a revoked key's spend stays its owner's.
    tracking.keys(&keys, listed_at);
    let mut csv = book.read_csv();
    let mut dropped = Vec::new();
    let mut deleted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in machines {
        let names = machine_key_names(naming, m);
        let mine: Vec<&arena_core::openrouter::KeyInfo> = keys
            .iter()
            .filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|w| w == n)))
            .filter(|k| !deleted.contains(k.hash.as_str()))
            .collect();
        let mut errs = Vec::new();
        for k in &mine {
            match api.delete_key(&k.hash).await {
                Ok(()) => {
                    deleted.insert(k.hash.as_str());
                }
                Err(e) => errs.push(e.to_string()),
            }
        }
        tracking.keys_deleted(&mine, &mine.iter().filter(|k| deleted.contains(k.hash.as_str())).map(|k| k.hash.clone()).collect::<Vec<_>>());
        if !errs.is_empty() {
            eprintln!("[keys] ✗ {m}: revoke failed ({}) — its CSV row is kept", errs.join("; "));
            out.failed.push(m.clone());
            continue;
        }
        let row = match csv.as_deref().and_then(|t| arena_core::apikeys::remove_csv_host(t, m)) {
            Some(t) => {
                csv = Some(t);
                dropped.push(m.clone());
                "; dropped its CSV row"
            }
            None => "",
        };
        match mine.len() {
            0 if row.is_empty() => println!("[keys] = {m}: no OpenRouter key ({}), no CSV row", names.join(" / ")),
            0 => println!("[keys] ✓ {m}: no OpenRouter key named {} (already revoked?){row}", names.join(" / ")),
            n => println!("[keys] ✓ {m}: revoked {n} OpenRouter key(s){row}"),
        }
    }
    if let (Some(text), false) = (&csv, dropped.is_empty()) {
        if let Err(e) = book.write_csv(text) {
            eprintln!("[keys] ✗ couldn't update {}: {e} — rows kept for {}", book.csv.display(), dropped.join(", "));
            out.stale_rows = Some((dropped, e.to_string()));
        }
    }
    out
}

/// One line per pod for `stop`'s dry run and prompt (pure, so tested): which pod, and what
/// the stop does to its disk (judged by where that pod's repo is).
fn stop_lines(pods: &[&arena_core::Pod], fates: &[DiskFate], sites: &[RepoSite]) -> Vec<String> {
    pods.iter()
        .zip(fates)
        .zip(sites)
        .map(|((p, f), site)| format!("  {} (id={}, {}): {}", p.name, p.id, p.provider, f.describe(DiskOp::Stop, site)))
        .collect()
}

/// What a restart (or a stop, then a start) does to one pod's files: what its confirm text
/// says, and whether it needs `--wipe-ok`. Both also take where the ARENA repo is
/// ([`RepoSite`]: the configured `arena_core::backup::repo_path`, refined by the pod's own
/// answer — [`repo_sites`]): the gate is about whether the participants' work survives. With
/// the image's layout (`/root/<repo>`) it is NOT on the volume unless setup moved it there
/// (it does when the pod has one: `arena_core::volume`) — a pod having a `/workspace` volume
/// doesn't by itself make a restart safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskFate {
    /// The backend keeps the disk (a Hetzner VM).
    Kept,
    /// The container is reset to its image. `volume_gb`: the persistent volume (at
    /// /workspace) that survives — `Some(0)` = none, `None` = couldn't be read.
    Wiped { volume_gb: Option<u32> },
    /// A restart that isn't known to reset the container ([`ContainerReset::Unknown`]: RunPod
    /// v1's was seen keeping it, live 2026-10-09): gated and set up again exactly like
    /// [`DiskFate::Wiped`], but only ever said to be a possible wipe.
    MayWipe { volume_gb: Option<u32> },
}

/// The operations that reset a container disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskOp {
    Restart,
    Stop,
    /// A fresh container from an image (`pods reimage`) — a wipe on every backend.
    Reimage,
}

impl DiskFate {
    /// A wipe that the participants' work (the repo, where `site` says) doesn't survive: no
    /// volume, none confirmed (an unreadable spec counts as none: guessing "it has one" is the
    /// data-loss direction), or a volume the repo isn't on.
    fn needs_wipe_ok(self, site: &RepoSite) -> bool {
        match self {
            DiskFate::Kept => false,
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 => !site.on_volume(),
            DiskFate::Wiped { .. } | DiskFate::MayWipe { .. } => true,
        }
    }

    /// What the operation is known to have done to the container disk, for what the command
    /// says afterwards ([`come_back`]).
    fn reset(self) -> ContainerReset {
        match self {
            DiskFate::Kept => ContainerReset::No,
            DiskFate::Wiped { .. } => ContainerReset::Yes,
            DiskFate::MayWipe { .. } => ContainerReset::Unknown,
        }
    }

    /// One line for the prompt / dry run: what's lost and what survives, naming where the
    /// repo is. Pure, so tested.
    fn describe(self, op: DiskOp, site: &RepoSite) -> String {
        use arena_core::provider::runpod_v2::VOLUME_MOUNT_PATH as VOL;
        let wipe = match op {
            DiskOp::Restart if matches!(self, DiskFate::MayWipe { .. }) => {
                "⚠ may WIPE the container disk (this backend's restart isn't known to reset it to the image \
                 every time — counted as if it does)"
            }
            DiskOp::Restart => "⚠ WIPES the container disk (reset to the image)",
            DiskOp::Stop => "⚠ discards the container disk (it starts again as a fresh image)",
            DiskOp::Reimage => "⚠ WIPES the container disk (a fresh container from the image)",
        };
        let rest = "~/.name, setup's git remote, the distributed keys";
        let repo = site.describe();
        let work = format!("the ARENA repo at {repo} (participants' work)");
        // A stopped pod comes back only when someone starts it — no automatic re-setup then.
        let relink = match op {
            DiskOp::Restart | DiskOp::Reimage => "",
            DiskOp::Stop => " (`arena pods start` brings it back and re-links the repo)",
        };
        match self {
            DiskFate::Kept => match op {
                DiskOp::Restart => "disk kept — a hard reset (power-cycle): running processes die, files stay".into(),
                DiskOp::Stop => "disk kept — shut down (powered off only if it hangs), files stay".into(),
                DiskOp::Reimage => "disk kept".into(),
            },
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 && site.on_volume() => format!(
                "{wipe}: everything outside {VOL} is lost ({rest}, files outside the repo); the repo at \
                 {repo} is on the {v} GB volume at {VOL} and survives{relink}"
            ),
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 => format!(
                "{wipe}: everything outside {VOL} is lost — {work} is NOT on the volume, and {rest}; \
                 only the {v} GB volume at {VOL} survives"
            ),
            DiskFate::Wiped { volume_gb: Some(_) } | DiskFate::MayWipe { volume_gb: Some(_) } => {
                format!("{wipe} and it has NO persistent volume: nothing survives — {work}, {rest}")
            }
            DiskFate::Wiped { volume_gb: None } | DiskFate::MayWipe { volume_gb: None } => {
                format!("{wipe}; couldn't read whether it has a volume, so assume nothing survives — {work}, {rest}")
            }
        }
    }
}

/// Where the gate judges each pod's repo to be. Only a wipe *with* a volume makes the answer
/// matter (no volume: nothing survives anyway; a kept disk: nothing is lost), so only those
/// pods are asked — read-only, all at once, each within [`PROBE_TIMEOUT`] — where their repo
/// really lives: setup symlinks it onto the volume, which the configured path alone can't
/// show. No answer (unreachable, no endpoint, an unexpected reply) → the configured path, as
/// before this check.
async fn repo_sites(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    pods: &[&arena_core::Pod],
    fates: &[DiskFate],
    repo: &str,
) -> Vec<RepoSite> {
    let mut sites: Vec<RepoSite> = pods.iter().map(|_| RepoSite::configured(repo)).collect();
    let probe = arena_core::volume::probe_command(repo);
    let jobs: Vec<(usize, SshTarget, String)> = pods
        .iter()
        .zip(fates)
        .enumerate()
        .filter(|(_, (_, fate))| matches!(fate, DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if *v > 0))
        .filter_map(|(i, (pod, _))| SshTarget::from_pod(pod, cfg).ok().map(|t| (i, t, probe.clone())))
        .collect();
    exec_each_pod(remote, jobs, PROBE_TIMEOUT, |_, _, &i, call| {
        match call {
            Ok(out) if out.success => {
                sites[i] = RepoSite::resolve(repo, arena_core::volume::parse_probe(&out.stdout).as_ref());
            }
            _ => {}
        }
    })
    .await;
    sites
}

/// A pod's [`DiskFate`]: the owning backend says whether a restart wipes; if it does, the
/// pod's spec says whether a volume survives (read-only, bounded like a listing — an error
/// or a stall reads as "unknown", which the gate treats like no volume).
async fn disk_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    if !provider.restart_wipes_container_disk(pod) {
        return DiskFate::Kept;
    }
    let spec = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.pod_spec(&pod.id)).await;
    DiskFate::Wiped { volume_gb: match spec {
        Ok(Ok(s)) => Some(s.volume_gb),
        _ => None,
    } }
}

/// A restart's [`DiskFate`]: [`disk_fate`], except that a backend whose restart isn't known to
/// reset the container ([`ContainerReset::Unknown`]: RunPod v1, Vast) gives
/// [`DiskFate::MayWipe`] — the same gate and re-setup, without the claim. The two provider
/// answers agree on what is gated (`ContainerReset::may_wipe`, by the trait's default).
async fn restart_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    match (disk_fate(provider, pod).await, provider.restart_resets_container(pod)) {
        (DiskFate::Wiped { volume_gb }, ContainerReset::Unknown) => DiskFate::MayWipe { volume_gb },
        (fate, _) => fate,
    }
}

/// What a reimage does to a pod's disk: on every backend a fresh container (a VM would be
/// rebuilt), so only a persistent volume survives — whatever `restart_wipes_container_disk`
/// says about a restart. The volume is read from the pod's spec (bounded; unknown = none).
async fn reimage_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    let spec = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.pod_spec(&pod.id)).await;
    DiskFate::Wiped { volume_gb: match spec {
        Ok(Ok(s)) => Some(s.volume_gb),
        _ => None,
    } }
}

/// Why a restart/stop is refused without `--wipe-ok` (pure, so tested): it would destroy
/// the participants' work on these pods, and `--yes` is not an answer to that — the
/// operator has to say so explicitly, after a backup. `maybe`: a restart that isn't known to
/// reset ([`DiskFate::MayWipe`]) — refused all the same, but said as a possibility.
fn wipe_refusal(op: DiskOp, maybe: bool, names: &[&str], repo: &str) -> String {
    let (verb, done) = match op {
        DiskOp::Restart if maybe => (
            "restart",
            "may reset the container disk to the image (this backend's restart isn't known to reset it every time, so it counts as one that does)",
        ),
        DiskOp::Restart => ("restart", "resets the container disk to the image"),
        DiskOp::Stop => ("stop", "discards the container disk (a stopped RunPod pod keeps no data)"),
        DiskOp::Reimage => ("reimage", "replaces the container — and its disk — with a fresh one from the image"),
    };
    let alt = match op {
        DiskOp::Restart | DiskOp::Reimage => "",
        DiskOp::Stop => " If the group is done, terminate instead.",
    };
    format!(
        "refusing to {verb} {} without --wipe-ok: this {done}, and the participants' work — the repo \
         at {repo} — isn't on a confirmed persistent volume (there's no volume, none could be \
         confirmed, or the repo lives outside /workspace), so it would be lost. Back up first \
         (`arena pods backup {}`), then re-run with --wipe-ok (--yes alone isn't enough).{alt}",
        names.join(", "),
        names.join(" ")
    )
}

/// `pods restart`'s switches, passed through to [`handle_restart`] as one value.
#[derive(Debug, Clone, Copy, Default)]
struct RestartOpts {
    wipe_ok: bool,
    no_setup: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
}

/// How long to wait for a restarted pod's SSH endpoint to settle before re-provisioning
/// it. A parameter so tests don't sleep.
#[derive(Debug, Clone, Copy)]
struct SettleWait {
    stable_secs: u64,
    timeout_secs: u64,
    poll: Duration,
}

/// A restarted container can keep answering on its old endpoint for a moment and then
/// move: wait until the endpoint has stayed put for 30 s (as `reimage` does) so setup lands
/// on the *new* container, not the one being torn down — up to 10 min.
const RESTART_SETTLE: SettleWait = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };

/// `pods restart <target>`. On a backend whose restart wipes the container disk (RunPod v2)
/// or may (RunPod v1, Vast: [`restart_fate`]) the prompt says plainly what's lost and what
/// survives, a pod whose repo isn't on a confirmed volume is refused without `--wipe-ok` —
/// even with `--yes` — and afterwards the pod is brought back usable: wait for its endpoint
/// to settle, re-run setup on it (`~/.name`, deploy key + git remote, API keys; `--no-setup`
/// skips), sync the proxy. Only a reset known to happen is said to have. A Hetzner reset
/// keeps the disk: no gate, no re-setup, just the sync.
async fn handle_restart(
    provider: &dyn Provider,
    // How we reach the pod for the re-setup: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    // A malformed SETUP_TIMEOUT_SECS fails before anything is touched (as in `up`).
    let timeouts = if opts.no_setup {
        arena_core::setup::SetupTimeouts::default()
    } else {
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    };
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pod = pods[arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?].clone();
    let label = format!("{} (id={}, {})", pod.name, pod.id, pod.provider);
    // A restart needs a running pod: RunPod v2 answers 409 for a stopped one, v1 answered
    // 2xx and left it stopped (then a 10-min endpoint wait) — live finding #15.
    if arena_core::status::is_stopped(&pod.status) {
        anyhow::bail!("{}", stopped_restart_refusal(&pod, provider.restart_wipes_container_disk(&pod)));
    }
    // A locked pod refuses a restart (RunPod itself does): say so before anything is read.
    let locked = arena_core::lock::refusal("restart", &[&pod], "");
    if let (Some(r), false) = (&locked, opts.dry_run) {
        anyhow::bail!("{r}");
    }
    let fate = restart_fate(provider, &pod).await;
    let repo = arena_core::backup::repo_path(cfg);
    let site = repo_sites(&remote, cfg, &[&pod], &[fate], &repo).await.remove(0);
    let wiped = fate.reset().may_wipe();
    let setup = wiped && !opts.no_setup;
    if setup {
        // Setup is what brings a wiped pod back: make sure it can run before wiping one.
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("re-running setup after the restart needs the setup config — fix it, or pass --no-setup")?;
    }
    let then = afterwards_line(fate.reset(), setup, opts.skip_proxy);
    let what = format!("restart {label}:\n  {}\n  {then}", fate.describe(DiskOp::Restart, &site));
    if opts.dry_run {
        println!("[dry-run] would {what}");
        if let Some(r) = &locked {
            print_would_refuse(r);
        }
        if fate.needs_wipe_ok(&site) && !opts.wipe_ok {
            println!("(would be refused without --wipe-ok)");
        }
        return Ok(());
    }
    if fate.needs_wipe_ok(&site) && !opts.wipe_ok {
        let maybe = matches!(fate, DiskFate::MayWipe { .. });
        anyhow::bail!("{}", wipe_refusal(DiskOp::Restart, maybe, &[pod.name.as_str()], &repo));
    }
    if !confirm(opts.yes, &format!("Will {what}"))? {
        println!("aborted.");
        return Ok(());
    }
    // The fleet provider routes it to the backend that owns the pod id.
    provider.restart_pod(&pod.id).await.map_err(|e| anyhow::anyhow!("{}", arena_core::lock::explain(&pod.name, &e)))?;
    println!("[restarted] {label}");

    let back = Comeback { setup, sync: !opts.skip_proxy, settle, timeouts, reset: fate.reset() };
    if wiped {
        come_back(provider, remote, cfg, std::slice::from_ref(&pod), back, keys_dir, "restart").await
    } else {
        // A Hetzner reset keeps the VM (and its address): no setup, just the sync.
        if back.sync {
            sync_proxy(cfg, provider, "restart").await;
        }
        Ok(())
    }
}

/// Why `restart` refuses a stopped pod, and the way back (pure, tested): `pods start` — which
/// brings back a pod whose container disk went at the stop (`start_wipes`: RunPod, Vast) as a
/// fresh image, set up again, but a Hetzner VM with its disk as it was (live R: the Hetzner
/// refusal promised a fresh image).
fn stopped_restart_refusal(pod: &arena_core::Pod, start_wipes: bool) -> String {
    let comes = if start_wipes {
        "it comes back as a fresh image and is set up again"
    } else {
        "its disk was kept: it comes back as it was, with nothing set up again"
    };
    format!(
        "{} is stopped ({}) — a restart needs a running pod. Start it with `arena pods start {}` ({comes}).",
        pod.name, pod.status, pod.name
    )
}

/// What a lifecycle command that resets containers says it does afterwards (pure, tested):
/// `reset` is what the operation does to the container disk — a possible reset
/// ([`ContainerReset::Unknown`]) is set up again like a certain one, but never said to have
/// left a bare image.
fn afterwards_line(reset: ContainerReset, setup: bool, skip_proxy: bool) -> &'static str {
    use ContainerReset::{No, Unknown, Yes};
    match (reset, setup, skip_proxy) {
        (Yes | Unknown, true, false) => "then: wait for its SSH endpoint to settle, re-run setup on it (~/.name, deploy key + git remote, API keys; a repo with changes or commits of its own is left as it is), sync the proxy",
        (Yes | Unknown, true, true) => "then: wait for its SSH endpoint to settle, re-run setup on it (~/.name, deploy key + git remote, API keys; a repo with changes or commits of its own is left as it is)",
        (Yes, false, false) => "then: wait for its SSH endpoint to settle and sync the proxy (no setup: it comes back as the bare image)",
        (Yes, false, true) => "then: nothing (no setup, no proxy sync: it comes back as the bare image)",
        (Unknown, false, false) => "then: wait for its SSH endpoint to settle and sync the proxy (no setup: if it was reset, it comes back as the bare image)",
        (Unknown, false, true) => "then: nothing (no setup, no proxy sync: if it was reset, it comes back as the bare image)",
        (No, _, false) => "then: sync the proxy",
        (No, _, true) => "then: nothing",
    }
}

/// How [`come_back`] brings reset pods back.
#[derive(Debug, Clone, Copy)]
struct Comeback {
    /// Re-run setup on them (they're back as the bare image otherwise).
    setup: bool,
    /// Sync the proxy afterwards.
    sync: bool,
    settle: SettleWait,
    timeouts: arena_core::setup::SetupTimeouts,
    /// What the operation did to their container disks: `Yes` (a reimage, a start, a RunPod v2
    /// restart) or `Unknown` (a restart that may have kept it) — what the output may claim.
    reset: ContainerReset,
}

/// After a container reset — `restart`, `reimage`, `start` — bring the pods back usable:
/// wait for each one's SSH endpoint to settle (so setup lands on the NEW container, not the
/// one going away), re-run setup on them (`~/.name`, deploy key + git remote, API keys, the
/// repo re-linked onto its volume), then sync the proxy once their endpoints have settled
/// again ([`settled_sync`]). The proxy is synced whatever setup did: the reset happened
/// either way. `Err` = some pod isn't set up again, with the command that fixes it — and a
/// warning not to reset it again, which would only wipe it once more.
async fn come_back(
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    pods: &[arena_core::Pod],
    how: Comeback,
    keys_dir: &str,
    why: &'static str,
) -> Result<()> {
    if pods.is_empty() || !(how.setup || how.sync) {
        if !how.setup && !pods.is_empty() {
            no_setup_note(pods, how.reset);
        }
        return Ok(());
    }
    let s = how.settle;
    let mut settled = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for pod in pods {
        println!("waiting for {}'s SSH endpoint to settle…", pod.name);
        match wait_for_stable_endpoint_every(provider, &pod.id, s.stable_secs, s.timeout_secs, s.poll).await {
            Ok(_) => settled.push(pod.id.clone()),
            Err(e) => problems.push(format!("{}: no SSH endpoint ({e})", pod.name)),
        }
    }
    if how.setup && !settled.is_empty() {
        println!("\n{}", reprovision_line(settled.len(), why, how.reset));
        let setup = match just_these(provider, &settled).await {
            Ok(sel) => handle_setup(remote, cfg, true, false, None, None, false, false, how.timeouts, &sel, keys_dir, Some(provider)).await,
            Err(e) => Err(e),
        };
        if let Err(e) = setup {
            problems.push(format!("{e:#}"));
        }
    }
    if how.sync {
        // Only the pods that settled are waited for again: one that never got an endpoint has
        // had its wait (and is named below) — another one, or two, per pod would only make a
        // dead pod cost the whole command half an hour.
        let back: Vec<arena_core::Pod> = pods.iter().filter(|p| settled.contains(&p.id)).cloned().collect();
        settled_sync(cfg, provider, &back, s, why).await;
    }
    if !how.setup {
        no_setup_note(pods, how.reset);
    }
    if problems.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let (past, again) = match why {
        "restart" => ("restarted", "not another restart"),
        "reimage" => ("reimaged", "not another reimage"),
        _ => ("started", "not a restart"),
    };
    let it = if pods.len() == 1 { "it is" } else { "they are" };
    let wipes = if how.reset == ContainerReset::Unknown { "that may wipe it" } else { "that would wipe it again" };
    anyhow::bail!(
        "{} {past}, but {it} NOT set up again: {}\n  → run `arena pods setup {}` once it answers \
         ({again} — {wipes})",
        names.join(", "),
        problems.join("; "),
        names.join(" ")
    )
}

/// The line before [`come_back`] re-runs setup (pure, tested): a reset is said to have
/// happened only when it is known to (a RunPod v1 restart may have kept the disk).
fn reprovision_line(n: usize, why: &str, reset: ContainerReset) -> String {
    match reset {
        ContainerReset::Unknown => format!("Re-provisioning {n} pod(s) (in case the {why} reset them to the image — it may not have)…"),
        _ => format!("Re-provisioning {n} pod(s) (the {why} reset them to the image)…"),
    }
}

/// `--no-setup`'s reminder after a reset (or a possible one: `reset`).
fn no_setup_note(pods: &[arena_core::Pod], reset: ContainerReset) {
    println!("{}", no_setup_line(pods, reset));
}

/// [`no_setup_note`]'s words (pure, tested).
fn no_setup_line(pods: &[arena_core::Pod], reset: ContainerReset) -> String {
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let back = if reset == ContainerReset::Unknown { "may be back" } else { "back" };
    format!(
        "(--no-setup: {} {back} as the bare image — `arena pods setup {}` restores ~/.name, the git remote and keys)",
        names.join(", "),
        names.join(" ")
    )
}

/// Whether `name` has a stable port on a configured proxy (a MACHINE_NAME_LIST name) — the
/// pods whose forward a sync is expected to route.
fn proxied_name(cfg: &Config, name: &str) -> bool {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    arena_core::proxy::ProxyConfig::from_config(cfg).is_ok()
        && cfg.machine_names.iter().any(|m| arena_core::naming::qualify(prefix, m) == name)
}

/// The pods of `pods` (with a stable port) whose forward `sync` didn't route to them — kept
/// on an old endpoint because the listing showed them without one. Empty when the sync
/// wasn't made (skipped / failed: its own line said so).
fn unrouted<'a>(cfg: &Config, sync: &ProxySync, pods: &'a [arena_core::Pod]) -> Vec<&'a arena_core::Pod> {
    let ProxySync::Synced { routed, .. } = sync else { return Vec::new() };
    pods.iter()
        .filter(|p| proxied_name(cfg, &p.name))
        .filter(|p| !routed.iter().any(|f| f.name == p.name && f.pod_id.as_deref() == Some(p.id.as_str())))
        .collect()
}

/// The proxy sync after a container reset, made once the pods' endpoints have SETTLED: a
/// reset container's endpoint can flap after it first settles (live: the post-restart sync
/// hit a moment the listing showed the pod without one, so its forward stayed on the dead
/// old endpoint). Waits for each pod to hold one endpoint for `settle.stable_secs`
/// (bounded), syncs, and if a pod's forward still isn't routed to it, waits for THAT pod
/// again and syncs once more — not for one whose first wait already ran out (it has no
/// endpoint to wait for). One still not routed is named with the command to run later.
async fn settled_sync(cfg: &Config, provider: &dyn Provider, pods: &[arena_core::Pod], settle: SettleWait, why: &str) -> ProxySync {
    /// Waits for each of `pods`; the ids whose wait ran out.
    async fn wait(provider: &dyn Provider, pods: &[&arena_core::Pod], settle: SettleWait) -> Vec<String> {
        let mut timed_out = Vec::new();
        for pod in pods {
            if let Err(e) =
                wait_for_stable_endpoint_every(provider, &pod.id, settle.stable_secs, settle.timeout_secs, settle.poll).await
            {
                eprintln!("warning: {} has no settled SSH endpoint ({e})", pod.name);
                timed_out.push(pod.id.clone());
            }
        }
        timed_out
    }
    let dead = wait(provider, &pods.iter().collect::<Vec<_>>(), settle).await;
    let mut sync = sync_proxy(cfg, provider, why).await;
    let again: Vec<&arena_core::Pod> = unrouted(cfg, &sync, pods).into_iter().filter(|p| !dead.contains(&p.id)).collect();
    if !again.is_empty() {
        wait(provider, &again, settle).await;
        sync = sync_proxy(cfg, provider, why).await;
    }
    let missing: Vec<&str> = unrouted(cfg, &sync, pods).iter().map(|p| p.name.as_str()).collect();
    if !missing.is_empty() {
        eprintln!(
            "warning: the proxy doesn't route {} to the new endpoint yet (listed without one) — run `arena proxy apply` once it answers",
            missing.join(", ")
        );
    }
    sync
}

/// The one thing that can't follow a name, for the renamed pods that have it: a RunPod or Vast
/// container's `MACHINE_NAME` env var ([`arena_core::setup::machine_name_in_env`]) — not a
/// Hetzner or Shadeform VM's, which has none (prod 2026-10-30: the note was printed for a
/// Hetzner VM). `None` when no renamed pod has one. A pod missing from `pods` is assumed to
/// have one: the note is only ever extra. Pure.
fn machine_name_note(renamed: &[&PlannedRename], pods: &[arena_core::Pod]) -> Option<String> {
    let with_env: Vec<&str> = renamed
        .iter()
        .filter(|r| pods.iter().find(|p| p.id == r.id).is_none_or(|p| arena_core::setup::machine_name_in_env(&p.provider)))
        .map(|r| r.new.as_str())
        .collect();
    let whose = match with_env.len() {
        0 => return None,
        n if n == renamed.len() => "each renamed pod's MACHINE_NAME env var keeps".to_string(),
        _ => format!("the MACHINE_NAME env var of {} keeps", with_env.join(", ")),
    };
    Some(format!(
        "note: {whose} the old name — env only changes with a reimage (which wipes the disk); ~/.name carries the new one."
    ))
}

/// `pods rename`: validate every (old → new) pair, confirm, rename via the provider's
/// metadata-only rename, then bring along everything keyed by the machine name (PLAN 1.D):
/// `~/.name` on the pod — and the hostname of a VM named by it (Hetzner) — the OpenRouter key
/// mapping, and the proxy (stable ports follow names). A container's `MACHINE_NAME` env var
/// can't follow — env is fixed at create/reimage — so the summary says so where there is one.
/// A pod whose backend can't rename ([`Provider::rename_support`]) is refused before anything
/// is asked. Nothing after the provider rename is fatal: the pods are renamed either way, so a
/// pod that can't be reached or a key that can't be moved is reported.
#[allow(clippy::too_many_arguments)]
async fn handle_rename(
    provider: &dyn Provider,
    // How `~/.name` is rewritten: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    book: &KeyBook<'_>,
    request: RenameRequest,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    // Whole-fleet: a new name held by a pod on a provider that didn't answer isn't free.
    let pods = arena_core::provider::list_complete(provider).await.context("listing pods")?;
    let plan = plan_renames(prefix, &cfg.machine_names, &pods, &request)?;
    if plan.is_empty() {
        println!("nothing to rename.");
        return Ok(());
    }
    // Before anything is asked: a batch with a pod its backend can't rename would stop there.
    let unsupported: Vec<String> = plan
        .iter()
        .filter_map(|r| {
            let pod = pods.iter().find(|p| p.id == r.id)?;
            provider.rename_support(pod).err().map(|e| format!("{}: {}", r.old, lock_why(e)))
        })
        .collect();
    if !unsupported.is_empty() {
        anyhow::bail!("refusing to rename anything:\n  {}", unsupported.join("\n  "));
    }
    let summary: Vec<String> = plan.iter().map(|r| format!("  {} → {} (id={})", r.old, r.new, r.id)).collect();
    let steps = rename_followup_steps(prefix, &plan, &pods, book, skip_proxy);
    let what = format!("rename {} pod(s):\n{}\nthen:\n{}", plan.len(), summary.join("\n"), steps.join("\n"));
    if dry_run {
        println!("[dry-run] would {what}");
        if let Some(note) = machine_name_note(&plan.iter().collect::<Vec<_>>(), &pods) {
            println!("{note}");
        }
        println!("[dry-run] nothing changed.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will {what}"))? {
        println!("aborted.");
        return Ok(());
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut done: Vec<&PlannedRename> = Vec::new();
    let mut failure = None;
    for (i, r) in plan.iter().enumerate() {
        if let Err(e) = arena_core::retry::retrying(&policy, || provider.rename_pod(&r.id, &r.new)).await {
            failure = Some(format!(
                "renaming {} → {} failed: {}\n{}",
                r.old,
                r.new,
                arena_core::lock::explain(&r.old, &e),
                rename_failure_hint(&request, i, plan.len())
            ));
            break;
        }
        println!("[renamed] {} → {}", r.old, r.new);
        // The reaper's record (and a pin by this id) follows the new name.
        if let Some(p) = pods.iter().find(|p| p.id == r.id) {
            reap::renamed(cfg, &p.provider, &r.id, &r.new);
        }
        done.push(r);
    }
    // The pods renamed so far get their ~/.name and key mapping even when the batch stopped:
    // a re-run renames only the rest, so these would otherwise keep the old ones for good.
    let missed = rewrite_name_files(remote, cfg, prefix, &done, &pods).await;
    let key_warnings = move_key_mappings(book, &Naming::from_config(cfg), &done, &pods).await;
    let note = machine_name_note(&done, &pods);
    if let Some(failure) = failure {
        // Deliberately no sync on a partial batch: in a `--from-prefix` rename the pods not
        // renamed yet still carry the old prefix, which has no slot in the current list — a
        // sync would drop their forwards. Left alone, every old forward keeps routing (a
        // renamed pod keeps its port: same list index), until the re-run.
        for line in rename_report(note.as_deref(), &missed, &key_warnings) {
            eprintln!("{line}");
        }
        anyhow::bail!("{failure}");
    }
    if skip_proxy {
        println!("(proxy not synced — run `arena proxy apply`)");
    } else {
        sync_proxy(cfg, provider, "rename").await;
    }
    for line in rename_report(note.as_deref(), &missed, &key_warnings) {
        println!("{line}");
    }
    Ok(())
}

/// The follow-up steps `rename` takes after the provider renames, for its dry run and
/// prompt: whose `~/.name` is rewritten (and who has no endpoint to reach), what moves in
/// the keys CSV and on OpenRouter, and the proxy sync. Reads the local CSV (never printing
/// a key) but makes no API call.
fn rename_followup_steps(
    prefix: &str,
    plan: &[PlannedRename],
    pods: &[arena_core::Pod],
    book: &KeyBook<'_>,
    skip_proxy: bool,
) -> Vec<String> {
    use arena_core::setup::short_name;
    let mut out = Vec::new();
    let shorts: Vec<String> = plan.iter().map(|r| format!("{} ← '{}'", r.new, short_name(prefix, &r.new))).collect();
    out.push(format!("  1. rewrite ~/.name (export MACHINE_NAME='<short>') over SSH: {}", shorts.join(", ")));
    let by_hostname: Vec<&str> = plan
        .iter()
        .filter(|r| pods.iter().any(|p| p.id == r.id && arena_core::setup::hostname_is_machine_name(&p.provider)))
        .map(|r| r.new.as_str())
        .collect();
    if !by_hostname.is_empty() {
        out.push(format!("     and set the hostname (a Hetzner VM is named by it; kept across a reboot): {}", by_hostname.join(", ")));
    }
    let no_endpoint: Vec<&str> = plan
        .iter()
        .filter(|r| !pods.iter().any(|p| p.id == r.id && p.ssh_ip.is_some() && p.ssh_port.is_some()))
        .map(|r| r.new.as_str())
        .collect();
    if !no_endpoint.is_empty() {
        out.push(format!(
            "     no SSH endpoint right now (will be reported — re-run `pods setup` later): {}",
            no_endpoint.join(", ")
        ));
    }
    let csv = book.csv.display();
    match book.read_csv() {
        None => out.push(format!("  2. OpenRouter key mapping: no {csv} — nothing to move locally")),
        Some(text) => {
            let pairs: Vec<(&str, &str)> = plan.iter().map(|r| (r.old.as_str(), r.new.as_str())).collect();
            let done: Vec<&PlannedRename> = plan.iter().collect();
            let cross = cross_cohort_dst(book, prefix);
            let mv = plan_csv_move(&text, cross.as_ref().map(|(_, t)| t.as_str()), &pairs, &names_after(pods, &done));
            out.push(match (&cross, mv.moved.is_empty()) {
                (_, true) => format!("  2. OpenRouter key mapping: no rows for these machines in {csv}"),
                (None, false) => format!("  2. OpenRouter key mapping: move rows in {csv}: {}", mv.moved.join(", ")),
                (Some((dst, _)), false) => format!(
                    "  2. OpenRouter key mapping: move rows out of the previous cohort's file into {} and point {csv} \
                     at it: {}{}",
                    dst.display(),
                    mv.moved.join(", "),
                    if mv.carried.is_empty() { String::new() } else { format!(" (carrying along: {})", mv.carried.join(", ")) }
                ),
            });
            if !mv.taken.is_empty() {
                out.push(format!("     ⚠ already have their own row (left alone): {}", mv.taken.join(", ")));
            }
        }
    }
    out.push(match book.api {
        Some(_) => "     and rename each machine's key on OpenRouter to the new name (if it has one)".into(),
        None => "     (no OPENROUTER_PROVISIONING_KEY: key names on OpenRouter are left as they are)".into(),
    });
    out.push(if skip_proxy { "  3. proxy NOT synced (--skip-proxy)".into() } else { "  3. redeploy the proxy".into() });
    out
}

/// What a rename's SSH follow-ups couldn't do, each `(new name, why)`.
#[derive(Debug, Default, PartialEq)]
struct NameMissed {
    /// Pods whose `~/.name` still has the old name.
    name_file: Vec<(String, String)>,
    /// VMs named by their hostname whose hostname is still the old name.
    hostname: Vec<(String, String)>,
}

/// Rewrite `~/.name` on each renamed pod to its new short name — exactly the line setup
/// writes ([`arena_core::setup::name_file_command`]) — and, on a VM named by its hostname
/// (Hetzner: [`arena_core::setup::hostname_is_machine_name`]), set that to the new name too
/// ([`arena_core::setup::hostname_command`]): its prompt, login banner and `~/.name` fallback
/// come from it. Concurrently across pods, each command within `PROBE_TIMEOUT`. The endpoint
/// comes from the pre-rename listing (a metadata rename doesn't move it). Never fails the
/// rename: what it couldn't do comes back for the summary.
async fn rewrite_name_files(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    prefix: &str,
    done: &[&PlannedRename],
    pods: &[arena_core::Pod],
) -> NameMissed {
    use arena_core::setup::{hostname_command, hostname_is_machine_name, name_file_command, short_name};
    /// One command's outcome: `Ok` = it ran and succeeded, else why not.
    fn outcome(res: arena_core::Result<arena_core::ssh::SshOutput>) -> std::result::Result<(), String> {
        match res {
            Ok(o) if o.success => Ok(()),
            Ok(o) => Err(format!("exit {}: {}", o.code.unwrap_or(-1), o.stderr.trim())),
            Err(e) => Err(describe_error(&e)),
        }
    }
    fn report(missed: &mut Vec<(String, String)>, what: &str, name: String, res: std::result::Result<(), String>) {
        match res {
            Ok(()) => println!("[{what}] ✓ {name}"),
            Err(why) => {
                eprintln!("[{what}] ✗ {name}: {why}");
                missed.push((name, why));
            }
        }
    }
    let mut missed = NameMissed::default();
    let mut set = tokio::task::JoinSet::new();
    for r in done {
        let pod = pods.iter().find(|p| p.id == r.id);
        let by_hostname = pod.is_some_and(|p| hostname_is_machine_name(&p.provider));
        let Some(target) = pod.and_then(|p| SshTarget::from_pod(p, cfg).ok()) else {
            report(&mut missed.name_file, "~/.name", r.new.clone(), Err("no SSH endpoint".into()));
            if by_hostname {
                report(&mut missed.hostname, "hostname", r.new.clone(), Err("no SSH endpoint".into()));
            }
            continue;
        };
        let cmd = name_file_command(short_name(prefix, &r.new));
        // `None` when the name isn't one: said, never written into /etc.
        let host_cmd = by_hostname.then(|| hostname_command(&r.new));
        let (remote, name) = (remote.clone(), r.new.clone());
        set.spawn(async move {
            let name_file = outcome(remote.exec(&target, &cmd, Some(PROBE_TIMEOUT)).await);
            let hostname = match host_cmd {
                None => None,
                Some(None) => Some(Err(format!("`{name}` isn't a plain hostname — left as it was"))),
                Some(Some(c)) => Some(outcome(remote.exec(&target, &c, Some(PROBE_TIMEOUT)).await)),
            };
            (name, name_file, hostname)
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((name, name_file, hostname)) => {
                report(&mut missed.name_file, "~/.name", name.clone(), name_file);
                if let Some(h) = hostname {
                    report(&mut missed.hostname, "hostname", name, h);
                }
            }
            Err(e) => report(&mut missed.name_file, "~/.name", "?".into(), Err(format!("task crashed: {e}"))),
        }
    }
    missed
}

/// The keys CSV's side of moving key mappings after a rename (pure, so table-tested; the
/// dry run and the real move share it).
#[derive(Debug, Default, PartialEq)]
struct CsvMove {
    /// The new text of the file the rows end up in; `None`: nothing to write.
    dst: Option<String>,
    /// Cross-cohort only: the old cohort's file with the moved rows taken out.
    src: Option<String>,
    /// `old → new`, per moved row.
    moved: Vec<String>,
    /// New names that already have a row of their own: left alone (and, by the caller,
    /// their OpenRouter key too — the two sides never disagree).
    taken: Vec<String>,
    /// Cross-cohort only: other current pods whose rows were copied into the new file, so
    /// `copy-keys` (which reads through the link) still finds them.
    carried: Vec<String>,
}

/// Plan the CSV half of a rename's key move. `src`: the file the `openrouter_api_keys.csv`
/// link points at now. `dst`: `None` when that is already the current cohort's file — rows
/// are renamed in place; else `Some(text)` of the current cohort's file (the seeded header
/// when it doesn't exist yet): the link points at an *older* cohort's file (a
/// `--from-prefix` rename after a prefix change), and the renamed rows move INTO the current
/// one. Rewriting the old file instead would strand them: the next `keys gen` repoints the
/// link to the current cohort's file and skips these machines (their keys already exist
/// under the new names), so they'd never be delivered again. `live`: every current pod's
/// name after the renames — their rows are carried along so the repointed link loses none.
fn plan_csv_move(src: &str, dst: Option<&str>, pairs: &[(&str, &str)], live: &[String]) -> CsvMove {
    use arena_core::apikeys::{parse_csv, remove_csv_host, rename_csv_host, upsert_csv, CsvRename};
    let mut out = CsvMove::default();
    let Some(dst) = dst else {
        let mut text = src.to_string();
        for (old, new) in pairs {
            match rename_csv_host(&text, old, new) {
                CsvRename::Renamed(t) => {
                    text = t;
                    out.moved.push(format!("{old} → {new}"));
                }
                CsvRename::Taken => out.taken.push(new.to_string()),
                CsvRename::NoRow => {}
            }
        }
        out.dst = (!out.moved.is_empty()).then_some(text);
        return out;
    };
    // The last row wins, as when copy-keys exports them in order.
    let key_of = |text: &str, host: &str| parse_csv(text).into_iter().rev().find(|(h, _)| h == host).map(|(_, k)| k);
    let (mut d, mut s) = (dst.to_string(), src.to_string());
    let mut new_names = Vec::new();
    for (old, new) in pairs {
        let Some(secret) = key_of(&s, old) else { continue };
        if old != new && (key_of(&d, new).is_some() || key_of(&s, new).is_some()) {
            out.taken.push(new.to_string());
            continue;
        }
        d = upsert_csv(&d, new, &secret);
        s = remove_csv_host(&s, old).unwrap_or(s);
        out.moved.push(format!("{old} → {new}"));
        new_names.push(new.to_string());
    }
    if out.moved.is_empty() {
        return out; // nothing of ours in the old file: leave both files and the link alone
    }
    for host in live.iter().filter(|h| !new_names.contains(h)) {
        if let (Some(secret), None) = (key_of(&s, host), key_of(&d, host)) {
            d = upsert_csv(&d, host, &secret);
            out.carried.push(host.clone());
        }
    }
    out.dst = Some(d);
    out.src = Some(s);
    out
}

/// Where a rename's key rows go: `(the current cohort's file, its text or the seeded header)`
/// when the keys link points at another cohort's file (see [`plan_csv_move`]), else `None`
/// (rows move in place). A legacy regular file stays in place, as before.
fn cross_cohort_dst(book: &KeyBook<'_>, prefix: &str) -> Option<(PathBuf, String)> {
    let linked = std::fs::read_link(&book.csv).ok()?;
    let file = cohort_keys_file(prefix);
    if linked.file_name() == Some(std::ffi::OsStr::new(&file)) {
        return None;
    }
    let path = book.csv.with_file_name(&file);
    let text = std::fs::read_to_string(&path).ok().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| cohort_keys_header(prefix));
    Some((path, text))
}

/// Every current pod's name once the renames in `done` are applied (from the pre-rename
/// listing) — whose rows a cross-cohort move carries along.
fn names_after(pods: &[arena_core::Pod], done: &[&PlannedRename]) -> Vec<String> {
    pods.iter().map(|p| done.iter().find(|r| r.id == p.id).map_or_else(|| p.name.clone(), |r| r.new.clone())).collect()
}

/// Move each renamed machine's OpenRouter key mapping to its new name: its row(s) in the
/// keys CSV (into the current cohort's file — see [`plan_csv_move`]) and its key's `name`
/// on OpenRouter (with a provisioning key; found by [`machine_key_names`], as `keys gen`
/// named it), so `copy-keys`, `keys rotate|revoke` and `terminate --revoke-key` — which all
/// go by machine name — keep finding it. Quiet when there's nothing to move (no CSV or no
/// row; no provisioning key or no key under the old name). A new name that already holds
/// its own row or key is left alone and warned about — silently merging two machines' keys
/// is worse than a manual fix — and then neither side moves, so the CSV and OpenRouter stay
/// consistent. Never fails the rename: problems come back as warning lines.
async fn move_key_mappings(
    book: &KeyBook<'_>,
    naming: &Naming<'_>,
    done: &[&PlannedRename],
    pods: &[arena_core::Pod],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if done.is_empty() {
        return warnings;
    }
    let keys = match book.api {
        None => None,
        Some(api) => match api.list_keys().await {
            Ok(k) => Some(k),
            Err(e) => {
                let pairs: Vec<String> = done.iter().map(|r| format!("{} → {}", r.old, r.new)).collect();
                warnings.push(format!(
                    "couldn't list OpenRouter keys ({e}) — key names not moved: {} (rename them on openrouter.ai)",
                    pairs.join(", ")
                ));
                None
            }
        },
    };
    let named = |names: &[String]| -> Vec<&arena_core::openrouter::KeyInfo> {
        keys.iter().flatten().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|w| w == n))).collect()
    };
    // Decide per machine, so a conflict on either side moves neither: OpenRouter first...
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for r in done {
        let mine = named(&machine_key_names(naming, &r.old));
        if !mine.is_empty() && !named(&machine_key_names(naming, &r.new)).is_empty() {
            warnings.push(format!(
                "{} already has its own OpenRouter key on OpenRouter — left {}'s key mapping as it was; keep one \
                 (`arena keys revoke`/`rotate`) and fix the other by hand",
                r.new, r.old
            ));
            continue;
        }
        pairs.push((r.old.as_str(), r.new.as_str()));
    }
    // ...then the CSV.
    let csv_text = book.read_csv();
    let cross = csv_text.as_ref().and_then(|_| cross_cohort_dst(book, naming.prefix));
    let plan = match &csv_text {
        Some(src) => plan_csv_move(src, cross.as_ref().map(|(_, t)| t.as_str()), &pairs, &names_after(pods, done)),
        None => CsvMove::default(),
    };
    for new in &plan.taken {
        let old = pairs.iter().find(|(_, n)| n == new).map_or("", |(o, _)| *o);
        warnings.push(format!(
            "{new} already has its own OpenRouter key row in the CSV — left {old}'s key mapping as it was; keep one \
             (`arena keys revoke`/`rotate`) and fix the other by hand"
        ));
    }
    if let Some(e) = write_csv_move(book, naming.prefix, cross.as_ref().map(|(p, _)| p.as_path()), &plan) {
        warnings.push(e);
    }
    if let Some(api) = book.api {
        for (old, new) in pairs.iter().filter(|(_, n)| !plan.taken.iter().any(|t| t == n)) {
            for k in named(&machine_key_names(naming, old)) {
                match api.rename_key(&k.hash, new).await {
                    Ok(()) => println!("[keys] OpenRouter key {} → {new}", k.name.as_deref().unwrap_or(old)),
                    Err(e) => warnings.push(format!("renaming the OpenRouter key {old} → {new} failed: {e}")),
                }
            }
        }
    }
    warnings
}

/// Write a [`CsvMove`]: in place through the link, or — cross-cohort (`dst` = the current
/// cohort's file) — the new file first (secrets in two places beats in none), then the link
/// repointed at it, then the moved rows taken out of the old file. Each file keeps (a new
/// one inherits) the old file's mode: they hold secrets. A failure stops there, and comes
/// back as the warning saying what state it left.
fn write_csv_move(book: &KeyBook<'_>, prefix: &str, dst: Option<&std::path::Path>, plan: &CsvMove) -> Option<String> {
    let Some(text) = &plan.dst else { return None };
    let moved = plan.moved.join(", ");
    let Some(dst) = dst else {
        return match book.write_csv(text) {
            Ok(()) => {
                println!("[keys] {}: {moved}", book.csv.display());
                None
            }
            Err(e) => Some(format!("couldn't update {} ({e}) — its rows still use the old names: {moved}", book.csv.display())),
        };
    };
    // Resolved before the link moves: afterwards the link leads to the new file.
    let old_real = std::fs::canonicalize(&book.csv).unwrap_or_else(|_| book.csv.clone());
    if let Err(e) = write_secret_file(dst, text) {
        return Some(format!("couldn't write {} ({e}) — key rows NOT moved (still under the old names in {}): {moved}", dst.display(), old_real.display()));
    }
    let dir = book.csv.parent().unwrap_or(std::path::Path::new("."));
    if let Err(e) = ensure_cohort_keys_file(dir, prefix) {
        return Some(format!(
            "copied the key rows into {} but couldn't point {} at it ({e:#}) — copy-keys still reads {}; fix the link by hand",
            dst.display(),
            book.csv.display(),
            old_real.display()
        ));
    }
    let carried = if plan.carried.is_empty() { String::new() } else { format!(" (carried along: {})", plan.carried.join(", ")) };
    println!("[keys] moved into {} (this cohort's keys file): {moved}{carried}", dst.display());
    if let Some(src) = &plan.src {
        if let Err(e) = write_secret_file(&old_real, src) {
            return Some(format!(
                "moved the key rows into {}, but couldn't take them out of {} ({e}) — harmless (copy-keys reads the new file), tidy it by hand",
                dst.display(),
                old_real.display()
            ));
        }
    }
    None
}

/// The closing lines of a rename (pure, so tested): the MACHINE_NAME note where it applies
/// ([`machine_name_note`]), the pods whose `~/.name` or hostname still has the old name (with
/// the command that fixes them), and any key-mapping warnings.
fn rename_report(note: Option<&str>, missed: &NameMissed, key_warnings: &[String]) -> Vec<String> {
    let mut out: Vec<String> = note.map(String::from).into_iter().collect();
    for (what, missed) in [("~/.name still has", &missed.name_file), ("the hostname is still", &missed.hostname)] {
        if missed.is_empty() {
            continue;
        }
        let names: Vec<&str> = missed.iter().map(|(n, _)| n.as_str()).collect();
        let why: Vec<String> = missed.iter().map(|(n, w)| format!("{n}: {w}")).collect();
        out.push(format!(
            "⚠ {what} the old name on {} ({}) — re-run `arena pods setup {}` once it answers",
            names.join(", "),
            why.join("; "),
            names.join(" ")
        ));
    }
    out.extend(key_warnings.iter().map(|w| format!("⚠ keys: {w}")));
    out
}

enum RenameRequest {
    One { old: String, new: String },
    FromPrefix(String),
}

/// What to tell the operator when a rename batch stops part-way (pure, so tested). A
/// `--from-prefix` batch must NOT suggest `proxy apply`: the pods not renamed yet still
/// carry the old prefix, which has no slot in the current MACHINE_NAME_LIST, so any proxy
/// write — `proxy apply`, another lifecycle command's sync, the `cron install --proxy`
/// tick — drops their forwards. Re-running the rename finishes the batch, then syncs.
fn rename_failure_hint(request: &RenameRequest, done: usize, total: usize) -> String {
    match request {
        RenameRequest::One { .. } => {
            format!("{done} of {total} rename(s) done; proxy NOT synced (fix and re-run the rename, or `arena proxy apply`)")
        }
        RenameRequest::FromPrefix(from) => format!(
            "{done} of {total} rename(s) done; proxy NOT synced — every forward still routes. Fix the \
             cause and re-run the rename to finish the batch (it syncs the proxy at the end). Until \
             then do NOT run `arena proxy apply` or another create/terminate/replace: the pods still \
             named {from}-* have no slot in MACHINE_NAME_LIST, so any proxy sync drops their \
             forwards — including the `cron install --proxy` tick (re-install the cron without \
             --proxy to pause it)."
        ),
    }
}

#[derive(Debug, PartialEq)]
struct PlannedRename {
    id: String,
    old: String,
    new: String,
}

/// Pure planner for `pods rename`. Every new name must be a MACHINE_NAME_LIST entry (so it
/// has a stable proxy port) and not held by another pod or another rename in the batch.
/// All problems are collected and reported together; any problem → no plan.
fn plan_renames(
    prefix: &str,
    candidates: &[String],
    pods: &[arena_core::Pod],
    request: &RenameRequest,
) -> Result<Vec<PlannedRename>> {
    use std::collections::HashSet;
    let pairs: Vec<(&arena_core::Pod, String)> = match request {
        RenameRequest::One { old, new } => {
            // The shared single-pod matcher: a typo names the closest pods, and a name two
            // pods share is refused (renaming whichever listed first could be the wrong one).
            let naming = Naming { prefix, list: candidates };
            let pod = &pods[arena_core::selector::resolve_one(&naming, pods, old)?];
            vec![(pod, arena_core::naming::canonical_name(prefix, candidates, new))]
        }
        RenameRequest::FromPrefix(from) => {
            let pre = format!("{from}-");
            pods.iter()
                .filter_map(|p| p.name.strip_prefix(&pre).map(|rest| (p, format!("{prefix}-{rest}"))))
                .filter(|(p, new)| p.name != *new)
                .collect()
        }
    };
    let valid: HashSet<String> = candidates.iter().map(|c| arena_core::naming::qualify(prefix, c)).collect();
    let renamed: HashSet<&str> = pairs.iter().map(|(p, _)| p.id.as_str()).collect();
    // Names still held after the batch: pods not being renamed keep theirs.
    let held: HashSet<&str> =
        pods.iter().filter(|p| !renamed.contains(p.id.as_str())).map(|p| p.name.as_str()).collect();
    let mut seen = HashSet::new();
    let mut problems = Vec::new();
    for (pod, new) in &pairs {
        if pod.name == *new {
            problems.push(format!("{} is already named {new}", pod.name));
        } else if !valid.contains(new) {
            problems.push(format!("{} → {new}: not in MACHINE_NAME_LIST (no stable proxy port)", pod.name));
        } else if held.contains(new.as_str()) {
            problems.push(format!("{} → {new}: name already taken by another pod", pod.name));
        } else if !seen.insert(new.clone()) {
            problems.push(format!("{} → {new}: two pods would get this name", pod.name));
        }
    }
    if !problems.is_empty() {
        anyhow::bail!("refusing to rename anything:\n  {}", problems.join("\n  "));
    }
    Ok(pairs
        .into_iter()
        .map(|(p, new)| PlannedRename { id: p.id.clone(), old: p.name.clone(), new })
        .collect())
}

/// Derive the staging + parked names for a blue-green replace of `canonical`. The
/// replacement is built under `<canonical>-new`; at the swap the source is parked at
/// `<canonical>-old` and the replacement is renamed onto `canonical` itself — so the
/// proxy port / ssh-config identity (which key off the canonical name) carry straight
/// over. Pure so the naming contract is unit-tested.
fn replace_stage_names(canonical: &str) -> (String, String) {
    (format!("{canonical}-new"), format!("{canonical}-old"))
}

/// Build the base spec for a replacement pod: snapshot the source's spec, fall back for what
/// it doesn't report — the GPU type to the source's own (enriched) listing first
/// (`listed_gpu`, see [`source_gpu_from_listing`]), then config; the cloud tier and image to
/// config — re-apply the configured CUDA constraint, re-seed the shared SSH key, then apply
/// the CLI overrides EXCEPT `--gpu` / `--cloud`, which [`plan_replacement`] places (they
/// can be lists). disk/volume/ports/env come from the snapshot so per-pod differences are
/// preserved. Also returns notes for the plan: where the GPU type / tier came from when the
/// source's spec didn't say (`GPU …` / `tier …`).
///
/// The CUDA constraint (the floor from `MIN_CUDA_VERSION` / `ALLOWED_CUDA_VERSIONS`, or
/// `EXACT_CUDA_VERSIONS`) can't come from the snapshot — neither
/// RunPod API reports it back — so without re-applying it a replacement was created
/// unconstrained and could land the cu130 image on a CUDA 12 host (a pod whose torch can't
/// use its GPU). A snapshot that did carry one keeps it.
async fn build_replacement_spec(
    cfg: &Config,
    owner: &dyn Provider,
    src_id: &str,
    src_label: &str,
    listed_gpu: Option<&str>,
    ov: &SpecOverrides,
) -> Result<(PodSpec, Vec<String>)> {
    let mut spec = owner
        .pod_spec(src_id)
        .await
        .with_context(|| format!("snapshotting spec of {src_label}"))?;
    let base = PodSpec::from_config(cfg);
    let mut notes = Vec::new();
    if spec.image.is_empty() {
        spec.image = base.image;
    }
    if spec.gpu_type.is_empty() {
        match listed_gpu {
            Some(g) => {
                notes.push(format!("GPU {g}: the source's, from its listing (its spec snapshot doesn't report one)"));
                spec.gpu_type = g.to_string();
            }
            None if !base.gpu_type.is_empty() => {
                notes.push(format!(
                    "GPU {}: config GPU_TYPE — the source's couldn't be read (pass --gpu to choose)",
                    base.gpu_type
                ));
                spec.gpu_type = base.gpu_type;
            }
            None => {}
        }
    }
    if spec.cloud_type.is_empty() {
        notes.push(format!(
            "tier {}: config CLOUD_TYPE — the API doesn't report the source's (pass --cloud to choose)",
            base.cloud_type
        ));
        spec.cloud_type = base.cloud_type;
    }
    if spec.allowed_cuda.is_empty() && spec.min_cuda.is_none() {
        spec.allowed_cuda = base.allowed_cuda;
        spec.min_cuda = base.min_cuda;
    }
    let pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    if !pubkeys.is_empty() {
        spec.env.retain(|(k, _)| k != "PUBLIC_KEY");
        spec.env.push(("PUBLIC_KEY".to_string(), pubkeys.join("\n")));
    }
    apply_spec_overrides(&mut spec, &SpecOverrides { gpu: None, cloud: None, ..ov.clone() });
    // Config CREATE_EXTRA_JSON is the cohort's create default, like GPU_TYPE: the
    // replacement's create carries it too — checked against the owner's own fields now,
    // before anything is created. (`--api-json` itself is create/up only.)
    if spec.api_extra.is_none() {
        spec.api_extra = create_extra(owner, cfg, None)?;
    }
    check_create_body(owner, &spec)?;
    Ok((spec, notes))
}

/// The source pod's GPU type as a create id, from its (enriched) listing — for when its spec
/// snapshot doesn't say: RunPod v1's REST returns an empty `machine` (live finding #17: a
/// 3070 pod's replacement silently became config's A4000, and a MIG or multi-GPU pod would
/// have become the config default too). The listing's GraphQL details name the card the way
/// RunPod displays it; [`gpu_id_from_listing`] maps that back to an id. Best-effort and
/// bounded; `None` = unknown (the caller falls back to config, and says so).
async fn source_gpu_from_listing(cfg: &Config, owner: &dyn Provider, src: &arena_core::Pod) -> Option<String> {
    let mut pods = vec![src.clone()];
    if pods[0].gpu_type.is_none() {
        let _ = arena_core::ratelimit::bounded(ENRICH_TIMEOUT, owner.enrich(&mut pods)).await;
    }
    let shown = pods.into_iter().next()?.gpu_type?;
    let catalog = if owner.name() == "runpod" { live_gpu_catalog(cfg).await } else { Err(format!("{} has no catalog", owner.name())) };
    gpu_id_from_listing(&shown, catalog)
}

/// The create id for a GPU as a listing shows it (`RTX 3070`, `B300 MIG 34GB`), pure: with
/// RunPod's catalog, the one GPU whose id or display name it is ([`arena_core::gpu::check_gpu_flag`]
/// — an ambiguous one is no answer); without, a known preset only (an alias, `RTX 3070` →
/// `NVIDIA GeForce RTX 3070`). Never a guess: `None` sends the caller to config, saying so.
fn gpu_id_from_listing(shown: &str, catalog: std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String>) -> Option<String> {
    match catalog {
        Ok(types) => arena_core::gpu::check_gpu_flag(shown, &types).ok().filter(|id| !id.trim().is_empty() && !id.contains(',')),
        Err(_) => {
            let id = arena_core::gpu::resolve(shown);
            (id != shown.trim() || arena_core::gpu::find(&id).is_some()).then_some(id)
        }
    }
}

/// A replacement pod's spec and where it may land (`replace`, `migrate copy`).
struct Replacement {
    /// What every option is layered on ([`build_replacement_spec`]).
    base: PodSpec,
    /// The options, in the order they're tried: one, unless `--gpu` / `--cloud` list several
    /// or `--max-price` is set — then the same plan `create`/`up` would make.
    plan: arena_core::placement::OptionPlan,
    /// Where the GPU type / tier came from when not from the source's spec.
    notes: Vec<String>,
}

impl Replacement {
    /// The spec `name` is created from on the first option.
    fn first_spec(&self, name: &str) -> PodSpec {
        first_create_spec(&self.base, Some(&self.plan), name)
    }

    /// More than one option, or priced against a cap: shown as the option table.
    fn is_placement(&self) -> bool {
        self.plan.options.len() > 1 || self.plan.max_price.is_some()
    }
}

/// Plan a replacement for `src` (read-only): its base spec, then where it may land. `--gpu`
/// is checked against the owner's catalog first (RunPod) and, like `--cloud`, may be a list:
/// the options go through the same planner as `create`/`up` (live finding #19: `--gpu
/// 3070,A4000` went to the API as ONE invalid id). Without a list or a cap it's the single
/// option — the source's GPU and tier, or the override.
async fn plan_replacement(
    cfg: &Config,
    owner: &dyn Provider,
    src: &arena_core::Pod,
    ov: &SpecOverrides,
    place: &PlaceFlags,
) -> Result<Replacement> {
    use arena_core::placement::Request;
    let gpu = checked_gpu(cfg, owner.name(), ov.gpu.clone()).await?;
    let listed = if gpu.is_none() { source_gpu_from_listing(cfg, owner, src).await } else { None };
    let label = format!("{} (id={})", src.name, src.id);
    let (base, mut notes) = build_replacement_spec(cfg, owner, &src.id, &label, listed.as_deref(), ov).await?;
    if gpu.is_some() {
        notes.retain(|n| !n.starts_with("GPU "));
    }
    if ov.cloud.is_some() {
        notes.retain(|n| !n.starts_with("tier "));
    }
    let req = Request::from_flags(gpu.as_deref(), ov.cloud.as_deref(), &base, place.max_price, place.order)?
        .with_near(arena_core::near::Near::for_provider(cfg, owner.name())?, owner.name(), &base);
    if owner.name() != "hetzner" && req.gpus.iter().any(|g| g.trim().is_empty()) {
        anyhow::bail!(
            "couldn't determine a GPU type for the replacement (the API doesn't report it and \
             no GPU_TYPE is configured) — pass --gpu <type> (see `arena offers runpod`)"
        );
    }
    let plan = if req.is_single() {
        let one = PodSpec { gpu_type: req.gpus[0].clone(), cloud_type: req.clouds[0].clone(), ..base.clone() };
        single_option_plan(owner.name(), &one)
    } else {
        plan_placement(cfg, owner.name(), &req, &base).await?
    };
    Ok(Replacement { base, plan, notes })
}

/// The replacement's target in a plan: the spec of a single option, or the option table.
fn print_replacement_plan(owner: &dyn Provider, rep: &Replacement, name: &str) {
    if rep.is_placement() {
        println!("  {}", plan_header(&rep.plan));
        for line in arena_core::placement::render_plan(&rep.plan).lines() {
            println!("    {line}");
        }
    } else {
        println!("  target spec: {}", owner.describe(&rep.first_spec(name)));
    }
    for n in &rep.notes {
        println!("  note: {n}");
    }
    if let Some(line) = extra_keys_line(&rep.base) {
        println!("               {line}");
    }
}

/// Create `name` from the replacement's options ([`arena_core::placement::place`], as
/// `create`/`up` do): one create at a time, the first option with capacity wins, retried in
/// rounds while `--retry-mins` allows (Ctrl+C stops between rounds). Nothing else has been
/// touched before it exists; on failure nothing was created. The price its create answered
/// with is remembered for the balance (`balance`) the moment it exists, as `create`/`up` do:
/// the wait, setup and copy after it take minutes, or fail — the pod bills all the same.
async fn create_replacement(
    owner: &dyn Provider,
    rep: &Replacement,
    name: &str,
    place: &PlaceFlags,
    interrupt: &Interrupt,
    balance: Option<&money::BalanceCtx>,
) -> Result<arena_core::Pod> {
    use arena_core::placement::{self, End, Progress};
    let mut show = |p: &Progress| {
        if p.is_created() {
            println!("      {}", p.line());
        } else {
            eprintln!("      {}", p.line());
        }
    };
    let names = [name.to_string()];
    let run = placement::place(owner, &rep.base, &names, &rep.plan, place.rounds(), None, || interrupt.wait(), &mut show).await;
    if let Some(pod) = run.created.into_iter().next() {
        if let Some(bal) = balance {
            bal.remember_prices([&pod]);
        }
        return Ok(pod);
    }
    let (why, hint) = match run.end {
        End::Exhausted => (
            "no capacity on any option right now".to_string(),
            " Re-run with --retry-mins N to wait for it, or with other --gpu/--cloud options (see `arena offers`).",
        ),
        End::WindowElapsed => (format!("no capacity within the {}-minute retry window", place.retry_mins), ""),
        End::Interrupted => ("interrupted".to_string(), ""),
        End::Aborted { error, .. } | End::Failed { error, .. } => (error.to_string(), ""),
        End::Filled => ("no pod came back".to_string(), ""),
    };
    anyhow::bail!("creating {name}: {why} — nothing was created, the original is untouched.{hint}")
}

/// The short machine name of `name` (`devtest-apple` → `apple`).
fn short_of<'a>(cfg: &Config, name: &'a str) -> &'a str {
    arena_core::setup::short_name(cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena"), name)
}

/// Write `~/.name` on pod `id` for the machine `name`, in setup's own format
/// ([`arena_core::setup::name_file_command`], trailing newline included — live finding #26:
/// the swap hand-rolled one without it). Re-resolves the endpoint first (within
/// `wait_secs`). Never on a STOPPED pod — it's listed with the endpoint it had, which may be
/// another pod's by now — and only on the pod itself ([`pod_guard`], in the same session): a
/// write that reached another pod would label it as this machine in backups and metrics.
/// `false` = it couldn't.
async fn write_name_file(remote: &dyn Remote, cfg: &Config, provider: &dyn Provider, id: &str, name: &str, wait_secs: u64) -> bool {
    let listed = provider.list_pods().await.ok().and_then(|ps| ps.into_iter().find(|p| p.id == id));
    if listed.is_some_and(|p| arena_core::status::is_stopped(&p.status)) {
        return false;
    }
    let cmd = arena_core::replica::guarded(
        pod_guard(provider, id).as_deref(),
        &arena_core::setup::name_file_command(short_of(cfg, name)),
    );
    match wait_for_endpoint(provider, id, wait_secs).await {
        Ok(p) => match SshTarget::from_pod(&p, cfg) {
            Ok(t) => remote.exec(&t, &cmd, Some(PROBE_TIMEOUT)).await.is_ok_and(|o| o.success),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Everything keyed by the machine name, after a swap made `promoted` (pod `new_id`) the
/// canonical `canonical` and parked `parked` (pod `old_id`): `~/.name` on both — so they
/// don't both claim the name (#26) — the promoted pod's per-host API keys under its final
/// name (#5), and the note that its `MACHINE_NAME` env var can't follow. Best-effort, every
/// miss said with the command that fixes it.
#[allow(clippy::too_many_arguments)]
async fn after_swap(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    new_id: &str,
    staging: &str,
    parked: &str,
    old_id: &str,
    keys_dir: &str,
) {
    if !write_name_file(remote.as_ref(), cfg, owner, new_id, canonical, 60).await {
        eprintln!(
            "      warning: couldn't update ~/.name on {canonical} — `arena pods setup {canonical}` writes it \
             (until then backups/metrics label it {})",
            short_of(cfg, staging)
        );
    }
    // The parked original still says the canonical name; a stopped one can't be reached
    // (and is about to be terminated anyway), so it's only asked when it's up.
    let old_up = owner
        .list_pods()
        .await
        .ok()
        .and_then(|ps| ps.into_iter().find(|p| p.id == old_id))
        .is_some_and(|p| !arena_core::status::is_stopped(&p.status) && p.ssh_ip.is_some());
    if old_up && !write_name_file(remote.as_ref(), cfg, owner, old_id, parked, 30).await {
        eprintln!("      note: {parked}'s ~/.name still says {} (harmless once it's terminated)", short_of(cfg, canonical));
    }
    println!(
        "      note: {canonical}'s MACHINE_NAME env var still says {staging} — env is fixed at create (only a \
         reimage, which wipes the disk, resets it); ~/.name carries {}.",
        short_of(cfg, canonical)
    );
    rekey_promoted(remote, cfg, owner, new_id, canonical, keys_dir).await;
}

/// Give the promoted pod its per-host API keys under its FINAL name (live finding #5: setup
/// ran while it was `<name>-new`, keys are matched on the exact name, and the copy
/// deliberately leaves the rc files they live in behind — so it silently had none). The
/// keys CSVs are re-distributed to it as `copy-keys <name>` would; when that fails, the
/// command to run is said. Nothing to carry without key files.
async fn rekey_promoted(remote: Arc<dyn Remote>, cfg: &Config, owner: &dyn Provider, id: &str, canonical: &str, keys_dir: &str) {
    let keys = KeySources::load(cfg, keys_dir, None, None);
    if keys.per_host.is_empty() && keys.broadcast.is_empty() && archive::pod_archive(cfg).ready().is_none() {
        return;
    }
    println!("      giving {canonical} its API keys under its new name…");
    let sel = match just_these(owner, &[id.to_string()]).await {
        Ok(mut sel) => {
            // Whatever the listing says this instant: the keys are for the canonical name.
            for p in &mut sel.pods {
                p.name = canonical.to_string();
            }
            sel
        }
        Err(e) => {
            eprintln!("      warning: {canonical}'s API keys weren't re-distributed ({e:#}) — run `arena pods copy-keys {canonical}`");
            return;
        }
    };
    if let Err(e) = handle_copy_keys(remote, cfg, keys_dir, None, None, &sel, false, true).await {
        eprintln!("      warning: {canonical}'s API keys weren't re-distributed ({e:#}) — run `arena pods copy-keys {canonical}`");
    }
}

/// The new pod's host maintenance window, when its provider reports one (best-effort,
/// bounded): a host going into maintenance stops its pods — on RunPod that wipes the
/// container disk — so a replacement landing on one is no replacement (live finding #28).
async fn maintenance_window(owner: &dyn Provider, id: &str) -> Option<String> {
    let pods = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, owner.list_pods()).await.ok()?.ok()?;
    let mut mine: Vec<arena_core::Pod> = pods.into_iter().filter(|p| p.id == id).collect();
    if mine.is_empty() {
        return None;
    }
    let _ = arena_core::ratelimit::bounded(ENRICH_TIMEOUT, owner.enrich(&mut mine)).await;
    let label = arena_core::fleet::maintenance_label(mine[0].maintenance.as_ref());
    (label != "-").then_some(label)
}

/// The proxy is write-only (`SSH_PROXY_RELOAD_CMD=""`): arena writes the forwards but never
/// reloads nginx, so nothing can be verified *through* it (live finding #29: the cutover
/// waited 45 s "for nginx", then blamed the new pod). The proxy's file, for the message.
fn write_only_proxy(cfg: &Config) -> Option<String> {
    arena_core::proxy::ProxyConfig::from_config(cfg).ok().filter(|p| p.reload_cmd.trim().is_empty()).map(|p| p.nginx_path)
}

/// The config line a plan prints for the copy: what it carries and what it leaves (#27).
fn copy_scope_line() -> String {
    format!("copies {}; {}", arena_core::pull::REPLICATION_KEEPS, arena_core::pull::REPLICATION_SKIPS)
}

/// The plan / confirm note on a locked original: copying only reads it, but what retires it
/// will be refused until it's unlocked (never unlocked here).
fn locked_original_note(src: &arena_core::Pod) -> Option<String> {
    arena_core::lock::is_locked(src).then(|| {
        format!(
            "note: {0} is locked — the copy only reads it, but the cutover will be refused until \
             `arena pods unlock {0}` (arena never unlocks it by itself)",
            src.name
        )
    })
}

/// The refusal for copying from a STOPPED original (`replace`, `migrate copy`), said while
/// planning — before a new pod is created and billed only for the copy to find nothing
/// running to read (review finding). `None` when it's up.
fn stopped_source_refusal(src: &arena_core::Pod) -> Option<String> {
    arena_core::status::is_stopped(&src.status).then(|| {
        format!(
            "{0} is stopped ({1}) — there is nothing running to copy from. Start it first (`arena pods start {0}`), \
             then re-run; nothing was created.",
            src.name, src.status
        )
    })
}

/// The plan line for a create carrying `--api-json` / `CREATE_EXTRA_JSON`: its top-level
/// keys only (values may hold a token). `None` without extra fields.
fn extra_keys_line(spec: &PodSpec) -> Option<String> {
    let extra = spec.api_extra.as_ref()?;
    Some(format!(
        "+ {} [{}] merged into the create request",
        arena_core::apiextra::SOURCES,
        extra.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
    ))
}

/// Resolve a migration's canonical name + the owning provider, rejecting a `-new`/`-old`
/// staging name (the user must pass the live machine name). Returns (owner, canonical, pods).
async fn resolve_migration(
    cfg: &Config,
    target: &str,
) -> Result<(Box<dyn Provider>, String, Vec<arena_core::Pod>)> {
    let (owner, src_id, src_label) = resolve_target_any(cfg, target).await?;
    let (canonical, pods) = migration_pods(owner.as_ref(), &src_id, &src_label).await?;
    Ok((owner, canonical, pods))
}

/// The migration's canonical name (the source pod's, never a `-new`/`-old` staging name)
/// and the owner's listing — [`resolve_migration`] once the owner is known.
async fn migration_pods(owner: &dyn Provider, src_id: &str, src_label: &str) -> Result<(String, Vec<arena_core::Pod>)> {
    let pods = owner.list_pods().await.context("listing pods")?;
    let canonical = pods
        .iter()
        .find(|p| p.id == src_id)
        .ok_or_else(|| anyhow::anyhow!("{src_label} vanished while planning"))?
        .name
        .clone();
    let bare = canonical
        .strip_suffix("-new")
        .or_else(|| canonical.strip_suffix("-old"));
    if let Some(bare) = bare {
        anyhow::bail!(
            "{canonical} is a migration staging pod — pass the live machine name `{bare}` instead"
        );
    }
    Ok((canonical, pods))
}

/// Can we reach pod `expected_id` *through the proxy*? Computes the machine's stable proxy
/// port (`starting_port + its index in MACHINE_NAME_LIST`), SSHes to `proxy_host:port`, and
/// confirms the pod answering is the expected one (RUNPOD_POD_ID via /proc/1/environ). This
/// is the cutover's safety gate — a swap that leaves the proxy pointing at an unreachable or
/// wrong pod is exactly what broke a participant before. A quick probe (`PROBE_TIMEOUT`):
/// the caller retries while nginx settles, so a hung attempt must not stall that loop.
async fn proxy_reaches_pod(remote: &dyn Remote, cfg: &Config, name: &str, expected_id: &str) -> Result<bool> {
    let px = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let idx = cfg
        .machine_names
        .iter()
        .position(|c| arena_core::naming::qualify(prefix, c) == name)
        .ok_or_else(|| anyhow::anyhow!("{name} not in MACHINE_NAME_LIST — no stable proxy port"))?;
    let port = px.starting_port.checked_add(idx as u16).ok_or_else(|| {
        anyhow::anyhow!("proxy port for {name} overflows u16 (starting_port + index too high)")
    })?;
    let target = arena_core::ssh::SshTarget {
        user: cfg.get("SSH_USER").unwrap_or("root").to_string(),
        host: px.proxy_host.clone(),
        port,
        key_paths: arena_core::ssh::config_ssh_keys(cfg),
        connect_timeout_secs: 15,
        host_keys: Default::default(),
    };
    let probe = "tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p'";
    match remote.exec(&target, probe, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {
            let got = o.stdout.trim();
            // FAIL CLOSED: require the pod's own RUNPOD_POD_ID to be present AND match. An
            // empty read (probe failed, or a recycled ip:port landed on a pod that doesn't
            // expose it) must NOT pass — the entire point of this gate is to catch the proxy
            // reaching the wrong/foreign pod. RunPod always exposes RUNPOD_POD_ID in PID 1's
            // env, so empty == "couldn't confirm" == not safe to cut over.
            Ok(!got.is_empty() && got == expected_id)
        }
        _ => Ok(false),
    }
}

/// `migrate copy`: build + set up `<name>-new` (or reuse it) and sync `<name>`'s files onto
/// it. No rename, no proxy — the participant keeps using `<name>` and can test the new pod.
/// The sync is a replica's ([`copy_pod_files`]): deletions on the original propagate; what it
/// replaces or deletes on `-new` that changed there since its last sync is kept in
/// `~/arena-sync-kept/` there; and a re-sync from an original that was reset since is refused.
/// A stopped original is refused up front — before anything is created and billed.
#[allow(clippy::too_many_arguments)]
async fn handle_migrate_copy(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    place: &PlaceFlags,
    dry_run: bool,
    yes: bool,
    interrupt: &Interrupt,
    // Where the new pod's price is remembered for the balance.
    bal: &money::BalanceCtx,
) -> Result<()> {
    // Setup runs after the create below: a malformed SETUP_TIMEOUT_SECS must fail now,
    // before anything is created (and billed) — not after a ~15 min wait for the new pod,
    // stranding it. Checked even on a dry-run (so the preview surfaces it) and on a
    // re-sync that won't run setup (it's a config error either way; fix it once).
    let setup_timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None)?;
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let job = MigrateCopyJob { setup_timeouts, ov, place, dry_run, yes, interrupt, keys_dir: KEYS_DIR, balance: Some(bal) };
    migrate_copy_on(remote, cfg, owner.as_ref(), &canonical, &pods, job).await
}

/// Everything [`migrate_copy_on`] needs besides the pods and providers.
struct MigrateCopyJob<'a> {
    setup_timeouts: arena_core::setup::SetupTimeouts,
    ov: &'a SpecOverrides,
    place: &'a PlaceFlags,
    dry_run: bool,
    yes: bool,
    interrupt: &'a Interrupt,
    /// Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &'a str,
    /// Where `-new`'s price is remembered for the balance ([`create_replacement`]); `None`:
    /// not remembered.
    balance: Option<&'a money::BalanceCtx>,
}

/// [`handle_migrate_copy`] once the owner and its listing are known ([`resolve_migration`])
/// — the seam tests drive with a fake owner.
async fn migrate_copy_on(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    pods: &[arena_core::Pod],
    job: MigrateCopyJob<'_>,
) -> Result<()> {
    let MigrateCopyJob { setup_timeouts, ov, place, dry_run, yes, interrupt, keys_dir, balance } = job;
    let canonical = canonical.to_string();
    let src = pods.iter().find(|p| p.name == canonical).expect("the canonical pod is listed").clone();
    let new_name = format!("{canonical}-new");
    let existing_new = pods.iter().find(|p| p.name == new_name).cloned();
    let lock_note = locked_original_note(&src);
    let stopped = stopped_source_refusal(&src);
    if let (Some(r), false) = (&stopped, dry_run) {
        anyhow::bail!("{r}");
    }
    // Not from a pod the auto-reaper is terminating right now (RS5).
    let reaping = reap::reap_in_progress(cfg, &src, "migrate");
    if let (Some(r), false) = (&reaping, dry_run) {
        anyhow::bail!("{r}");
    }
    // Nor with a floor the copy can't read (it'd fail only after the new pod is made and set up).
    let bad_floor = disk::floor_refusal(cfg, "migrate");
    if let (Some(r), false) = (&bad_floor, dry_run) {
        anyhow::bail!("{r}");
    }

    if dry_run {
        for r in [&stopped, &reaping, &bad_floor].into_iter().flatten() {
            print_would_refuse(r);
        }
        println!("migrate copy {canonical} (dry-run):");
        if existing_new.is_some() {
            println!(
                "  - {new_name} exists → incremental re-sync {canonical} → {new_name}: deletions mirrored; what it \
                 replaces or deletes on {new_name} that changed there since its last sync is kept in ~/{}/ there; \
                 refused if {canonical} was reset since that sync",
                arena_core::replica::KEPT_DIR
            );
        } else {
            let rep = plan_replacement(cfg, owner, &src, ov, place).await?;
            println!("  - create {new_name}:");
            print_replacement_plan(owner, &rep, &new_name);
            println!("  - wait for it to stabilize, then set it up (~/.name={} until the cutover)", short_of(cfg, &new_name));
            println!("  - sync {canonical} → {new_name}: {}", copy_scope_line());
        }
        if let Some(n) = &lock_note {
            println!("  {n}");
        }
        println!("  (no rename, no proxy change)\n[dry-run] nothing changed.");
        return Ok(());
    }

    let (new_id, fresh) = if let Some(np) = existing_new {
        println!("Re-using existing {new_name} (id={}) — incremental re-sync.", np.id);
        let shaping = ov.gpu.is_some() || ov.gpus.is_some() || ov.cloud.is_some() || ov.disk.is_some() || ov.volume.is_some()
            || ov.image.is_some() || ov.bootstrap || place.max_price.is_some() || place.retry_mins > 0;
        if shaping {
            eprintln!(
                "note: the spec/placement flags only shape a NEW pod — {new_name} exists and is re-synced as it is \
                 (terminate it first to build it differently)"
            );
        }
        (np.id, false)
    } else {
        let rep = plan_replacement(cfg, owner, &src, ov, place).await?;
        println!("migrate copy {canonical} → {new_name}:");
        print_replacement_plan(owner, &rep, &new_name);
        if rep.plan.options.is_empty() {
            anyhow::bail!("nothing to create: no option left to try (see the table above, or `arena offers`)");
        }
        if let Some(n) = &lock_note {
            println!("  {n}");
        }
        if !confirm(
            yes,
            &format!(
                "Will create {new_name} and sync {canonical}'s files onto it — no rename, no proxy change. ({})",
                copy_scope_line()
            ),
        )? {
            println!("aborted.");
            return Ok(());
        }
        println!("[1/3] creating {new_name}…");
        let created = create_replacement(owner, &rep, &new_name, place, interrupt, balance).await?;
        println!("      created id={}", created.id);
        println!("[2/3] waiting for {new_name} to come up and stabilize…");
        wait_for_endpoint(owner, &created.id, 900)
            .await
            .with_context(|| format!("{new_name} never came up"))?;
        wait_for_stable_endpoint(owner, &created.id, 90, 600)
            .await
            .with_context(|| format!("{new_name} never stabilized"))?;
        println!("      provisioning {new_name} (its per-host API keys follow at the cutover, under {canonical})…");
        // By id (a fresh listing, for the settled endpoint): the pod we just made.
        let new = just_these(owner, std::slice::from_ref(&created.id)).await?;
        handle_setup(remote.clone(), cfg, true, false, None, None, false, false, setup_timeouts, &new, keys_dir, Some(owner))
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        (created.id, true)
    };

    println!("[3/3] syncing {canonical} → {new_name}…");
    copy_pod_files(cfg, owner, remote.as_ref(), &src.id, &new_id, &CopyOpts::new(fresh))
        .await
        .with_context(|| format!("syncing {canonical} -> {new_name}"))?;
    clean_marker(owner, remote.as_ref(), &new_id, cfg).await;

    println!("\n✓ {new_name} is built and synced (participant still on {canonical}, untouched).");
    if let Some(window) = maintenance_window(owner, &new_id).await {
        eprintln!(
            "  ⚠ {new_name}'s host has a scheduled maintenance window ({window}): the provider will stop it then, \
             wiping its container disk. Consider `arena pods terminate {new_name}` and copying again."
        );
    }
    if let Ok(Some(p)) = owner.list_pods().await.map(|ps| ps.into_iter().find(|p| p.id == new_id)) {
        if let (Some(ip), Some(port)) = (p.ssh_ip.as_deref(), p.ssh_port) {
            println!("  Test it directly:  ssh -p {port} root@{ip}  (use a fleet key)");
        }
    }
    println!(
        "  Re-run `arena pods migrate copy {canonical}` to re-sync any changes, then\n  \
         `arena pods migrate cutover {canonical}` to switch over (proxy is verified + auto-reverts)."
    );
    Ok(())
}

/// `migrate cutover`'s switches, passed through as one value.
#[derive(Debug, Clone, Copy, Default)]
struct CutoverOpts {
    skip_proxy: bool,
    /// Cut over to the copy already on `-new` without syncing from the original first.
    no_final_sync: bool,
    dry_run: bool,
    yes: bool,
}

/// `migrate cutover`: final delta sync, swap names, re-point + VERIFY the proxy, auto-revert
/// if the new pod isn't reachable through the proxy. Keeps `<name>-old`.
async fn handle_migrate_cutover(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    // The fleet (every configured provider), for the proxy syncs.
    fleet: &dyn Provider,
    cfg: &Config,
    target: &str,
    opts: CutoverOpts,
) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    migrate_cutover_on(remote, fleet, cfg, owner.as_ref(), &canonical, &pods, opts, KEYS_DIR).await
}

/// When `<name>-new` was last synced, as the plan says it: `2026-10-08 11:02 UTC (3h ago)`,
/// or that it can't tell. Read from the pod (bounded).
async fn last_sync_label(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, id: &str) -> String {
    let stamp = match wait_for_endpoint(owner, id, 30).await.ok().and_then(|p| SshTarget::from_pod(&p, cfg).ok()) {
        Some(t) => match remote.exec(&t, &arena_core::replica::last_sync_command(), Some(PROBE_TIMEOUT)).await {
            Ok(o) if o.success => arena_core::replica::parse_last_sync(&o.stdout),
            _ => None,
        },
        None => None,
    };
    let Some(at) = stamp else { return "an unknown time (no sync stamp on it)".to_string() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    sync_time_label(at, now)
}

/// [`last_sync_label`]'s words for a sync at `at`, seen at `now` (pure, tested): the minute
/// it happened and exactly how long ago, as `up` says it (`3m55s`) — live R printed "(180s
/// ago)" for 235 s: the age was floored to whole minutes, then printed in seconds.
fn sync_time_label(at: u64, now: u64) -> String {
    let day = arena_core::schedule::ymd_string((at / 86_400) as i64);
    let ago = Duration::from_secs(now.saturating_sub(at));
    format!("{day} {:02}:{:02} UTC ({} ago)", at % 86_400 / 3600, at % 3600 / 60, arena_core::pipeline::fmt_elapsed(ago))
}

/// [`handle_migrate_cutover`] once the owner and its listing are known ([`resolve_migration`])
/// — the seam tests drive with a fake owner (and fleet).
#[allow(clippy::too_many_arguments)]
async fn migrate_cutover_on(
    remote: Arc<dyn Remote>,
    fleet: &dyn Provider,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    pods: &[arena_core::Pod],
    opts: CutoverOpts,
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let CutoverOpts { skip_proxy, no_final_sync, dry_run, yes } = opts;
    let new_name = format!("{canonical}-new");
    let old_name = format!("{canonical}-old");
    let src = pods.iter().find(|p| p.name == canonical).expect("the canonical pod is listed").clone();
    let src_id = src.id.clone();
    let new_id = pods
        .iter()
        .find(|p| p.name == new_name)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no {new_name} — run `arena pods migrate copy {canonical}` first"))?;
    if pods.iter().any(|p| p.name == old_name) {
        anyhow::bail!("{old_name} already exists — finish/revert the previous migration first");
    }
    // The cutover moves the participant off the pod (renamed to -old, then `finish`
    // terminates it): a locked original is refused here, at the step that starts that.
    let locked = arena_core::lock::refusal("cut over", &[&src], LOCKED_REPLACEMENT_NOTE);
    if let (Some(r), false) = (&locked, dry_run) {
        anyhow::bail!("{r}");
    }
    // Nor while the auto-reaper is terminating it (RS5).
    let reaping = reap::reap_in_progress(cfg, &src, "cut over");
    if let (Some(r), false) = (&reaping, dry_run) {
        anyhow::bail!("{r}");
    }
    // Nor, with a final sync, with a floor its copy can't read.
    let bad_floor = if no_final_sync { None } else { disk::floor_refusal(cfg, "cut over") };
    if let (Some(r), false) = (&bad_floor, dry_run) {
        anyhow::bail!("{r}");
    }
    let write_only = if skip_proxy { None } else { write_only_proxy(cfg) };
    let sync_line = if no_final_sync {
        format!(
            "final sync SKIPPED (--no-final-sync): {new_name} keeps the copy from its last sync, {} — \
             anything changed on {canonical} since stays behind on it",
            last_sync_label(remote.as_ref(), cfg, owner, &new_id).await
        )
    } else {
        format!("final delta sync {canonical} → {new_name} (refused if {canonical} was reset since {new_name}'s last sync)")
    };
    let verify_line = match &write_only {
        Some(path) => format!(
            "proxy apply (write-only: {path} is written, nginx is NOT reloaded by arena — so nothing can be \
             verified through it; the new pod's own health check stands, and nothing auto-reverts)"
        ),
        None => "proxy apply, then SSH through the proxy to confirm it works (auto-revert if not)".to_string(),
    };

    if dry_run {
        for r in [&locked, &reaping, &bad_floor].into_iter().flatten() {
            print_would_refuse(r);
        }
        println!("migrate cutover {canonical} (dry-run):");
        println!("  1. {sync_line}");
        println!("  2. rename {canonical} → {old_name}, then {new_name} → {canonical} (+ ~/.name on both, API keys under {canonical})");
        if skip_proxy {
            println!("  3. proxy SKIPPED — run `arena proxy apply` yourself");
        } else {
            println!("  3. {verify_line}");
        }
        println!("  {old_name} kept (delete later with `migrate finish {canonical}`).\n[dry-run] nothing changed.");
        return Ok(());
    }

    if !confirm(
        yes,
        &format!(
            "Cut over {canonical} to {new_name}: {sync_line}; then swap names and {}. {old_name} kept.",
            if skip_proxy { "leave the proxy alone".to_string() } else { verify_line.clone() }
        ),
    )? {
        println!("aborted.");
        return Ok(());
    }

    // 1. final delta sync + verify it landed.
    if no_final_sync {
        println!("[1/4] {sync_line}.");
    } else {
        println!("[1/4] final delta sync {canonical} → {new_name}…");
        copy_pod_files(cfg, owner, remote.as_ref(), &src_id, &new_id, &CopyOpts::new(false)).await.with_context(|| {
            format!(
                "final sync {canonical} -> {new_name} — nothing was swapped. If {canonical} is down for good, cut over \
                 to the copy already on {new_name}: `arena pods migrate cutover {canonical} --no-final-sync`"
            )
        })?;
        clean_marker(owner, remote.as_ref(), &new_id, cfg).await;
    }
    println!("[2/4] verifying {new_name} health…");
    verify_replacement(owner, remote.as_ref(), &new_id, cfg).await?;

    // 3. swap names (old-first so the canonical name is never on two pods).
    println!("[3/4] swapping names…");
    let policy = arena_core::retry::RetryPolicy::default();
    arena_core::retry::retrying(&policy, || owner.rename_pod(&src_id, &old_name))
        .await
        .with_context(|| format!("renaming {canonical} -> {old_name}"))?;
    if let Err(e) =
        arena_core::retry::retrying(&policy, || owner.rename_pod(&new_id, canonical)).await
    {
        eprintln!("      promote failed ({e}); rolling back {old_name} -> {canonical}");
        let _ = owner.rename_pod(&src_id, canonical).await;
        return Err(e).context("promoting new pod (rolled back; original intact)");
    }
    after_swap(remote.clone(), cfg, owner, canonical, &new_id, &new_name, &old_name, &src_id, keys_dir).await;
    if let Some(n) = reap::policy_not_carried_note(cfg, owner.name(), &src_id, &new_id) {
        println!("      {n}");
    }

    // 4. proxy apply + verify through the proxy; auto-revert on failure.
    if skip_proxy {
        println!("[4/4] proxy skipped — run `arena proxy apply` to route {canonical} to the new pod.");
    } else {
        println!("[4/4] re-pointing proxy and verifying {canonical} through it…");
        // Unlike the other lifecycle commands, the cutover *depends* on the proxy: a sync
        // that was skipped or failed — or one that didn't route the name to the new pod —
        // means nobody can reach the new pod, so revert now.
        let sync = sync_proxy(cfg, fleet, "migrate cutover").await;
        let not_routed = replace_cleanup_blocker(&sync, proxied_name(cfg, canonical), canonical, owner.name(), &new_id);
        if !matches!(sync, ProxySync::Synced { .. }) || not_routed.is_some() {
            let why = not_routed.unwrap_or_else(|| "the proxy wasn't re-pointed".to_string());
            eprintln!("      {why} — auto-reverting.");
            cutover_revert(remote.as_ref(), cfg, fleet, owner, canonical, &new_id, &src_id, true).await;
            anyhow::bail!("cutover reverted: {why}. {canonical} is back on the original.");
        }
        if let Some(path) = &write_only {
            println!(
                "      [proxy] write-only: {canonical}'s forward to the new pod is written to {path}, but arena doesn't \
                 reload nginx — reload it yourself, then check {canonical}'s port reaches the new pod (roll back with \
                 `arena pods migrate revert {canonical}`)."
            );
        } else {
            // `nginx -s reload` is GRACEFUL: for a few seconds after the reload, existing worker
            // processes keep serving the OLD config, so a fresh connection can still be routed to
            // the OLD pod. A single immediate probe therefore reads the old pod's id and false-
            // fails. Wait for the reload to settle and retry the through-proxy identity check —
            // succeed as soon as the proxy actually reaches the new pod (up to ~45s).
            let settle_tries = 15;
            let mut reached = false;
            for attempt in 1..=settle_tries {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if proxy_reaches_pod(remote.as_ref(), cfg, canonical, &new_id).await.unwrap_or(false) {
                    reached = true;
                    println!("      ✓ {canonical} reachable through the proxy on the new pod (after ~{}s).", attempt * 3);
                    break;
                }
                if attempt % 3 == 0 {
                    eprintln!("      waiting for nginx to finish cutting over to the new pod ({}s)…", attempt * 3);
                }
            }
            if !reached {
                eprintln!("      ✗ {canonical} NOT reachable through the proxy after ~{}s — auto-reverting.", settle_tries * 3);
                cutover_revert(remote.as_ref(), cfg, fleet, owner, canonical, &new_id, &src_id, true).await;
                anyhow::bail!(
                    "cutover reverted: the new pod wasn't reachable through the proxy (it passed its own SSH \
                     health check, so look at the proxy host first). {canonical} is back on the original \
                     (untouched); retry once the proxy answers."
                );
            }
        }
    }

    println!(
        "\n✓ Cut over: {canonical} is now the new pod. Original parked as {old_name} (kept).\n  \
         If something's wrong, roll back:  arena pods migrate revert {canonical}\n  \
         When you're happy, remove the old pod:  arena pods migrate finish {canonical}"
    );
    Ok(())
}

/// Roll a cutover back: rename `<name>` → `<name>-new`, `<name>-old` → `<name>`, put each
/// pod's `~/.name` back, and (unless `apply_px` is false) re-point the proxy to the restored
/// original. Shared by the cutover's auto-revert and the manual `migrate revert`.
#[allow(clippy::too_many_arguments)]
async fn cutover_revert(
    remote: &dyn Remote,
    cfg: &Config,
    fleet: &dyn Provider,
    owner: &dyn Provider,
    canonical: &str,
    new_id: &str,
    old_id: &str,
    apply_px: bool,
) {
    let new_name = format!("{canonical}-new");
    // Demote the (failed) new pod, then restore the original to the canonical name. Order
    // matters so the canonical name is never held by two pods.
    let _ = owner.rename_pod(new_id, &new_name).await;
    if let Err(e) = owner.rename_pod(old_id, canonical).await {
        eprintln!("  REVERT WARNING: couldn't restore {canonical} ({e}) — fix names manually!");
    }
    // Best-effort, short: the names are what matter; `pods setup` rewrites either file. A
    // stopped pod isn't written (`write_name_file`): its listed endpoint is stale.
    let _ = write_name_file(remote, cfg, owner, new_id, &new_name, 30).await;
    if !write_name_file(remote, cfg, owner, old_id, canonical, 30).await {
        eprintln!(
            "  note: {canonical}'s ~/.name wasn't rewritten (it isn't running?) — `arena pods setup {canonical}` writes it \
             once it is"
        );
    }
    if apply_px && !matches!(sync_proxy(cfg, fleet, "migrate revert").await, ProxySync::Synced { .. }) {
        eprintln!("  REVERT WARNING: the proxy wasn't re-pointed — run `arena proxy apply`.");
    }
}

/// `migrate revert`: manual rollback of a cutover (swap `<name>` ↔ `<name>-old` + proxy).
/// Work done on the new pod since the cutover stays on it (now `<name>-new`) — the confirm
/// and the summary say so (live finding #7); a later `migrate copy` keeps (in
/// `~/arena-sync-kept/` there), never destroys, whatever of it the copy replaces.
#[allow(clippy::too_many_arguments)]
async fn handle_migrate_revert(
    remote: &dyn Remote,
    fleet: &dyn Provider,
    cfg: &Config,
    target: &str,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let old_name = format!("{canonical}-old");
    let new_name = format!("{canonical}-new");
    let new_id = pods.iter().find(|p| p.name == canonical).expect("the canonical pod is listed").id.clone();
    let old_id = pods
        .iter()
        .find(|p| p.name == old_name)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no {old_name} to revert to (nothing to roll back)"))?;
    let stays = format!(
        "work done on the new pod since the cutover is NOT on the original: it stays on {new_name} (a later \
         `migrate copy` keeps — never destroys — whatever of it the copy replaces, in ~/{}/ there; it isn't merged back)",
        arena_core::replica::KEPT_DIR
    );

    if dry_run {
        println!("migrate revert {canonical} (dry-run):");
        println!("  rename {canonical} → {new_name}, {old_name} → {canonical}");
        println!("  {}", if skip_proxy { "(no proxy)" } else { "then proxy apply (route back to the original)" });
        println!("  note: {stays}");
        println!("[dry-run] nothing changed.");
        return Ok(());
    }
    if !confirm(
        yes,
        &format!("Roll {canonical} back to the original ({old_name}){}. Note: {stays}.", if skip_proxy { "" } else { " + proxy" }),
    )? {
        println!("aborted.");
        return Ok(());
    }
    cutover_revert(remote, cfg, fleet, owner.as_ref(), &canonical, &new_id, &old_id, !skip_proxy).await;
    println!("✓ Reverted: {canonical} is the original again; the new pod is parked as {new_name}.");
    println!("{}", revert_followup(&canonical, &new_name));
    Ok(())
}

/// What `migrate revert` says about the new pod's work (pure, tested): it stays there, and
/// how to get it — `pods pull <new>` backs up THAT pod only (`pull`'s positionals are pods,
/// like every command's; it once read one as the backup's label and pulled every pod's home
/// into a folder named after it, onto the disk the live cohort shares).
fn revert_followup(canonical: &str, new_name: &str) -> String {
    format!(
        "  Its work since the cutover stays there — copy what's wanted back to {canonical} by hand (back it up \
         first with `arena pods pull {new_name}`)."
    )
}

/// `migrate finish`'s switches, passed through as one value.
#[derive(Debug, Clone, Copy, Default)]
struct FinishOpts {
    /// Terminate a STOPPED `-old` anyway.
    discard_stopped: bool,
    dry_run: bool,
    yes: bool,
}

/// `migrate finish`: terminate the parked `<name>-old` pod, once the proxy routes the
/// participant's port to the live pod ([`migrate_finish_on`]).
async fn handle_migrate_finish(remote: &dyn Remote, fleet: &dyn Provider, cfg: &Config, target: &str, opts: FinishOpts) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    migrate_finish_on(remote, fleet, cfg, owner.as_ref(), &canonical, &pods, opts).await
}

/// Why `migrate finish` keeps a STOPPED `-old` (pure, tested): nothing was synced from it
/// since the live pod's last sync (`last_sync`, as [`last_sync_label`] says it) — after a
/// `cutover --no-final-sync` the participant's changes since are still on it, on its volume
/// (`volume_gb`: `Some(0)` none, `None` unknown) — and the terminate would delete them.
fn stopped_old_refusal(old_name: &str, status: &str, canonical: &str, volume_gb: Option<u32>, last_sync: &str) -> String {
    let holds = match volume_gb {
        Some(v) if v > 0 => format!("its {v} GB volume still holds them"),
        Some(_) => "it has no volume, so on RunPod (whose stop discards the container disk) it likely holds nothing more — \
                    then --discard-stopped is safe"
            .to_string(),
        None => "couldn't read whether it has a volume that holds them".to_string(),
    };
    format!(
        "NOT terminating {old_name}: it is stopped ({status}), so nothing was synced from it after {canonical}'s last \
         sync ({last_sync}) — whatever the participant changed on it since (e.g. before a `cutover --no-final-sync`) \
         was never copied, and {holds}. Start it (`arena pods start {old_name}`) and back it up (`arena pods pull \
         {old_name}`) first, or re-run with --discard-stopped to terminate it anyway."
    )
}

/// [`handle_migrate_finish`] once the owner and its listing are known — the seam tests drive
/// with a fake owner. Live finding #6: finish terminated `-old` while the proxy still routed
/// the participant's port to it (a `--skip-proxy` cutover) — the port then pointed at a dead
/// pod. Now the proxy is synced first, and the terminate refused unless that sync ROUTED
/// `<name>`'s stable port to the live pod ([`replace_cleanup_blocker`], the same rule as
/// `pods replace`). Without a proxy (or a name with no stable port) nothing can route to
/// `-old`, and it goes as before. A STOPPED `-old` is kept unless `discard_stopped`
/// ([`stopped_old_refusal`]): it may hold what the last sync never copied.
#[allow(clippy::too_many_arguments)]
async fn migrate_finish_on(
    remote: &dyn Remote,
    fleet: &dyn Provider,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    pods: &[arena_core::Pod],
    opts: FinishOpts,
) -> Result<()> {
    let FinishOpts { discard_stopped, dry_run, yes } = opts;
    let old_name = format!("{canonical}-old");
    let old = pods
        .iter()
        .find(|p| p.name == old_name)
        .ok_or_else(|| anyhow::anyhow!("no {old_name} to remove (nothing to finish)"))?;
    let live = pods.iter().find(|p| p.name == canonical);
    let proxied = proxied_name(cfg, canonical);
    let locked = arena_core::lock::refusal("terminate", &[old], "");
    let stopped = if arena_core::status::is_stopped(&old.status) && !discard_stopped {
        let spec = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, owner.pod_spec(&old.id)).await;
        let volume = match spec {
            Ok(Ok(s)) => Some(s.volume_gb),
            _ => None,
        };
        let last = match live {
            Some(l) => last_sync_label(remote, cfg, owner, &l.id).await,
            None => "an unknown time".to_string(),
        };
        Some(stopped_old_refusal(&old_name, &old.status, canonical, volume, &last))
    } else {
        None
    };
    if dry_run {
        let first = if proxied { format!("sync the proxy, confirm {canonical}'s port routes to the live pod, then ") } else { String::new() };
        println!("migrate finish {canonical} (dry-run): would {first}terminate {old_name} (id={}).", old.id);
        for r in [&locked, &stopped].into_iter().flatten() {
            print_would_refuse(r);
        }
        return Ok(());
    }
    if let Some(r) = locked {
        anyhow::bail!("{r}");
    }
    if let Some(r) = stopped {
        anyhow::bail!("{r}");
    }
    if !confirm(yes, &format!("Permanently terminate {old_name} (id={})? This deletes the original pod.", old.id))? {
        println!("aborted.");
        return Ok(());
    }
    if proxied {
        let sync = sync_proxy(cfg, fleet, "migrate finish").await;
        let blocker = match live {
            Some(live) => replace_cleanup_blocker(&sync, true, canonical, owner.name(), &live.id),
            None => Some(format!("no pod is named {canonical} now")),
        };
        if let Some(why) = blocker {
            anyhow::bail!(
                "NOT terminating {old_name}: {why} — {canonical}'s stable port may still lead to {old_name}. Make the \
                 proxy route it to the live pod (`arena proxy apply`), then re-run `arena pods migrate finish {canonical}`."
            );
        }
    }
    owner
        .terminate_pod(&old.id)
        .await
        .map_err(|e| anyhow::anyhow!("terminating {old_name}: {}", arena_core::lock::explain(&old_name, &e)))?;
    println!("✓ Terminated {old_name}. Migration of {canonical} complete.");
    // Without a proxy no sync ran above, and `SSH_CONFIG_FILE` (each pod's own endpoint) would
    // keep `-old`'s block: rewritten from the listing after the terminate.
    if !proxied {
        for line in ssh_config_after(cfg, fleet, "migrate finish").await {
            println!("{line}");
        }
    }
    remove_staging_for(&std::env::temp_dir(), &old.id);
    Ok(())
}

/// `migrate status`: show the migration pair's state and the next step.
async fn handle_migrate_status(cfg: &Config, target: &str) -> Result<()> {
    let (_owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let show = |label: &str, name: &str| {
        match pods.iter().find(|p| p.name == name) {
            Some(p) => println!(
                "  {label:5} {name:24} id={} {} {}:{}",
                p.id,
                p.status,
                p.ssh_ip.as_deref().unwrap_or("-"),
                p.ssh_port.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
            ),
            None => println!("  {label:5} {name:24} (none)"),
        }
    };
    println!("Migration status for {canonical}:");
    show("live", &canonical);
    show("new", &format!("{canonical}-new"));
    show("old", &format!("{canonical}-old"));
    let has_new = pods.iter().any(|p| p.name == format!("{canonical}-new"));
    let has_old = pods.iter().any(|p| p.name == format!("{canonical}-old"));
    println!(
        "\nNext: {}",
        if has_old {
            format!("`migrate finish {canonical}` (delete old) or `migrate revert {canonical}` (roll back)")
        } else if has_new {
            format!("`migrate copy {canonical}` (re-sync) or `migrate cutover {canonical}` (switch over)")
        } else {
            format!("`migrate copy {canonical}` to begin")
        }
    );
    Ok(())
}

/// Blue-green pod replacement: build a fresh pod from the source's spec, copy its files
/// over, then rename-swap it into the canonical machine name (parking the old one as
/// `<name>-old`). Identity-preserving — the proxy port / ssh-config (which key off the
/// canonical name) carry straight over. Same spec as the source unless `ov` overrides it.
///
/// Ordering is chosen so nothing is destroyed until the replacement is built **and**
/// verified, and there's a manual gate right before the swap. The swap renames old-first so
/// two pods never share the canonical name; if the promote fails it rolls back.
#[allow(clippy::too_many_arguments)]
async fn handle_replace(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    // The fleet (every configured provider), for the proxy sync.
    fleet: &dyn Provider,
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    place: &PlaceFlags,
    opts: ReplaceOpts,
    interrupt: &Interrupt,
    // Where the new pod's price is remembered for the balance.
    bal: &money::BalanceCtx,
) -> Result<()> {
    // Setup runs after the create (step 3/7): a malformed SETUP_TIMEOUT_SECS must fail
    // now, before anything is created (and billed) — not after the new pod has come up,
    // which would strand a `-new` pod that also blocks a re-run. Dry-runs check it too.
    let setup_timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None)?;
    // Resolve the source pod and the backend that owns it. `resolve_target_any` accepts a
    // name or a raw id, so read the canonical name back from the fleet rather than trusting
    // the argument (a user may pass an id).
    let (owner, src_id, src_label) = resolve_target_any(cfg, target).await?;
    let job = ReplaceJob { setup_timeouts, ov, place, opts, interrupt, keys_dir: KEYS_DIR, balance: Some(bal) };
    replace_on(remote, fleet, cfg, owner.as_ref(), src_id, src_label, job).await
}

/// `pods replace`'s switches, passed through to [`replace_on`] as one value.
#[derive(Debug, Clone, Copy, Default)]
struct ReplaceOpts {
    keep_old: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
}

/// Everything [`replace_on`] needs besides the pods and providers.
struct ReplaceJob<'a> {
    setup_timeouts: arena_core::setup::SetupTimeouts,
    ov: &'a SpecOverrides,
    place: &'a PlaceFlags,
    opts: ReplaceOpts,
    interrupt: &'a Interrupt,
    /// Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &'a str,
    /// Where the new pod's price is remembered for the balance ([`create_replacement`]);
    /// `None`: not remembered.
    balance: Option<&'a money::BalanceCtx>,
}

/// [`handle_replace`] once the source pod (`src_id`) and the backend that owns it are
/// known — the seam tests drive with a fake owner.
async fn replace_on(
    remote: Arc<dyn Remote>,
    fleet: &dyn Provider,
    cfg: &Config,
    owner: &dyn Provider,
    src_id: String,
    src_label: String,
    job: ReplaceJob<'_>,
) -> Result<()> {
    let ReplaceOpts { keep_old, skip_proxy, dry_run, yes } = job.opts;
    let pods = owner.list_pods().await.context("listing pods for replace")?;
    let src = pods
        .iter()
        .find(|p| p.id == src_id)
        .ok_or_else(|| anyhow::anyhow!("source pod {src_label} vanished while planning"))?
        .clone();
    let canonical = src.name.clone();
    if canonical.ends_with("-new") || canonical.ends_with("-old") {
        anyhow::bail!(
            "{canonical} looks like a replace staging pod — pass the canonical machine name, \
             not a -new/-old leftover"
        );
    }
    let (new_name, old_name) = replace_stage_names(&canonical);
    // Replace renames the pod and (unless --keep-old) terminates it: a locked one is refused
    // before anything is read or created — never unlocked here.
    let locked = arena_core::lock::refusal("replace", &[&src], LOCKED_REPLACEMENT_NOTE);
    if let (Some(r), false) = (&locked, dry_run) {
        anyhow::bail!("{r}");
    }
    // The copy reads the original: a stopped one is refused now, not after a create.
    let stopped = stopped_source_refusal(&src);
    if let (Some(r), false) = (&stopped, dry_run) {
        anyhow::bail!("{r}");
    }
    // Nor one the auto-reaper is terminating right now (RS5).
    let reaping = reap::reap_in_progress(cfg, &src, "replace");
    if let (Some(r), false) = (&reaping, dry_run) {
        anyhow::bail!("{r}");
    }
    // Nor with a floor the copy can't read (it'd fail only after the new pod is made and set up,
    // leaving it billing).
    let bad_floor = disk::floor_refusal(cfg, "replace");
    if let (Some(r), false) = (&bad_floor, dry_run) {
        anyhow::bail!("{r}");
    }

    let rep = plan_replacement(cfg, owner, &src, job.ov, job.place).await?;

    // Pre-flight: a leftover -new/-old from a prior aborted run is a resume signal, not a
    // fresh start — flag it so the operator cleans up rather than colliding.
    for staging in [&new_name, &old_name] {
        if let Some(p) = pods.iter().find(|p| &p.name == staging) {
            eprintln!(
                "warning: {staging} already exists (id={}) — likely a leftover from an \
                 interrupted replace. Resolve it before proceeding.",
                p.id
            );
        }
    }

    // Print the plan.
    println!("Replace {canonical}  (id={src_id}, {})", owner.name());
    print_replacement_plan(owner, &rep, &new_name);
    println!("  pipeline:");
    println!("    1. create   {new_name}");
    println!("    2. setup    {new_name}   (deploy key, ssh config, repo, ~/.name={} until the swap)", short_of(cfg, &new_name));
    println!("    3. copy     {canonical} -> {new_name}   ({})", copy_scope_line());
    println!("    4. verify   {new_name}   (the copy read back on the right pod, ssh + GPU health check, maintenance window)");
    println!(
        "    5. swap     rename {canonical} -> {old_name}, then {new_name} -> {canonical} (+ ~/.name, API keys under {canonical})"
    );
    if skip_proxy {
        println!("    6. proxy    SKIPPED (--skip-proxy) — run `arena proxy apply` yourself");
    } else {
        println!("    6. proxy    apply (repoint nginx; stable port reclaimed by the canonical name)");
    }
    if keep_old {
        println!("    7. cleanup  keep {old_name} (--keep-old) — terminate it yourself once happy");
    } else {
        println!("    7. cleanup  terminate {old_name} once the proxy routes {canonical} to the new pod");
    }

    if dry_run {
        for r in [&locked, &stopped, &reaping, &bad_floor].into_iter().flatten() {
            println!();
            print_would_refuse(r);
        }
        println!("\n[dry-run] nothing changed.");
        return Ok(());
    }

    // Don't clobber a leftover from an interrupted run.
    if pods.iter().any(|p| p.name == new_name || p.name == old_name) {
        anyhow::bail!(
            "{new_name}/{old_name} already exist — clean up the previous (interrupted) replace \
             before running again (`arena pods terminate <name>`)"
        );
    }
    if rep.plan.options.is_empty() {
        anyhow::bail!("nothing to create: no option left to try (see the table above, or `arena offers`)");
    }
    if !confirm(
        yes,
        &format!(
            "Will build {new_name}, copy {canonical}'s files onto it, verify, then swap it in as \
             {canonical} (old parked as {old_name}{}).",
            if keep_old { ", kept" } else { ", then terminated" }
        ),
    )? {
        println!("aborted.");
        return Ok(());
    }

    let policy = arena_core::retry::RetryPolicy::default();

    // [1/7] create the replacement under the staging name — on the first option with
    // capacity (several with a --gpu/--cloud list), retried while --retry-mins allows.
    println!("[1/7] creating {new_name}…");
    let created = create_replacement(owner, &rep, &new_name, job.place, job.interrupt, job.balance).await?;
    println!("      created {new_name} id={}", created.id);

    // [2/7] wait for an SSH endpoint, then for it to STABILIZE. A freshly-created pod's
    // ip:port churns while RunPod places it, and a still-moving pod can migrate and reset its
    // container disk — wiping any setup+copy. Provisioning a *settled* pod avoids most of that.
    println!("[2/7] waiting for {new_name} to come up and stabilize…");
    // RunPod can be slow to assign a fresh pod's SSH endpoint — 5+ min is not unusual,
    // especially on the secure tier — so give it a generous window before giving up.
    wait_for_endpoint(owner, &created.id, 900)
        .await
        .with_context(|| format!("{new_name} never came up — left in place for inspection"))?;
    wait_for_stable_endpoint(owner, &created.id, 90, 600)
        .await
        .with_context(|| format!("{new_name}'s endpoint never stabilized"))?;

    // [3-5/7] provision + copy, then confirm the copy SURVIVES a settle window. Even a settled
    // pod can still migrate and reset to image state (losing setup AND the copy), so re-do
    // setup+copy until the delivery marker is still present after a wait — or give up (no swap,
    // original untouched).
    let copy_attempts = 3;
    let mut persisted = false;
    for attempt in 1..=copy_attempts {
        println!("[3/7] provisioning {new_name}… (attempt {attempt}/{copy_attempts}; its per-host API keys follow at the swap)");
        // By id (a fresh listing each attempt, for the current endpoint): the pod we made.
        let new = just_these(owner, std::slice::from_ref(&created.id)).await?;
        handle_setup(remote.clone(), cfg, true, false, None, None, false, false, job.setup_timeouts, &new, job.keys_dir, Some(owner))
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        println!("[4/7] copying {canonical} → {new_name} ({})…", copy_scope_line());
        // A pod made for this replace: its copy baseline starts now.
        copy_pod_files(cfg, owner, remote.as_ref(), &src_id, &created.id, &CopyOpts::new(true))
            .await
            .with_context(|| replace_copy_failed(&canonical, &new_name))?;
        println!("[5/7] confirming the copy persists (~90s — pods can reset while still settling)…");
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        if marker_present(owner, remote.as_ref(), &created.id, cfg).await {
            persisted = true;
            break;
        }
        eprintln!("      {new_name} reset to image state (lost setup+copy) — re-provisioning…");
        let _ = wait_for_stable_endpoint(owner, &created.id, 90, 600).await;
    }
    if !persisted {
        anyhow::bail!(
            "{new_name} keeps resetting its disk (it's migrating between hosts), so the copy \
             won't stick. NOT swapping — {canonical} is untouched. Try again when capacity is \
             less contended, or terminate {new_name} with `arena pods terminate {new_name}`."
        );
    }
    clean_marker(owner, remote.as_ref(), &created.id, cfg).await;

    // Health check before the swap (re-resolves the endpoint fresh and retries).
    println!("      verifying health of {new_name}…");
    verify_replacement(owner, remote.as_ref(), &created.id, cfg)
        .await
        .with_context(|| format!("verifying {new_name}"))?;
    // A host going into maintenance will stop the new pod (wiping it): say so before the
    // swap, and keep the original rather than terminate it.
    let maintenance = maintenance_window(owner, &created.id).await;
    if let Some(w) = &maintenance {
        eprintln!(
            "      ⚠ {new_name}'s host has a scheduled maintenance window ({w}): the provider will stop it \
             then, wiping its container disk. {old_name} will be KEPT after the swap."
        );
    }

    // Manual gate: nothing destructive has happened yet. A "no" leaves the built+copied
    // replacement in place under its staging name for inspection.
    let warn = maintenance.as_ref().map(|w| format!(" ⚠ Its host has a maintenance window ({w}).")).unwrap_or_default();
    if !confirm(
        yes,
        &format!(
            "{new_name} is built, copied and verified.{warn} Swap it in as {canonical} now? \
             (renames {canonical} -> {old_name}, then {new_name} -> {canonical})"
        ),
    )? {
        eprintln!(
            "aborted before swap. {new_name} is left in place (no swap). Inspect it, then either \
             re-run or remove it with `arena pods terminate {new_name}`."
        );
        return Ok(());
    }

    // [6/7] swap. Old-first so the canonical name is never held by two pods at once. If the
    // promote fails after parking the source, roll the source's name back so the canonical
    // name still resolves to the (untouched) original.
    println!("[6/7] swapping…");
    arena_core::retry::retrying(&policy, || owner.rename_pod(&src_id, &old_name))
        .await
        .with_context(|| format!("renaming {canonical} -> {old_name}"))?;
    println!("      {canonical} -> {old_name}");
    if let Err(e) =
        arena_core::retry::retrying(&policy, || owner.rename_pod(&created.id, &canonical)).await
    {
        eprintln!("      promote failed ({e}); rolling back {old_name} -> {canonical}");
        let _ = owner.rename_pod(&src_id, &canonical).await;
        return Err(e).with_context(|| {
            format!("promoting {new_name} -> {canonical} (rolled back; original {canonical} intact)")
        });
    }
    println!("      {new_name} -> {canonical}");
    after_swap(remote.clone(), cfg, owner, &canonical, &created.id, &new_name, &old_name, &src_id, job.keys_dir).await;
    if let Some(n) = reap::policy_not_carried_note(cfg, owner.name(), &src_id, &created.id) {
        println!("      {n}");
    }

    // [7/7] proxy + cleanup.
    let mut blocker = if skip_proxy {
        println!("[7/7] proxy skipped (--skip-proxy) — run `arena proxy apply` to repoint nginx.");
        None
    } else {
        println!("[7/7] re-pointing proxy…");
        // Fleet-wide (not just `owner`'s pods): a single-provider list here used to drop
        // every other provider's forwards, and a failed list (`unwrap_or_default`) all of them.
        let sync = sync_proxy(cfg, fleet, "replace").await;
        replace_cleanup_blocker(&sync, proxied_name(cfg, &canonical), &canonical, owner.name(), &created.id)
    };
    if let (None, Some(w)) = (&blocker, &maintenance) {
        blocker = Some(format!("the new pod's host has a maintenance window ({w}) — keep the original until it's past"));
    }
    if keep_old {
        println!(
            "done — {canonical} is the fresh pod. {old_name} kept (--keep-old); terminate it with \
             `arena pods terminate {old_name}` once you're happy."
        );
    } else if let Some(why) = blocker {
        eprintln!(
            "{canonical} is the fresh pod, but {old_name} is NOT terminated: {why}. Once {canonical} is safely \
             on the new pod (`arena proxy apply` routes it there), remove it with `arena pods terminate {old_name}`."
        );
    } else {
        println!("terminating parked {old_name}…");
        if let Err(e) = arena_core::retry::retrying(&policy, || owner.terminate_pod(&src_id)).await {
            eprintln!("      couldn't terminate {old_name}: {} — remove it manually.", arena_core::lock::explain(&old_name, &e));
        } else {
            println!("done — {canonical} replaced; {old_name} terminated.");
            remove_staging_for(&std::env::temp_dir(), &src_id);
        }
    }
    Ok(())
}

/// Why `pods replace` must keep the parked original instead of terminating it (pure, so
/// every outcome is table-tested): the canonical name's stable port may still lead to it.
/// Only a sync that *routed* the canonical forward to the replacement (its owner listed it
/// with an endpoint — R1) clears that; a sync that failed, was skipped although a proxy is
/// configured, or merely *kept* the forward (the owner's listing failed, or the new pod was
/// listed without an endpoint) leaves it on the old target. `proxied` = a proxy is
/// configured *and* `canonical` has a stable port on it (is in MACHINE_NAME_LIST); without
/// that no forward can lead to the old pod, so nothing blocks.
fn replace_cleanup_blocker(
    sync: &ProxySync,
    proxied: bool,
    canonical: &str,
    provider: &str,
    new_id: &str,
) -> Option<String> {
    if !proxied {
        return None;
    }
    match sync {
        ProxySync::Skipped(why) => Some(format!("the proxy sync was skipped ({why})")),
        ProxySync::Failed(e) => Some(format!("the proxy wasn't synced ({})", e.split_whitespace().collect::<Vec<_>>().join(" "))),
        ProxySync::Synced { routed, failed_providers, .. } => {
            let on_new = routed.iter().any(|f| {
                f.name == canonical && f.provider.as_deref() == Some(provider) && f.pod_id.as_deref() == Some(new_id)
            });
            if on_new {
                None
            } else if failed_providers.iter().any(|p| p == provider) {
                Some(format!("{provider} failed to list, so the proxy still routes {canonical} to its previous target"))
            } else {
                Some(format!("the proxy doesn't route {canonical} to the new pod yet (no SSH endpoint listed for it?)"))
            }
        }
    }
}

/// Poll `provider` until the pod `id` has an SSH endpoint (ip + port), or `timeout_secs`
/// elapses. Returns the ready pod. A pod up behind RunPod's SSH proxy only is given up on
/// after `up`'s grace ([`ProxyOnlyClock`]): its host has no public IP, and a longer wait
/// won't give it one. A timeout says why the listing shows no endpoint when it can.
async fn wait_for_endpoint(
    provider: &dyn Provider,
    id: &str,
    timeout_secs: u64,
) -> Result<arena_core::Pod> {
    let policy = arena_core::retry::RetryPolicy::default();
    // tokio's clock (the real one in production): paused-clock tests run these waits instantly.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut proxy_only = ProxyOnlyClock::default();
    let mut last: Option<arena_core::Pod> = None;
    loop {
        let pods =
            arena_core::retry::retrying(&policy, || provider.list_pods()).await.unwrap_or_default();
        let pod = pods.into_iter().find(|p| p.id == id);
        if let Some(p) = pod.as_ref().filter(|p| p.ssh_ip.is_some() && p.ssh_port.is_some()) {
            return Ok(p.clone());
        }
        if let Some(why) = proxy_only.seen(pod.as_ref()) {
            anyhow::bail!("{why}");
        }
        last = pod.or(last);
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out after {timeout_secs}s{}", endpoint_gap(last.as_ref()));
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// How long a pod has been listed as up behind RunPod's SSH proxy only
/// ([`arena_core::Pod::proxy_only`]), for the endpoint waits outside `up` (`replace`/`migrate`'s
/// new pod, `start`/`reimage`/`restart`'s settle). Lane P made such a pod a host failure in `up`
/// after [`up::NO_PUBLIC_IP_GRACE`]: no public IP, so no scp, rsync or port-forward. These
/// waits used to run their whole window (up to 15 min) and then say only "timed out". As in
/// `up`, a listing that lacks the pod proves nothing and keeps the clock going (one failed
/// backend call drops all its pods from the aggregate listing). Only a listing that shows the
/// pod not proxy-only resets it.
#[derive(Debug, Default)]
struct ProxyOnlyClock(Option<tokio::time::Instant>);

impl ProxyOnlyClock {
    /// A listing's view of the pod (`None`: not in it). `Some(why)` once it has been proxy-only
    /// for the whole grace.
    fn seen(&mut self, pod: Option<&arena_core::Pod>) -> Option<String> {
        match pod {
            Some(p) if p.proxy_only && (p.ssh_ip.is_none() || p.ssh_port.is_none()) => {
                let since = *self.0.get_or_insert_with(tokio::time::Instant::now);
                (since.elapsed() >= up::NO_PUBLIC_IP_GRACE).then(|| {
                    format!(
                        "{} for {} — its host has no public IP (no scp/rsync/port-forwards)",
                        arena_core::fleet::PROXY_ONLY,
                        arena_core::pipeline::fmt_elapsed(since.elapsed())
                    )
                })
            }
            Some(_) => {
                self.0 = None;
                None
            }
            None => None,
        }
    }
}

/// Why the last listing showed `pod` without an endpoint, as a suffix for a wait's timeout:
/// proxy-only, or stopped; empty when there's nothing more to say (still coming up).
fn endpoint_gap(pod: Option<&arena_core::Pod>) -> String {
    match pod {
        Some(p) if p.proxy_only => format!(" — {} (its host has no public IP)", arena_core::fleet::PROXY_ONLY),
        Some(p) => arena_core::fleet::no_endpoint_reason(p).map(|w| format!(" — {w}")).unwrap_or_default(),
        None => String::new(),
    }
}

/// Re-resolve a pod's *current* SSH endpoint into a target. Endpoints can be reassigned at
/// any time (more so on busy hosts), so callers fetch fresh right before each transfer.
async fn fresh_target(
    provider: &dyn Provider,
    id: &str,
    cfg: &Config,
) -> Result<arena_core::ssh::SshTarget> {
    let pod = wait_for_endpoint(provider, id, 120).await?;
    Ok(arena_core::ssh::SshTarget::from_pod(&pod, cfg)?)
}

/// The on-pod path + per-pod token of the copy marker (a dotfile the copy carries). Reading
/// it back from a freshly-resolved, identity-confirmed dest proves (a) the copy *delivered*
/// and (b) — after a wait — that it *persisted* (a pod that migrated and reset to image state
/// loses it). Used by both `copy_pod_files` and the post-copy persistence re-check.
fn copy_marker_path() -> &'static str {
    "$HOME/.arena_replace_marker"
}
fn copy_marker_token(dest_id: &str) -> String {
    format!("arena-replace-{dest_id}")
}

/// Is the copy marker present on pod `dest_id` *with the right token, on the right pod*?
/// Re-resolves the endpoint fresh, confirms identity via `RUNPOD_POD_ID` (from
/// `/proc/1/environ` — it's not in the non-interactive ssh env but is in PID 1's), and reads
/// the marker. Any failure (unreachable, wrong recycled-port pod, marker gone, a probe that
/// ran out of its `PROBE_TIMEOUT`) → false.
async fn marker_present(provider: &dyn Provider, remote: &dyn Remote, dest_id: &str, cfg: &Config) -> bool {
    let token = copy_marker_token(dest_id);
    let Ok(target) = fresh_target(provider, dest_id, cfg).await else { return false };
    let probe = format!(
        "printf 'ID=%s\\nMARK=%s\\n' \
         \"$(tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p')\" \
         \"$(cat \"{}\" 2>/dev/null)\"",
        copy_marker_path()
    );
    let Ok(o) = remote.exec(&target, &probe, Some(PROBE_TIMEOUT)).await else { return false };
    if !o.success {
        return false;
    }
    let (mut pid, mut mark) = (String::new(), String::new());
    for line in o.stdout.lines() {
        if let Some(v) = line.strip_prefix("ID=") { pid = v.trim().to_string(); }
        if let Some(v) = line.strip_prefix("MARK=") { mark = v.trim().to_string(); }
    }
    // Identity: on RunPod, FAIL CLOSED — require a non-empty RUNPOD_POD_ID that matches. An
    // empty read on a recycled-port pod (or one that can't expose the id) must not be treated
    // as "right pod"; the marker token alone can ride along a copy that landed on the wrong
    // pod. Only non-RunPod providers (no such id) fall back to token-only.
    let identity_ok = if provider.name() == "runpod" { pid == dest_id } else { pid.is_empty() || pid == dest_id };
    mark == token && identity_ok
}

/// Confirm the pod answering at `target` really is `expected_id` (via `RUNPOD_POD_ID` in
/// PID 1's env). On RunPod, fail closed: a mismatch/empty means a recycled ip:port reached a
/// DIFFERENT pod, so we must not read from / write to it. Non-RunPod providers (no such id)
/// pass. Used to identity-check the SOURCE before copying (the dest is checked separately).
/// A probe that times out (`PROBE_TIMEOUT`) is "couldn't confirm" → false.
async fn target_is_pod(
    remote: &dyn Remote,
    target: &arena_core::ssh::SshTarget,
    expected_id: &str,
    provider: &dyn Provider,
) -> bool {
    if provider.name() != "runpod" {
        return true;
    }
    let probe = "tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p'";
    matches!(remote.exec(target, probe, Some(PROBE_TIMEOUT)).await, Ok(o) if o.success && o.stdout.trim() == expected_id)
}

/// Best-effort removal of the copy marker from a pod.
async fn clean_marker(provider: &dyn Provider, remote: &dyn Remote, dest_id: &str, cfg: &Config) {
    if let Ok(t) = fresh_target(provider, dest_id, cfg).await {
        let _ = remote.exec(&t, &format!("rm -f \"{}\"", copy_marker_path()), Some(PROBE_TIMEOUT)).await;
    }
}

/// Wait until pod `id`'s SSH endpoint has been present and UNCHANGED for `stable_secs` — a
/// freshly-created pod's ip:port churns while RunPod places it, and a pod that's still moving
/// can migrate and reset its container disk (wiping any setup+copy). Provisioning a *settled*
/// pod avoids most of that. Polls every ~15s up to `timeout_secs`; returns the settled pod,
/// or the current pod at timeout if it at least has an endpoint (else errors).
async fn wait_for_stable_endpoint(
    provider: &dyn Provider,
    id: &str,
    stable_secs: u64,
    timeout_secs: u64,
) -> Result<arena_core::Pod> {
    wait_for_stable_endpoint_every(provider, id, stable_secs, timeout_secs, Duration::from_secs(15)).await
}

/// [`wait_for_stable_endpoint`], polling every `poll` (tests pass zero).
async fn wait_for_stable_endpoint_every(
    provider: &dyn Provider,
    id: &str,
    stable_secs: u64,
    timeout_secs: u64,
    poll: Duration,
) -> Result<arena_core::Pod> {
    let policy = arena_core::retry::RetryPolicy::default();
    // tokio's clock (the real one in production): paused-clock tests run these waits instantly.
    let start = tokio::time::Instant::now();
    let mut last: Option<(String, u16)> = None;
    let mut stable_since = tokio::time::Instant::now();
    let mut latest: Option<arena_core::Pod> = None;
    let mut proxy_only = ProxyOnlyClock::default();
    loop {
        let pods =
            arena_core::retry::retrying(&policy, || provider.list_pods()).await.unwrap_or_default();
        let pod = pods.into_iter().find(|p| p.id == id);
        if let Some(why) = proxy_only.seen(pod.as_ref()) {
            anyhow::bail!("{why}");
        }
        let ep = pod.as_ref().and_then(|p| p.ssh_ip.clone().zip(p.ssh_port));
        latest = pod.or(latest);
        match ep {
            Some(cur) => {
                if last.as_ref() != Some(&cur) {
                    last = Some(cur);
                    stable_since = tokio::time::Instant::now();
                } else if stable_since.elapsed().as_secs() >= stable_secs {
                    return Ok(latest.unwrap());
                }
            }
            None => {
                last = None;
                stable_since = tokio::time::Instant::now();
            }
        }
        if start.elapsed().as_secs() >= timeout_secs {
            let gap = endpoint_gap(latest.as_ref());
            return latest
                .filter(|p| p.ssh_ip.is_some() && p.ssh_port.is_some())
                .ok_or_else(|| anyhow::anyhow!("endpoint never stabilized within {timeout_secs}s{gap}"));
        }
        tokio::time::sleep(poll).await;
    }
}

/// The context `replace` puts on a failed copy. The new pod is left running (and billed),
/// and a plain re-run of `replace` refuses while `<name>-new` exists — so say what to do:
/// `migrate copy` re-uses `-new` (rsync continues where it stopped), then `migrate
/// cutover` swaps it in; or terminate it and start over.
fn replace_copy_failed(canonical: &str, new_name: &str) -> String {
    format!(
        "copying {canonical} -> {new_name}. {new_name} is left running (billed). Continue the \
         copy onto it with `arena pods migrate copy {canonical}`, then `arena pods migrate \
         cutover {canonical}`; or remove it with `arena pods terminate {new_name}`"
    )
}

/// How [`copy_pod_files`] runs: whether the destination is a pod this command just created
/// (its replica baseline starts now — [`arena_core::replica::receiver_prepare_command`]) and, so
/// tests can drive the via-local copy with the real rsync, which rsync runs and where its
/// staging dir goes.
struct CopyOpts {
    fresh: bool,
    rsync: String,
    stage_root: PathBuf,
}

impl CopyOpts {
    fn new(fresh: bool) -> Self {
        Self { fresh, rsync: RSYNC.to_string(), stage_root: std::env::temp_dir() }
    }
}

/// How often a via-local copy tries its staging dir's lock again, 200 ms apart, before it
/// takes the dir for another run's (a look at the box holds it only for an instant).
const STAGE_LOCK_TRIES: u32 = 10;

/// The via-local copy's staging dir for source pod `src_id` under `root`.
fn staging_dir(root: &std::path::Path, src_id: &str) -> PathBuf {
    arena_core::disk::staging_dir(root, src_id)
}

/// Remove a leftover via-local staging copy of `src_id` (the participant's whole home), once
/// it can't be needed again: the source pod is gone. Under the stage's lock, held through the
/// removal: one another run is copying through is left alone, said. Says so when there was one.
fn remove_staging_for(root: &std::path::Path, src_id: &str) {
    let stage = staging_dir(root, src_id);
    let _held = match arena_core::disk::lock_dir(&stage) {
        Ok(held) => held,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return eprintln!("      the local staging copy {} is in use by another arena run — left as it is", stage.display())
        }
        Err(e) => return eprintln!("      warning: couldn't lock the local staging copy {} to remove it ({e}) — left as it is", stage.display()),
    };
    let size = arena_core::pull::human_bytes(local_size(&stage));
    match std::fs::remove_dir_all(&stage) {
        Ok(()) => println!("      removed the leftover local staging copy {} ({size})", stage.display()),
        Err(e) => eprintln!("      warning: couldn't remove the local staging copy {} ({size}): {e}", stage.display()),
    }
}

/// Whether `dir` holds any file (at any depth; links not followed) — a staging dir with none
/// has nothing worth keeping.
fn holds_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| match e.file_type() {
            Ok(t) if t.is_dir() => holds_files(&e.path()),
            Ok(_) => true,
            Err(_) => true,
        })
    })
}

/// The last non-empty stderr line of a failed command, clipped — what a fallback's note
/// quotes as the reason.
fn last_line(stderr: &str) -> String {
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no error output");
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(159).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Why an rsync over ssh failed, for the fallback's note (pure, tested): the last stderr line
/// that isn't rsync's own epilogue — its `rsync error: … (code 255)` summary and "connection
/// unexpectedly closed" say only that the transport died, and live R printed just that for a
/// missing pod-to-pod key, hiding ssh's `Permission denied (publickey)` above it. ssh's
/// known-hosts notice isn't a reason either. With nothing else there, rsync's own last line.
fn copy_failure_reason(stderr: &str) -> String {
    let generic = |l: &str| {
        l.starts_with("rsync error:")
            || l.contains("connection unexpectedly closed")
            || l.starts_with("Warning: Permanently added")
    };
    match stderr.lines().rev().map(str::trim).find(|l| !l.is_empty() && !generic(l)) {
        Some(line) => last_line(line),
        None => last_line(stderr),
    }
}

/// The identity check that every replica step on pod `id` runs first, in the same session
/// ([`arena_core::replica::identity_condition`]): on RunPod, PID 1's `RUNPOD_POD_ID` must be
/// `id`. Other providers expose no such id: `None` (as [`target_is_pod`]).
fn pod_guard(provider: &dyn Provider, id: &str) -> Option<String> {
    (provider.name() == "runpod").then(|| arena_core::replica::identity_condition(arena_core::replica::POD_ID_ENVIRON, id))
}

/// Why a replica sync was refused before anything was transferred, for the operator: what
/// the reset guard found ([`arena_core::replica::mirror_refusal`]) and what's still possible.
fn mirror_refusal_text(r: &arena_core::replica::MirrorRefusal, src: &str, dest: &str) -> String {
    use arena_core::replica::MirrorRefusal;
    // To the second: a reset and a sync in the same minute read as contradictory to the
    // minute ("container from 14:54, sync from 14:54" — live R, 2026-10-09).
    let when = |t: u64| {
        let day = arena_core::schedule::ymd_string((t / 86_400) as i64);
        format!("{day} {:02}:{:02}:{:02} UTC", t % 86_400 / 3600, t % 3600 / 60, t % 60)
    };
    let why = match r {
        MirrorRefusal::SourceReset { container, last_sync } => format!(
            "{src} was reset since {dest}'s last sync (its container dates from {}, the sync from {}): its home is \
             the image again",
            when(*container),
            when(*last_sync)
        ),
        MirrorRefusal::NoRecord { last_sync } => format!(
            "{src} has no record of {dest}'s last sync ({}) — it was reset or rebuilt since (the record goes with its \
             home), or {dest} was synced by an older arena",
            when(*last_sync)
        ),
    };
    format!(
        "NOT syncing {src} -> {dest}: {why}. A sync mirrors deletions, so it would delete from {dest} every file \
         of the participant's that {src} no longer has. Nothing was copied; both pods are untouched. {dest} keeps \
         its copy from that sync — cut over to it as it is (`arena pods migrate cutover <name> --no-final-sync`; \
         what changed on {src} since stays there), or terminate {dest} and copy afresh."
    )
}

/// Copy the source pod's home onto the destination pod (`replace`, `migrate copy` /
/// `cutover`). Tries a **direct** pod-to-pod rsync (run on the source, pushing to the dest
/// endpoint with the deploy key both pods hold); on failure falls back to **via-local** (pull
/// the source home into a control-side staging dir, then push it up to the dest). Both use
/// the replication filters (`arena_core::pull::PullConfig::replication`: caches, HF models,
/// `.claude`, `.ssh`, shell rc files, `~/.name` stay behind), and both carry the ARENA repo as
/// its tree (`arena_core::volume::pod_to_pod_copy` / `push_jobs`): on a pod with a volume the
/// repo is a link onto /workspace, and copying the link alone would leave the work behind.
///
/// The copy is a REPLICA ([`arena_core::replica`]): a file deleted on the source is deleted
/// on the destination too (live finding #20), but whatever the sync overwrites or deletes on
/// the destination is moved aside there (`~/arena-sync-kept/<stamp>/`, a visible folder the
/// backups carry), never destroyed — the destination may hold work of its own (live finding
/// #7: a cutover, work, a revert, then a copy that overwrote it). Before the sync the
/// destination lists what changed on it since its last one (by ctime); after a VERIFIED sync
/// it drops only the moved files on neither side of that line, and keeps and lists the rest;
/// after one that wasn't verified it drops nothing. Never `--delete` on the source, which is
/// only read, nor on any backup.
///
/// Correctness guards, learned the hard way against churning pods:
/// 1. **Both pods must be up** — a stopped source is said as such (not as a "reassigned
///    endpoint", which read as "re-run" — live finding #16); so is a stopped destination,
///    whose listed endpoint is stale.
/// 2. **Every step runs on the right pod** — on RunPod each one (the source's prepare and
///    the copy run on it; the destination's prepare, the transfer's remote rsync
///    (`--rsync-path`) and the tidy-up) first checks `RUNPOD_POD_ID` in the same session and
///    does nothing elsewhere: a recycled ip:port would otherwise copy a STRANGER's home, or
///    mirror onto one and move its files aside.
/// 3. **A mirror only from an unchanged source** — onto a destination that was synced
///    before, the source must still be what that sync copied: not reset since (its container
///    older than the destination's stamp) and holding its record of that sync (which a reset
///    takes with the home). Otherwise the sync is refused before anything moves: mirroring a
///    reset pod's bare image would delete the participant's files from the copy.
/// 4. **Fresh endpoints per transfer** — `src`/`dest` SSH ip:port are re-resolved
///    immediately before each rsync. An endpoint captured before a multi-GB pull can go
///    stale mid-pull, sending the push to a since-reassigned port.
/// 5. **Delivery is verified, not assumed** — a marker is planted on the source, carried by
///    the copy, and read back from a freshly-resolved dest. A push that "succeeded" against
///    a stale endpoint fails this check, so we never swap in a pod that didn't get the data.
///
/// The direct copy is skipped when both pods share a public IP — a pod can't reach its
/// neighbour through it, and the hairpin connect just hangs (live finding #30) — and its
/// ssh gives up connecting after seconds; when it fails, the reason printed is its own.
///
/// The via-local copy checks first (an rsync dry run) that the staging fits on the local
/// disk with room to spare — this box is shared with the live cohort — keeps the stage
/// private (0700: it is a participant's home), and removes it once the copy has landed; a
/// failed copy keeps it (a re-run continues from it), saying where and how big (live
/// finding #13: 3.1 GB per pod was left in /tmp, forever).
///
/// The probes and the direct copy go through `remote`, each with a budget (the copy gets
/// [`POD_COPY_TIMEOUT`]). The via-local legs spawn `rsync` itself ([`CopyLeg`], the same
/// budget per leg): rsync runs its own ssh transport, which is neither a `Remote` exec nor a copy.
async fn copy_pod_files(
    cfg: &Config,
    provider: &dyn Provider,
    remote: &dyn Remote,
    src_id: &str,
    dest_id: &str,
    opts: &CopyOpts,
) -> Result<()> {
    use arena_core::pull::{self, Mirror, PullConfig};
    use arena_core::replica::{self, SettleMode, BOOKKEEPING_TIMEOUT, WRONG_POD_EXIT};
    use arena_core::volume;
    let remote_key = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string();
    let user = cfg.get("SSH_USER").unwrap_or("root").to_string();
    // What the via-local staging must leave free on this box (CONTROL_MIN_FREE_GB): a bad value
    // fails here, before anything is copied — and replace/migrate refuse it before they create
    // anything (`disk::floor_refusal`): found only here, it left the new pod billing.
    let floor = disk::floor(cfg)?;
    // The repo's place in the home, carried as a TREE: on a pod with a volume it is a link
    // onto /workspace (setup put it there), and a plain home copy would carry only the link —
    // leaving the work on the old pod's volume, which goes with it.
    let repo = arena_core::backup::repo_path(cfg);
    let rel = volume::repo_pull(&repo, &user, None).map(|p| p.rel);
    let (src_guard, dest_guard) = (pod_guard(provider, src_id), pod_guard(provider, dest_id));

    // 1. Both pods must be up: the source to be read, the destination to be written — a
    // stopped one is listed with the endpoint it HAD, which may be another pod's by now.
    if let Ok(listed) = provider.list_pods().await {
        if let Some(p) = listed.iter().find(|p| p.id == src_id).filter(|p| arena_core::status::is_stopped(&p.status)) {
            anyhow::bail!("the source {} is stopped ({}) — there is nothing running to sync from", p.name, p.status);
        }
        if let Some(p) = listed.iter().find(|p| p.id == dest_id).filter(|p| arena_core::status::is_stopped(&p.status)) {
            anyhow::bail!(
                "the destination {} is stopped ({}) — start it first (`arena pods start {}`); nothing was copied",
                p.name,
                p.status,
                p.name
            );
        }
    }
    // Identity-check the SOURCE before reading from it: if its ip:port was reassigned to a
    // different pod, we'd otherwise copy a STRANGER's home onto the new pod (and the dest
    // marker check would still pass, since the marker rides along). Fail closed.
    let src_target = fresh_target(provider, src_id, cfg).await.context("resolving source endpoint")?;
    if !target_is_pod(remote, &src_target, src_id, provider).await {
        anyhow::bail!(
            "the source {src_id}'s SSH endpoint doesn't answer as that pod (it's down, or its ip:port was \
             reassigned to another pod) — refusing to copy from it"
        );
    }
    // Plant a delivery marker on the source (a dotfile the copy carries, not matched by any
    // exclude) — verified on the dest after the copy; the dest copy is left in place for the
    // caller's persistence re-check (then cleaned up there) — and read what the reset guard
    // needs: the source's record of its syncs, and its container's age.
    let token = copy_marker_token(dest_id);
    let marker = copy_marker_path();
    let prep = replica::source_prepare_command(src_guard.as_deref(), marker, &token, dest_id, volume::CONTAINER_MARK);
    let source = match remote.exec(&src_target, &prep, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => replica::parse_source_state(&o.stdout),
        Ok(o) if o.code == Some(WRONG_POD_EXIT) => anyhow::bail!(
            "the source {src_id}'s endpoint answered as a different pod — refusing to copy the wrong pod's data. Re-run."
        ),
        _ => None,
    }
    .ok_or_else(|| anyhow::anyhow!("couldn't prepare the source {src_id} for the copy (planting its marker) — nothing was copied"))?;
    let unmark = |landed: bool| replica::source_finish_command(src_guard.as_deref(), marker, dest_id, landed);

    // The destination, in one session behind its identity check: its baseline when this
    // command just made it (what's on it is image + setup), its last sync, and the list of
    // what changed on it since — what the tidy-up afterwards may never drop.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let stamp = replica::sync_stamp(now);
    let dest_pod = wait_for_endpoint(provider, dest_id, 120).await.context("resolving dest endpoint")?;
    let prepared = match SshTarget::from_pod(&dest_pod, cfg) {
        Ok(t) => match remote.exec(&t, &replica::receiver_prepare_command(dest_guard.as_deref(), &stamp, rel.as_deref(), opts.fresh), Some(BOOKKEEPING_TIMEOUT)).await {
            Ok(o) if o.success => replica::parse_prepared(&o.stdout).ok_or_else(|| "it didn't say it was ready".to_string()),
            Ok(o) if o.code == Some(WRONG_POD_EXIT) => Err(format!(
                "its endpoint {}:{} doesn't answer as {dest_id} (reassigned to another pod?)",
                dest_pod.ssh_ip.as_deref().unwrap_or("?"),
                dest_pod.ssh_port.unwrap_or(0)
            )),
            Ok(o) => Err(last_line(&o.stderr)),
            Err(e) => Err(describe_error(&e)),
        },
        Err(e) => Err(e.to_string()),
    };
    let prepared = match prepared {
        Ok(p) => p,
        Err(why) => {
            let _ = remote.exec(&src_target, &unmark(false), Some(PROBE_TIMEOUT)).await;
            anyhow::bail!("couldn't prepare {} for the sync: {why} — nothing was copied", dest_pod.name);
        }
    };
    // 3. A mirror only from a source that is still what the last sync copied.
    if let Some(r) = replica::mirror_refusal(opts.fresh, prepared.last_sync, &source) {
        let _ = remote.exec(&src_target, &unmark(false), Some(PROBE_TIMEOUT)).await;
        let src_name = provider
            .list_pods()
            .await
            .ok()
            .and_then(|ps| ps.into_iter().find(|p| p.id == src_id).map(|p| p.name))
            .unwrap_or_else(|| src_id.to_string());
        anyhow::bail!("{}", mirror_refusal_text(&r, &src_name, &dest_pod.name));
    }
    let pc = PullConfig {
        mirror: replica::home_mirror(&stamp),
        rsync_path: dest_guard.as_deref().map(replica::guarded_rsync_path),
        ..PullConfig::replication()
    };

    let transfer = async {
        // --- direct attempt: rsync ON the source, pushing to the dest's fresh endpoint ---
        let dest_ip = dest_pod.ssh_ip.clone().unwrap_or_default();
        let direct_ok = if dest_ip == src_target.host {
            eprintln!(
                "      both pods are behind {dest_ip} — a pod can't reach its neighbour through their shared IP, \
                 so copying via local staging…"
            );
            false
        } else {
            let direct = volume::pod_to_pod_copy(&dest_ip, dest_pod.ssh_port.unwrap_or(22), &user, &remote_key, &pc, rel.as_deref());
            // Behind the source's check too: it runs on whatever answers at the source's endpoint.
            let direct = match src_guard.as_deref() {
                Some(cond) => replica::guarded(Some(cond), &direct),
                None => direct,
            };
            match remote.exec(&src_target, &direct, Some(POD_COPY_TIMEOUT)).await {
                Ok(o) if o.success => true,
                Ok(o) => {
                    eprintln!("      direct pod-to-pod copy failed ({}) — copying via local staging…", copy_failure_reason(&o.stderr));
                    false
                }
                // A copy that ran out of its budget was under way (a missing pod-to-pod key fails at
                // once), so the via-local fallback would only repeat it, slower: stop here instead.
                // Nothing has been swapped, and rsync is incremental — copying onto the SAME dest
                // again continues from here. How to do that depends on the caller (`migrate copy`
                // re-uses `-new`; `replace` refuses while it exists), so the caller says it.
                Err(e @ arena_core::Error::Timeout { .. }) => anyhow::bail!(
                    "direct pod-to-pod copy {} — NOT swapping; the original is untouched. What was \
                     copied stays on the new pod: copying onto it again continues from there.",
                    describe_error(&e)
                ),
                Err(e) => {
                    eprintln!("      direct pod-to-pod copy couldn't run ({}) — copying via local staging…", describe_error(&e));
                    false
                }
            }
        };
        if direct_ok {
            println!("      copied (direct pod-to-pod)");
            return Ok(());
        }
        // Refused for room: recorded for the health check, said with how much to free where,
        // arena's biggest dirs there, and another filesystem to stage on (`need`: its size, 0
        // before it is sized).
        let refuse = |skip: &arena_core::disk::Skip, floor, need| {
            disk::note_skips(cfg, "replace/migrate staging", &[(dest_pod.name.clone(), skip.to_string())]);
            anyhow::anyhow!("not copying via local staging ({}): {}", opts.stage_root.display(), disk::staging_advice(cfg, skip, need, floor))
        };
        // Already under the floor: no copy fits, whatever its size — said before it is sized (an
        // rsync dry run over the whole home, for nothing) or anything is made.
        if let Some(floor) = floor {
            if let Some(fs) = arena_core::disk::reading(&opts.stage_root).ok().filter(|fs| fs.free < floor.bytes) {
                if let Err(skip) = arena_core::disk::judge(&fs, 0, &arena_core::disk::Size::Measured(0).into(), floor) {
                    return Err(refuse(&skip, floor, 0));
                }
            }
        }
        // pull source -> staging (fresh source), then push staging -> dest (FRESH dest, right
        // before the push — this is the endpoint most likely to have gone stale during the pull).
        let stage = staging_dir(&opts.stage_root, src_id);
        // Its lock, held for the whole copy: a second replace/migrate of this pod can't write
        // into the same stage, no cleanup removes it under this one (a cleanup holds it while it
        // removes one), and `config check` / `teardown --check` can tell it in use from one left
        // behind (their look takes the lock for an instant: tried again for a moment). Refused,
        // the other run's stage is left exactly as it is.
        let _held = {
            let mut tries = 0;
            loop {
                std::fs::create_dir_all(&stage).with_context(|| format!("creating staging dir {}", stage.display()))?;
                match arena_core::disk::lock_dir(&stage) {
                    Ok(held) => break held,
                    // Removed by a cleanup between the mkdir and the lock: made again.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && tries < STAGE_LOCK_TRIES => tries += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && tries < STAGE_LOCK_TRIES => {
                        tries += 1;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => anyhow::bail!(
                        "another arena run is copying {src_id} through {} right now — nothing copied; let it finish \
                         (or stop it), then re-run",
                        stage.display()
                    ),
                    Err(e) => anyhow::bail!("locking the staging dir {}: {e}", stage.display()),
                }
            }
        };
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making the staging dir {} private", stage.display()))?;
        }
        let stage_s = format!("{}/", stage.to_string_lossy());
        let kept_note = |stage: &std::path::Path| {
            format!(
                "the local staging copy is kept at {} ({}) so a re-run continues from it — `rm -rf {}` if you won't re-run",
                stage.display(),
                arena_core::pull::human_bytes(local_size(stage)),
                stage.display()
            )
        };
        let staged = async {
            let src_target = fresh_target(provider, src_id, cfg).await?;
            if !target_is_pod(remote, &src_target, src_id, provider).await {
                anyhow::bail!(
                    "the source {src_id}'s endpoint answered as a different pod before the pull — refusing to copy \
                     the wrong pod's data. Re-run."
                );
            }
            // Where the source's repo really is (read-only, bounded): a link is pulled as its tree,
            // into its place in the staging dir — as `pods pull` does. The stage is the tool's own
            // scratch copy: an exact mirror, so nothing deleted on the source rides back through it.
            let probe = match remote.exec(&src_target, &volume::probe_command(&repo), Some(PROBE_TIMEOUT)).await {
                Ok(out) if out.success => volume::parse_probe(&out.stdout),
                _ => None,
            };
            // The pulls read only the source: its remote rsync starts only there.
            let staging_pc = PullConfig {
                mirror: Mirror::Exact,
                rsync_path: src_guard.as_deref().map(replica::guarded_rsync_path),
                ..PullConfig::replication()
            };
            let jobs = volume::pull_jobs(&staging_pc, &stage_s, volume::repo_pull(&repo, &user, probe.as_ref()).as_ref());
            // Room first: what the pull would add to the stage (an rsync dry run), against the
            // free space this box keeps (CONTROL_MIN_FREE_GB, `arena_core::disk`).
            let mut needed = 0u64;
            for job in &jobs {
                let mut args = pull::rsync_args(&src_target, &job.config, &job.dest);
                args.insert(0, "--dry-run".into());
                let out = arena_core::remote::run_local(&opts.rsync, &args, "rsync", Some(POD_COPY_TIMEOUT))
                    .await
                    .context("sizing the local staging (rsync --dry-run)")?;
                if !out.success {
                    anyhow::bail!("sizing the local staging (rsync --dry-run) failed: {}", out.stderr.trim());
                }
                needed += arena_core::replica::transfer_bytes(&out.stdout)
                    .ok_or_else(|| anyhow::anyhow!("sizing the local staging: rsync --dry-run reported no size"))?;
            }
            // (Held, against other runs' copies, until the staging is done.)
            let _room = match floor {
                None => disk::Hold::default(),
                Some(floor) => {
                    let copy = [(dest_pod.name.clone(), vec![(opts.stage_root.clone(), arena_core::disk::Size::Measured(needed))])];
                    let (skipped, hold) = disk::judge_copies(cfg, floor, &copy, true);
                    if let Some((_, skip)) = skipped.first() {
                        return Err(refuse(skip, floor, needed));
                    }
                    hold
                }
            };
            for job in jobs.iter().filter(|j| j.repo) {
                clear_stale_link(&job.dest).with_context(|| format!("clearing an old link at {}", job.dest))?;
                std::fs::create_dir_all(&job.dest).with_context(|| format!("creating {}", job.dest))?;
            }
            // Each leg's rsyncs (the home, then a split-out repo) share the leg's one budget. The
            // floor stays a hard line while they write (`disk::Watch`: stopped under it).
            let leg = CopyLeg::start(POD_COPY_TIMEOUT);
            let watch = disk::Watch::new(floor, &[opts.stage_root.as_path()]);
            let pulled = disk::watched(watch.as_ref(), async {
                for job in &jobs {
                    leg.rsync(&opts.rsync, &pull::rsync_args(&src_target, &job.config, &job.dest))
                        .await
                        .context("pull source -> staging")?;
                }
                anyhow::Ok(())
            })
            .await;
            if let Err(skip) = &pulled {
                disk::note_skips(cfg, "replace/migrate staging", &[(dest_pod.name.clone(), skip.to_string())]);
            }
            pulled.map_err(|skip| {
                let advice = match floor {
                    Some(floor) => disk::staging_advice(cfg, &skip, needed, floor),
                    None => skip.to_string(),
                };
                anyhow::anyhow!("pull source -> staging ({}): {advice}", opts.stage_root.display())
            })??;
            let dest_target = fresh_target(provider, dest_id, cfg).await?;
            let staged_repo = rel.as_ref().is_some_and(|r| std::fs::symlink_metadata(stage.join(r)).is_ok_and(|m| m.is_dir()));
            let leg = CopyLeg::start(POD_COPY_TIMEOUT);
            for (src, pc) in volume::push_jobs(&pc, &stage_s, rel.as_deref(), staged_repo) {
                leg.rsync(&opts.rsync, &pull::push_rsync_args(&dest_target, &pc, &src)).await.context("push staging -> dest")?;
            }
            anyhow::Ok(())
        }
        .await;
        match staged {
            Ok(()) => {
                let size = arena_core::pull::human_bytes(local_size(&stage));
                match std::fs::remove_dir_all(&stage) {
                    Ok(()) => println!("      copied (via local staging; the staging copy, {size}, is removed)"),
                    Err(e) => eprintln!("      copied via local staging, but couldn't remove {} ({size}): {e}", stage.display()),
                }
                Ok(())
            }
            // Nothing staged (refused before the pull, or it failed at once): no copy to keep.
            Err(e) if !holds_files(&stage) => {
                let _ = std::fs::remove_dir_all(&stage);
                Err(e)
            }
            Err(e) => Err(e.context(kept_note(&stage))),
        }
    }
    .await;

    // Verify the data actually landed on the RIGHT pod (identity + marker). `marker_present`
    // re-resolves the endpoint fresh and confirms `RUNPOD_POD_ID` matches, so a push that
    // "succeeded" against a since-reassigned port (a different pod) is caught.
    let landed = transfer.is_ok() && marker_present(provider, remote, dest_id, cfg).await;
    // Clean the SOURCE marker now (and record a landed sync there, for the next one's reset
    // guard); leave the DEST marker for the caller's persistence re-check.
    let _ = remote.exec(&src_target, &unmark(landed), Some(PROBE_TIMEOUT)).await;
    // Sort what the sync moved aside on the destination — always, so even a partial sync's
    // report happens; only a verified one drops anything or moves the baseline on. Behind the
    // destination's own check: on any other pod it does nothing.
    let mode = if landed { SettleMode::Advance } else { SettleMode::Keep };
    let kept_at = format!("~/{}/{stamp}", replica::KEPT_DIR);
    match fresh_target(provider, dest_id, cfg).await {
        Ok(t) => match remote.exec(&t, &replica::settle_command(dest_guard.as_deref(), &stamp, rel.as_deref(), mode), Some(BOOKKEEPING_TIMEOUT)).await {
            Ok(o) if o.success => match replica::parse_settle(&o.stdout) {
                Some(s) => {
                    for line in replica::settle_lines(&dest_pod.name, &s, mode) {
                        eprintln!("      {line}");
                    }
                }
                None => eprintln!("      warning: couldn't read what the sync moved aside on {} (see {kept_at} there)", dest_pod.name),
            },
            Ok(o) if o.code == Some(WRONG_POD_EXIT) => eprintln!(
                "      warning: {}'s endpoint no longer answers as it — what the sync moved aside there ({kept_at}) wasn't sorted",
                dest_pod.name
            ),
            _ => eprintln!("      warning: couldn't sort what the sync moved aside on {} (it's in {kept_at} there)", dest_pod.name),
        },
        Err(_) => eprintln!("      warning: couldn't reach {} to sort what the sync moved aside ({kept_at})", dest_pod.name),
    }
    transfer?;
    if !landed {
        anyhow::bail!(
            "copy verification failed — the data didn't reach the intended new pod ({dest_id}) \
             (a churning endpoint likely got reassigned mid-transfer). NOT swapping; the \
             original is untouched. Re-run to retry."
        );
    }
    Ok(())
}

/// One leg of the via-local copy — the pull into staging, or the push out of it — and its
/// clock. A leg is up to two rsyncs (the home, then a linked repo's tree, split out because a
/// home copy alone would carry only the link), and they share ONE budget, as the direct
/// copy's two rsyncs share its one exec: each gets what the ones before it left, and one that
/// would start with nothing left doesn't start. A per-rsync budget would let a split leg run
/// twice as long as the "2 h per leg" the operator is promised.
///
/// The rsyncs are deliberately not [`Remote`] calls (like `pods pull`'s): rsync drives its own
/// ssh transport (`-e`), so it is neither an exec nor a single-file copy — but it is stopped
/// the same way ([`arena_core::remote::run_local`]), and its `--timeout` gives up on a silent
/// connection long before the budget does. A stopped leg swaps nothing: the caller bails
/// before the marker check, and rsync is incremental, so a re-run continues from what was
/// copied.
struct CopyLeg {
    budget: Duration,
    deadline: tokio::time::Instant,
}

impl CopyLeg {
    /// Start the leg's clock: `budget` ([`POD_COPY_TIMEOUT`]) for all of its rsyncs together.
    fn start(budget: Duration) -> Self {
        Self { budget, deadline: tokio::time::Instant::now() + budget }
    }

    /// Run one of the leg's rsyncs (`program` + argv) within what is left of the leg's budget:
    /// an error (with stderr) on a non-zero exit, `rsync timed out after <the leg's budget>`
    /// when the leg runs out — mid-rsync, or before one that would start with nothing left.
    async fn rsync(&self, program: &str, args: &[String]) -> Result<()> {
        let spent = || {
            anyhow::anyhow!(
                "rsync timed out after {} (the copy leg's budget, for all its rsyncs together)",
                arena_core::error::human_duration(&self.budget)
            )
        };
        let left = self.deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(spent());
        }
        match arena_core::remote::run_local(program, args, "rsync", Some(left)).await {
            Ok(out) if out.success => Ok(()),
            Ok(out) => anyhow::bail!("rsync failed: {}", out.stderr.trim()),
            Err(arena_core::Error::Timeout { .. }) => Err(spent()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Pre-swap health check: the replacement must answer over SSH (and report its GPU if it
/// has one). Re-resolves the pod's endpoint fresh on each attempt and retries a few times,
/// because RunPod can reassign a pod's SSH ip/port right after provisioning — a single shot
/// against a stale endpoint spuriously reads as "Permission denied". (A precise copied-size
/// check is approximated by rsync's own success in `copy_pod_files`, plus the manual swap
/// confirm; the post-swap `~/.name` write re-resolves its own endpoint.)
async fn verify_replacement(provider: &dyn Provider, remote: &dyn Remote, id: &str, cfg: &Config) -> Result<()> {
    // A freshly-provisioned pod can take a while to settle into a stable SSH state (it may
    // restart once post-setup), so be patient: ~10 tries over ~100s, re-resolving each time.
    let attempts = 10;
    let mut last = String::from("no attempt made");
    for attempt in 1..=attempts {
        let pod = match wait_for_endpoint(provider, id, 60).await {
            Ok(p) => p,
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
        };
        let target = arena_core::ssh::SshTarget::from_pod(&pod, cfg)?;
        let probe = "nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1; echo SSH_OK";
        // A quick probe: a hung attempt counts as a failed one and the loop retries.
        match remote.exec(&target, probe, Some(PROBE_TIMEOUT)).await {
            Ok(o) if o.success && o.stdout.contains("SSH_OK") => {
                let gpu = o.stdout.lines().find(|l| !l.contains("SSH_OK")).unwrap_or("").trim();
                println!("      ssh ok; gpu: {}", if gpu.is_empty() { "(none reported)" } else { gpu });
                return Ok(());
            }
            Ok(o) => last = format!("ssh connected but health check unhappy: {}", o.stderr.trim()),
            Err(e) => last = e.to_string(),
        }
        if attempt < attempts {
            eprintln!("      verify {attempt}/{attempts} failed ({last}); retrying in 10s…");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    }
    // Verification runs BEFORE the swap, so a failure here is safe: the source pod is still
    // canonical and untouched. Say so, so the failure doesn't read as data loss.
    anyhow::bail!(
        "health check failed after {attempts} attempts: {last}\n\
         No swap was done — the original pod is untouched and still canonical. The replacement \
         is left in place for inspection (terminate it with `arena pods terminate` if unwanted)."
    );
}

/// Single-quote for safe inclusion in a remote `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `replace`/`migrate`'s replacement spec and the `--gpu` check policy, with a fake
/// provider (no network: the configs carry no RUNPOD_API_KEY, so no catalog is fetched).
#[cfg(test)]
mod replacement_spec_tests {
    use super::{build_replacement_spec, gpu_flag_or_warning, gpu_id_from_listing, plan_replacement, PlaceFlags, SpecOverrides};
    use arena_core::provider::runpod::GpuType;
    use arena_core::{Config, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;

    /// A provider whose `pod_spec` returns a fixed snapshot (as RunPod's would: no CUDA
    /// constraint, GPU/cloud blank on v1).
    struct Snapshot {
        name: &'static str,
        spec: PodSpec,
    }

    #[async_trait]
    impl Provider for Snapshot {
        fn name(&self) -> &'static str {
            self.name
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised")
        }
        async fn stop_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
            Ok(self.spec.clone())
        }
    }

    fn snapshot(allowed_cuda: &[&str]) -> PodSpec {
        PodSpec {
            name: String::new(),
            image: "img:cu130".into(),
            gpu_type: String::new(), // v1 can't report it: config's GPU_TYPE fills in
            gpu_count: 1,
            cloud_type: String::new(),
            disk_gb: 40,
            volume_gb: 0,
            ports: "22/tcp".into(),
            env: Vec::new(),
            docker_args: None,
            allowed_cuda: allowed_cuda.iter().map(|s| s.to_string()).collect(),
            min_cuda: None,
            max_price: None,
            api_extra: None,
            avoid_machines: Vec::new(),
            server_type: None,
            vm_image: None,
            region: None,
        }
    }

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nGPU_TYPE=\"NVIDIA RTX A4000\"\nSHARED_SSH_KEY_PATH=/nonexistent/k\n{extra}"
        ))
    }

    /// The source pod as the owner lists it.
    fn src() -> Pod {
        Pod { id: "id-1".into(), name: "devtest-apple".into(), provider: "runpod".into(), status: "RUNNING".into(), ..Default::default() }
    }

    /// Review finding: the replacement never got `ALLOWED_CUDA_VERSIONS` back (the API
    /// doesn't report it), so a cu130 image could land on a CUDA 12 host.
    #[tokio::test]
    async fn replacement_reapplies_the_configured_cuda_constraint() {
        let owner = Snapshot { name: "runpod", spec: snapshot(&[]) };
        let ov = SpecOverrides::default();
        // (config, what the replacement is constrained to)
        // (config, exact set, floor) — ALLOWED_CUDA_VERSIONS is a floor now (finding #10).
        let none: Vec<&str> = vec![];
        for (extra, exact, floor) in [
            ("ALLOWED_CUDA_VERSIONS=\"13.0\"\n", none.clone(), Some("13.0")),
            ("ALLOWED_CUDA_VERSIONS=\"13.0, 12.8\"\n", none.clone(), Some("12.8")),
            ("RUNPOD_ALLOWED_CUDA_VERSIONS=13.0\n", none.clone(), Some("13.0")), // the legacy key, as create reads it
            ("MIN_CUDA_VERSION=13.0\n", none.clone(), Some("13.0")),
            ("EXACT_CUDA_VERSIONS=13.0,13.1\n", vec!["13.0", "13.1"], None),
            ("", none.clone(), None), // unconfigured: any host, as before
        ] {
            let (spec, notes) = build_replacement_spec(&cfg(extra), &owner, "id-1", "devtest-apple", None, &ov).await.unwrap();
            assert_eq!((spec.allowed_cuda.clone(), spec.min_cuda.as_deref()), (exact.iter().map(|s| s.to_string()).collect(), floor), "{extra:?}");
            assert!(notes.iter().any(|n| n.starts_with("GPU NVIDIA RTX A4000: config GPU_TYPE")), "{notes:?}");
            assert_eq!((spec.gpu_type.as_str(), spec.disk_gb), ("NVIDIA RTX A4000", 40), "snapshot + config fallback");
        }
        // A snapshot that does carry a constraint keeps it (and gets no floor beside it).
        let pinned = Snapshot { name: "runpod", spec: snapshot(&["12.8"]) };
        let (spec, _) = build_replacement_spec(&cfg("ALLOWED_CUDA_VERSIONS=13.0\n"), &pinned, "id-1", "x", None, &ov).await.unwrap();
        assert_eq!((spec.allowed_cuda.clone(), spec.min_cuda), (vec!["12.8".to_string()], None));
        // --gpu still applies on top (unchecked here: no key → no catalog) — through the plan.
        let ov = SpecOverrides { gpu: Some("3070".into()), ..Default::default() };
        let rep = plan_replacement(&cfg("ALLOWED_CUDA_VERSIONS=13.0\n"), &owner, &src(), &ov, &PlaceFlags::default()).await.unwrap();
        let spec = rep.first_spec("devtest-apple-new");
        assert_eq!((spec.gpu_type.as_str(), spec.min_cuda.as_deref()), ("NVIDIA GeForce RTX 3070", Some("13.0")));
        assert!(rep.notes.iter().all(|n| !n.starts_with("GPU ")), "--gpu given: no note on where the GPU came from: {:?}", rep.notes);
    }

    /// Config CREATE_EXTRA_JSON reaches a replacement's (and `migrate copy`'s) create like
    /// any create's, checked against the owner's own fields — refused before anything is
    /// created; a backend that can't merge extras refuses it outright.
    #[tokio::test]
    async fn replacement_carries_config_create_extra_json_checked_against_the_owner() {
        /// The snapshot backend with RunPod v2's create rules.
        struct V2Owner(Snapshot);
        #[async_trait]
        impl Provider for V2Owner {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> Result<Vec<Pod>> {
                Ok(Vec::new())
            }
            async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
                unimplemented!("not exercised")
            }
            async fn stop_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn restart_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
                self.0.pod_spec(id).await
            }
            fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> Result<()> {
                arena_core::provider::runpod_v2::RunpodV2Provider::new("k").check_create_extra(extra)
            }
        }
        let owner = V2Owner(Snapshot { name: "runpod", spec: snapshot(&[]) });
        let ov = SpecOverrides::default();
        let (spec, _) = build_replacement_spec(&cfg(""), &owner, "id-1", "x", None, &ov).await.unwrap();
        assert_eq!((spec.api_extra.as_ref(), super::extra_keys_line(&spec)), (None, None));
        let (spec, _) = build_replacement_spec(&cfg(r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"],"env":{"A":"hf_x"}}'"#), &owner, "id-1", "x", None, &ov)
            .await
            .unwrap();
        assert_eq!(spec.api_extra.as_ref().unwrap()["dataCenterIds"], serde_json::json!(["EU-RO-1"]));
        assert_eq!(
            super::extra_keys_line(&spec).unwrap(),
            "+ --api-json / CREATE_EXTRA_JSON [dataCenterIds, env] merged into the create request"
        );
        for (line, want) in [
            (r#"CREATE_EXTRA_JSON='{"gpu":{"id":"x"}}'"#, "`gpu.id` is set by arena"),
            ("CREATE_EXTRA_JSON='nope'", "CREATE_EXTRA_JSON: not valid JSON"),
        ] {
            let e = build_replacement_spec(&cfg(line), &owner, "id-1", "x", None, &ov).await.unwrap_err().to_string();
            assert!(e.contains(want), "{line}: {e}");
        }
        // The plain snapshot backend merges no extras: refused, not silently dropped.
        let plain = Snapshot { name: "vast", spec: snapshot(&[]) };
        let e = build_replacement_spec(&cfg(r#"CREATE_EXTRA_JSON='{"price":0.1}'"#), &plain, "id-1", "x", None, &ov).await.unwrap_err();
        assert!(e.to_string().contains("isn't supported on provider `vast`"), "{e}");
    }

    /// Live finding #17: on RunPod v1 the source's spec has no GPU type, so a replacement of a
    /// 1×RTX 3070 pod was created on config's A4000. The source's own GPU comes from its
    /// (enriched) listing first — mapped to a create id through the catalog, or through the
    /// known aliases without one — and config only when that can't be read (said in a note).
    #[tokio::test]
    async fn a_replacement_keeps_the_sources_gpu_from_its_listing() {
        let catalog = || {
            Ok(vec![
                GpuType { id: "NVIDIA GeForce RTX 3070".into(), display_name: "RTX 3070".into(), memory_gb: 8, ..Default::default() },
                GpuType { id: "NVIDIA B300 MIG 34GB".into(), display_name: "B300 MIG 34GB".into(), memory_gb: 34, ..Default::default() },
            ])
        };
        // (as the listing shows it, the catalog?, the create id)
        let cases: [(&str, bool, Option<&str>); 7] = [
            ("RTX 3070", true, Some("NVIDIA GeForce RTX 3070")),
            ("B300 MIG 34GB", true, Some("NVIDIA B300 MIG 34GB")),
            ("NVIDIA GeForce RTX 3070", true, Some("NVIDIA GeForce RTX 3070")),
            ("RTX 3070", false, Some("NVIDIA GeForce RTX 3070")), // a known alias, no catalog needed
            ("B300 MIG 34GB", false, Some("NVIDIA B300 SXM6 AC MIG 1g.34gb")), // the unlisted slice's alias
            ("PRO 6000 MIG 24GB", false, None),                   // no catalog, no alias: no guess
            ("Mystery GPU", true, None),
        ];
        for (shown, with_catalog, want) in cases {
            let cat = if with_catalog { catalog() } else { Err("offline".to_string()) };
            assert_eq!(gpu_id_from_listing(shown, cat).as_deref(), want, "{shown} (catalog: {with_catalog})");
        }

        /// v1's shape: no GPU in the spec snapshot, the listing's details name it.
        struct V1Owner;
        #[async_trait]
        impl Provider for V1Owner {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> Result<Vec<Pod>> {
                Ok(Vec::new())
            }
            async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
                for p in pods.iter_mut() {
                    p.gpu_type = Some("RTX 3070".into());
                }
                Ok(())
            }
            async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
                unimplemented!("not exercised")
            }
            async fn stop_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn restart_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
                Ok(snapshot(&[]))
            }
        }
        let rep = plan_replacement(&cfg(""), &V1Owner, &src(), &SpecOverrides::default(), &PlaceFlags::default()).await.unwrap();
        assert_eq!(rep.first_spec("devtest-apple-new").gpu_type, "NVIDIA GeForce RTX 3070", "not config's A4000");
        assert!(rep.notes.iter().any(|n| n.starts_with("GPU NVIDIA GeForce RTX 3070: the source's, from its listing")), "{:?}", rep.notes);
        assert!(rep.notes.iter().any(|n| n.starts_with("tier COMMUNITY: config CLOUD_TYPE")), "{:?}", rep.notes);
        // A listing that can't say: config, with a note telling the operator to choose.
        let blank = Snapshot { name: "runpod", spec: snapshot(&[]) };
        let rep = plan_replacement(&cfg(""), &blank, &src(), &SpecOverrides::default(), &PlaceFlags::default()).await.unwrap();
        assert_eq!(rep.first_spec("x").gpu_type, "NVIDIA RTX A4000");
        assert!(rep.notes.iter().any(|n| n.contains("config GPU_TYPE") && n.contains("pass --gpu")), "{:?}", rep.notes);
    }

    /// Live finding #19: `--gpu 3070,A4000` reached the API as ONE id
    /// (`NVIDIA GeForce RTX 3070,NVIDIA RTX A4000`, a 400). A list is placement options now —
    /// one create at a time, in order — each a real id; a single `--gpu` stays one option.
    #[tokio::test]
    async fn a_replacement_gpu_list_is_placement_options_not_one_id() {
        let owner = Snapshot { name: "runpod", spec: snapshot(&[]) };
        let ov = SpecOverrides { gpu: Some("3070,A4000".into()), ..Default::default() };
        let place = PlaceFlags { order: arena_core::placement::Order::Listed, ..Default::default() };
        let rep = plan_replacement(&cfg(""), &owner, &src(), &ov, &place).await.unwrap();
        let gpus: Vec<&str> = rep.plan.options.iter().map(|o| o.gpu.as_str()).collect();
        assert_eq!(gpus, ["NVIDIA GeForce RTX 3070", "NVIDIA RTX A4000"]);
        assert!(rep.is_placement());
        assert!(!rep.first_spec("x").gpu_type.contains(','), "never a comma-joined id");
        let one = plan_replacement(&cfg(""), &owner, &src(), &SpecOverrides { gpu: Some("3070".into()), ..Default::default() }, &PlaceFlags::default())
            .await
            .unwrap();
        assert_eq!((one.plan.options.len(), one.is_placement()), (1, false));
        // A list on the cloud too: gpu × cloud, as `up` expands it — with NEAR on (the
        // default), SECURE tried in Europe just before anywhere (as listed); off, as before.
        let ov = SpecOverrides { gpu: Some("3070".into()), cloud: Some("community,secure".into()), ..Default::default() };
        let rep = plan_replacement(&cfg(""), &owner, &src(), &ov, &place).await.unwrap();
        let tiers: Vec<(Option<&str>, bool)> = rep.plan.options.iter().map(|o| (o.cloud.as_deref(), o.near.is_some())).collect();
        assert_eq!(tiers, [(Some("COMMUNITY"), false), (Some("SECURE"), true), (Some("SECURE"), false)]);
        let rep = plan_replacement(&cfg("NEAR=any\n"), &owner, &src(), &ov, &place).await.unwrap();
        let tiers: Vec<Option<&str>> = rep.plan.options.iter().map(|o| o.cloud.as_deref()).collect();
        assert_eq!(tiers, [Some("COMMUNITY"), Some("SECURE")]);
    }

    /// Finding #22: on REST v1 a catalog GPU its create enum lacks (the PRO 6000 MIG slices)
    /// is refused before any create, by its display name, saying how to get back to v2 (the
    /// default); an unreadable enum refuses nothing.
    #[test]
    fn v1_refuses_a_gpu_its_create_api_rejects() {
        use super::v1_creatable_check;
        let mig = "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb";
        let catalog = vec![
            GpuType { id: mig.into(), display_name: "PRO 6000 MIG 24GB".into(), memory_gb: 24, ..Default::default() },
            GpuType { id: "NVIDIA RTX A4000".into(), display_name: "RTX A4000".into(), memory_gb: 16, ..Default::default() },
        ];
        let creatable = vec!["NVIDIA RTX A4000".to_string(), "NVIDIA B300 SXM6 AC MIG 1g.34gb".to_string()];
        assert!(v1_creatable_check("NVIDIA RTX A4000", &catalog, &creatable).is_ok());
        let e = v1_creatable_check(&format!("NVIDIA RTX A4000,{mig}"), &catalog, &creatable).unwrap_err().to_string();
        assert!(
            e.contains("`PRO 6000 MIG 24GB`") && e.contains("REST v1's create API rejects it")
                && e.contains("v2's create takes it: remove RUNPOD_API=v1 (v2 is the default), or pick another --gpu"),
            "{e}"
        );
        assert!(!e.contains("`RTX A4000`"), "only the bad token: {e}");
        assert!(v1_creatable_check(mig, &catalog, &[]).is_ok(), "no enum: nothing refused");
    }

    #[test]
    fn gpu_flag_policy_checks_with_a_catalog_and_warns_without_one() {
        let catalog = || {
            Ok(vec![
                GpuType { id: "NVIDIA GeForce RTX 3070".into(), display_name: "RTX 3070".into(), memory_gb: 8, ..Default::default() },
                GpuType { id: "NVIDIA RTX A4000".into(), display_name: "RTX A4000".into(), memory_gb: 16, ..Default::default() },
            ])
        };
        // Catalog reachable: canonical ids, no warning.
        let (gpu, warn) = gpu_flag_or_warning("3070,a4000".into(), catalog(), &[]).unwrap();
        assert_eq!((gpu.as_str(), warn), ("NVIDIA GeForce RTX 3070,NVIDIA RTX A4000", Vec::<String>::new()));
        // …and a typo fails loudly, with a suggestion.
        let e = gpu_flag_or_warning("3070x".into(), catalog(), &[]).unwrap_err().to_string();
        assert!(e.contains("`3070x` isn't a RunPod GPU type") && e.contains("did you mean `NVIDIA GeForce RTX 3070`"), "{e}");
        // A type the catalog lacks that arena has seen (its own list, a pod, the config): taken,
        // with a warning each.
        let (gpu, warn) = gpu_flag_or_warning("B300 MIG,NVIDIA X9".into(), catalog(), &["NVIDIA X9".into()]).unwrap();
        assert_eq!(gpu, "NVIDIA B300 SXM6 AC MIG 1g.34gb,NVIDIA X9");
        assert_eq!(warn, [
            "warning: --gpu `NVIDIA B300 SXM6 AC MIG 1g.34gb` isn't in RunPod's public catalog — trying it; RunPod's create decides",
            "warning: --gpu `NVIDIA X9` isn't in RunPod's public catalog — trying it; RunPod's create decides",
        ]);
        // Unreachable: passed through as typed, with one warning saying why.
        let (gpu, warn) = gpu_flag_or_warning("3070x".into(), Err("GraphQL catalog: timed out after 30s".into()), &[]).unwrap();
        assert_eq!(gpu, "3070x");
        assert!(warn.len() == 1 && warn[0].starts_with("warning: couldn't check --gpu `3070x`") && warn[0].contains("timed out"), "{warn:?}");
    }

    /// Non-RunPod providers keep pass-through (no catalog fetched at all), and so does a
    /// RunPod config without a key (the warning path; no network in either case).
    #[tokio::test]
    async fn checked_gpu_passes_through_off_runpod_and_without_a_key() {
        let c = cfg("");
        assert_eq!(super::checked_gpu(&c, "vast", Some("RTX 3070 typo".into())).await.unwrap().as_deref(), Some("RTX 3070 typo"));
        assert_eq!(super::checked_gpu(&c, "hetzner", None).await.unwrap(), None);
        // Shadeform: every token must name one of its GPU types (a typo there is an empty
        // catalog, i.e. "no stock" forever) — checked offline, no key needed.
        assert_eq!(super::checked_gpu(&c, "shadeform", Some("A6000,l40s".into())).await.unwrap().as_deref(), Some("A6000,l40s"));
        let e = super::checked_gpu(&c, "shadeform", Some("A6000,3090".into())).await.unwrap_err().to_string();
        assert!(e.contains("no Shadeform GPU type"), "{e}");
        assert_eq!(super::checked_gpu(&c, "runpod", Some("3070".into())).await.unwrap().as_deref(), Some("3070"));
        let v2 = cfg("RUNPOD_API=v2\n");
        assert_eq!(super::live_gpu_catalog(&v2).await.unwrap_err(), "no RUNPOD_API_KEY");
        assert_eq!(super::checked_gpu(&v2, "runpod", Some("3070".into())).await.unwrap().as_deref(), Some("3070"));
    }

    /// The tier whose v2 catalog serves the `--gpu` check and `arena offers runpod`: the configured
    /// one (any spelling) — so a plain create's price book reuses that fetch — else COMMUNITY.
    #[test]
    fn v2_catalog_tier_follows_the_configured_cloud() {
        assert_eq!(super::v2_default_cloud(&cfg("")), "COMMUNITY");
        assert_eq!(super::v2_default_cloud(&cfg("CLOUD_TYPE=secure\n")), "SECURE");
        assert_eq!(super::v2_default_cloud(&cfg("RUNPOD_CLOUD_TYPE=COMMUNITY\n")), "COMMUNITY");
        assert_eq!(super::v2_default_cloud(&cfg("CLOUD_TYPE=ALL\n")), "COMMUNITY");
    }

    /// Review G4 (item 17): `up` with ONE GPU (config `GPU_TYPE`, or a single `--gpu` — the
    /// common path, `placement_request` → `None`) labels its option through
    /// `single_option_plan`, with an unpriced book: no catalog, so a MIG slice keeps its id in
    /// the READY line and placement text (`READY … — 1×B300 SXM6 AC MIG 1g.34gb SECURE`, live
    /// BPF S6) while `pods list` says `1×B300 MIG 34GB` — docs/REFERENCE.md claims they now match.
    #[test]
    fn review_g4_single_spec_up_labels_a_mig_slice_as_pods_list_does() {
        let spec = PodSpec {
            gpu_type: "NVIDIA B300 SXM6 AC MIG 1g.34gb".into(),
            gpu_count: 1,
            cloud_type: "SECURE".into(),
            ..PodSpec::from_config(&cfg(""))
        };
        let plan = super::single_option_plan("runpod", &spec);
        assert_eq!(plan.options[0].label, "1×B300 MIG 34GB");
    }

    /// Live G #17: on the one-GPU path the dry run and the confirm describe the create with
    /// `provider.describe` — the MIG slice by its catalog id. They now name it as `pods list`,
    /// READY and the summary do; every other GPU's description is the provider's, unchanged.
    #[test]
    fn the_one_gpu_dry_run_and_confirm_name_a_mig_slice_as_pods_list_does() {
        let provider = arena_core::provider::runpod::RunpodProvider::new("k");
        let spec = |gpu: &str| PodSpec { gpu_type: gpu.into(), gpu_count: 1, cloud_type: "SECURE".into(), ..PodSpec::from_config(&cfg("")) };
        let mig = super::describe_create(&provider, &spec("NVIDIA B300 SXM6 AC MIG 1g.34gb"));
        assert!(mig.starts_with("1×B300 MIG 34GB, SECURE, disk "), "{mig}");
        assert!(!mig.contains("SXM6"), "{mig}");
        let a4000 = spec("NVIDIA RTX A4000");
        assert_eq!(super::describe_create(&provider, &a4000), arena_core::Provider::describe(&provider, &a4000));
    }

    /// Live G: the one-GPU path's `[created]` line named no GPU; it now says what the pod was
    /// created on, and its price when the provider reported one — as the placement path's does.
    #[test]
    fn the_one_gpu_created_line_names_the_gpu_and_price() {
        let pod = |cost: Option<f64>| arena_core::Pod {
            id: "qd3hgsswhhmk1b".into(),
            name: "devtest-delta".into(),
            provider: "runpod".into(),
            cost_per_hr: cost,
            ..Default::default()
        };
        let spec = PodSpec { gpu_type: "NVIDIA GeForce RTX 3070".into(), gpu_count: 1, cloud_type: "COMMUNITY".into(), ..PodSpec::from_config(&cfg("")) };
        let on = super::single_option_plan("runpod", &spec).options[0].describe();
        assert_eq!(
            super::created_line(&pod(Some(0.13)), Some(&on)),
            "[created] devtest-delta id=qd3hgsswhhmk1b on 1×RTX 3070 COMMUNITY ($0.13/h)"
        );
        assert_eq!(super::created_line(&pod(None), Some(&on)), "[created] devtest-delta id=qd3hgsswhhmk1b on 1×RTX 3070 COMMUNITY");
        assert_eq!(super::created_line(&pod(None), None), "[created] devtest-delta id=qd3hgsswhhmk1b");
    }
}

/// Scenario tests for target selection (PLAN 1.C) at the command layer, driven by a fake
/// `Provider` that records every mutation (no real API/SSH): the shared selector picks
/// exactly the pods each command family acts on, typos fail before anything is touched,
/// commands that change pods demand names or `--all`, and every old spelling still parses.
#[cfg(test)]
mod selection_tests {
    use super::lifecycle_tests::FakeKeys;
    use super::{
        handle_keys, handle_pods, keys_preview, keys_targets, keys_with, misplaced_run_flags, pod_name_as_command, select, Cli, Cmd, KeysAction,
        KeysCmd, PodCmd, Unscoped,
    };
    use arena_core::remote::FakeRemote;
    use arena_core::selector::SelectArgs;
    use arena_core::{Config, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use clap::Parser;
    use std::sync::{Arc, Mutex};

    /// A fleet that records `(op, id)` for every mutation, and counts list calls.
    struct FakeProvider {
        pods: Vec<Pod>,
        calls: Mutex<Vec<(&'static str, String)>>,
        lists: Mutex<usize>,
        /// GPU counts `enrich` fills in (by pod name) — the details a bare list omits.
        enriched: Vec<(&'static str, u32)>,
    }

    impl FakeProvider {
        fn calls(&self) -> Vec<(&'static str, String)> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, op: &'static str, id: &str) {
            self.calls.lock().unwrap().push((op, id.to_string()));
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            *self.lists.lock().unwrap() += 1;
            Ok(self.pods.clone())
        }
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            for p in pods.iter_mut() {
                if let Some((_, n)) = self.enriched.iter().find(|(name, _)| *name == p.name) {
                    p.gpu_count = Some(*n);
                }
            }
            Ok(())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised by selection tests")
        }
        async fn stop_pod(&self, id: &str) -> Result<()> {
            self.record("stop", id);
            Ok(())
        }
        async fn restart_pod(&self, id: &str) -> Result<()> {
            self.record("restart", id);
            Ok(())
        }
        async fn terminate_pod(&self, id: &str) -> Result<()> {
            self.record("terminate", id);
            Ok(())
        }
        async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
            let mut spec = PodSpec::from_config(&Config::parse(""));
            spec.env = vec![("KEEP".into(), "1".into())];
            Ok(spec)
        }
        async fn reimage_pod(&self, id: &str, _image: &str, _env: &[(String, String)]) -> Result<()> {
            self.record("reimage", id);
            Ok(())
        }
        fn restart_wipes_container_disk(&self, pod: &Pod) -> bool {
            pod.provider != "hetzner" // as the fleet provider routes it
        }
        // REVIEW sf SF3: as the fleet routes it — a Shadeform pod has no stop (no pod in the
        // existing fixtures is on Shadeform, so their tests are unchanged).
        fn stop_support(&self, pod: &Pod) -> Result<()> {
            if pod.provider == "shadeform" {
                Err(arena_core::Error::NotImplemented(arena_core::provider::shadeform::NO_STOP.into()))
            } else {
                Ok(())
            }
        }
    }

    fn pod(name: &str, status: &str, provider: &str, gpus: Option<u32>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.to_string(),
            provider: provider.into(),
            status: status.to_string(),
            gpu_count: gpus,
            ..Default::default()
        }
    }

    /// apple (1 GPU), bloom (2), cloud (count unknown until enriched), zebra (stopped), and
    /// a hetzner VM — listed out of order.
    fn fleet() -> FakeProvider {
        FakeProvider {
            pods: vec![
                pod("arena8-zebra", "EXITED", "runpod", Some(1)),
                pod("arena8-bloom", "RUNNING", "runpod", Some(2)),
                pod("arena8-apple", "RUNNING", "runpod", Some(1)),
                pod("arena8-cloud", "RUNNING", "runpod", None),
                pod("arena8-vm", "running", "hetzner", None),
            ],
            calls: Mutex::new(Vec::new()),
            lists: Mutex::new(0),
            enriched: vec![("arena8-cloud", 2)],
        }
    }

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(apple bloom cloud mayor vm zebra)\n")
    }

    fn args(targets: &[&str]) -> SelectArgs {
        SelectArgs::exact(targets.iter().copied())
    }

    async fn names(f: &FakeProvider, a: &SelectArgs) -> std::result::Result<Vec<String>, String> {
        select(f, &cfg(), a, Unscoped::All)
            .await
            .map(|s| s.pods.into_iter().map(|p| p.name).collect())
            .map_err(|e| e.to_string())
    }

    fn parse(argv: &[&str]) -> std::result::Result<Cmd, clap::Error> {
        Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd)
    }

    fn pods_cmd(argv: &[&str]) -> PodCmd {
        match parse(&[&["pods"][..], argv].concat()).unwrap_or_else(|e| panic!("{argv:?}: {e}")) {
            Cmd::Pods(p) => p,
            _ => unreachable!(),
        }
    }

    /// Run `pods <argv…>` against a fresh fleet; the recorded mutations, or the error.
    async fn run_pods(argv: &[&str]) -> (FakeProvider, std::result::Result<(), String>) {
        let f = fleet();
        let r = handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.map_err(|e| e.to_string());
        (f, r)
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("id-arena8-{n}")).collect()
    }

    /// The command lines the dashboard runs as children (arena-tui's `runs.rs`: its menu's
    /// start/rename/jobs/logs, the add-pod form's `up` and `offers`), which it builds without
    /// this parser, parse as it means them: the pod a target and never a JOB (review ai-review
    /// codex #3), `--yes` global, every flag of the form's there.
    #[test]
    fn the_dashboards_command_lines_parse_as_it_means_them() {
        let full = |argv: &[&str]| {
            Cli::try_parse_from(["arena", "--config", "/c.env", "--provider", "hetzner"].iter().chain(argv).copied()).unwrap_or_else(|e| panic!("{argv:?}: {e}"))
        };
        let pod = vec!["devtest-alpha".to_string()];
        let Cmd::Pods(PodCmd::Logs { tail, follow, sel }) = full(&["pods", "logs", "devtest-alpha", "--tail", "100"]).cmd else { panic!() };
        assert_eq!((tail, follow, arena_core::jobs::split_job_arg(&sel.targets).unwrap()), (100, false, (pod.clone(), None)));
        let Cmd::Pods(PodCmd::Jobs { kill, sel }) = full(&["pods", "jobs", "devtest-alpha"]).cmd else { panic!() };
        assert_eq!((kill, arena_core::jobs::split_job_arg(&sel.targets).unwrap()), (None, (pod.clone(), None)));
        let c = full(&["pods", "start", "devtest-alpha", "--yes"]);
        assert!(c.yes && matches!(&c.cmd, Cmd::Pods(PodCmd::Start { sel, no_setup: false, dry_run: false, .. }) if sel.targets == pod));
        let c = full(&["pods", "rename", "devtest-alpha", "bravo", "--yes"]);
        let Cmd::Pods(PodCmd::Rename { target, new_name, dry_run: false, .. }) = c.cmd else { panic!() };
        assert!(c.yes && target.as_deref() == Some("devtest-alpha") && new_name.as_deref() == Some("bravo"));
        let c = full(&["pods", "up", "--add", "2", "--server-type", "cx23", "--yes"]);
        assert!(c.yes && matches!(&c.cmd, Cmd::Pods(PodCmd::Up { add: Some(2), server_type: Some(t), dry_run: false, .. }) if t == "cx23"));
        let up = ["pods", "up", "--add", "1", "--gpu", "NVIDIA RTX A4000", "--gpus", "2", "--cloud", "COMMUNITY,SECURE", "--disk", "100", "--volume", "0", "--check", "--yes"];
        let Cmd::Pods(PodCmd::Up { gpu, gpus, cloud, disk, volume, check, retry_mins: 0, keep_trying: false, .. }) = full(&up).cmd else { panic!() };
        assert_eq!((gpu.as_deref(), gpus, cloud.as_deref(), disk, volume, check), (Some("NVIDIA RTX A4000"), Some(2), Some("COMMUNITY,SECURE"), Some(100), Some(0), true));
        // The form's retry field: `--retry-mins N` before `--yes`, on any provider; rounds a
        // minute apart (`--retry-secs`' default), never `--keep-trying`.
        for mins in ["10", "30", "120", "1440"] {
            let c = full(&["pods", "up", "--add", "1", "--server-type", "cx23", "--retry-mins", mins, "--yes"]);
            let Cmd::Pods(PodCmd::Up { retry_mins, retry_secs: 60, keep_trying: false, .. }) = c.cmd else { panic!("{mins}") };
            assert!(c.yes && retry_mins.to_string() == mins, "{mins}");
            let gpu = ["pods", "up", "--add", "2", "--gpu", "NVIDIA RTX A4000", "--gpus", "1", "--cloud", "COMMUNITY", "--disk", "100", "--volume", "0", "--retry-mins", mins, "--yes"];
            let Cmd::Pods(PodCmd::Up { retry_mins, add: Some(2), .. }) = full(&gpu).cmd else { panic!("{mins}") };
            assert_eq!(retry_mins.to_string(), mins);
        }
        let Cmd::Offers { gpu, cloud, gpus, server_type, json: true, .. } = full(&["offers", "--json", "--gpu", "NVIDIA RTX A4000", "--cloud", "SECURE", "--gpus", "1"]).cmd else { panic!() };
        assert_eq!((gpu.as_deref(), cloud.as_deref(), gpus, server_type), (Some("NVIDIA RTX A4000"), Some("SECURE"), Some(1), None));
        let Cmd::Offers { server_type, json: true, .. } = full(&["offers", "--json", "--server-type", "cx33"]).cmd else { panic!() };
        assert_eq!(server_type.as_deref(), Some("cx33"));
        // The type field's catalogue: `offers <provider> --json`.
        for p in ["runpod", "vast", "hetzner"] {
            let Cmd::Offers { view, json: true, .. } = full(&["offers", p, "--json"]).cmd else { panic!() };
            assert_eq!(view.as_deref(), Some(p));
        }
    }

    #[tokio::test]
    async fn the_resolver_picks_in_list_order_and_fails_loudly_on_typos() {
        let f = fleet();
        assert_eq!(names(&f, &SelectArgs::default()).await.unwrap(), ["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-vm", "arena8-zebra"]);
        assert_eq!(names(&f, &args(&["bloom", "arena8-apple"])).await.unwrap(), ["arena8-apple", "arena8-bloom"]);
        assert_eq!(names(&f, &args(&["apple..cloud"])).await.unwrap(), ["arena8-apple", "arena8-bloom", "arena8-cloud"]);
        assert_eq!(names(&f, &SelectArgs { on: Some("hetzner".into()), ..Default::default() }).await.unwrap(), ["arena8-vm"]);
        // The regression guard, now for every token: a typo (include OR exclude) is an
        // error naming it and the closest pod — never a quietly smaller (or larger) set.
        let e = names(&f, &args(&["arena8-zebrra"])).await.unwrap_err();
        assert!(e.contains("`arena8-zebrra`: no pod has that name or id — did you mean arena8-zebra?"), "{e}");
        let e = names(&f, &SelectArgs { exclude: vec!["aple".into()], ..Default::default() }).await.unwrap_err();
        assert!(e.contains("--exclude `aple`: no pod has that name or id — did you mean arena8-apple?"), "{e}");
        // A list name with no pod, and a narrowed-to-nothing selection, are errors too.
        assert!(names(&f, &args(&["mayor"])).await.unwrap_err().contains("no pod named arena8-mayor right now"));
        let e = names(&f, &SelectArgs { on: Some("vast".into()), ..Default::default() }).await.unwrap_err();
        assert!(e.starts_with("nothing selected: all pods → 5, --on vast → 0"), "{e}");
    }

    #[tokio::test]
    async fn gpus_fetches_the_details_a_listing_omits() {
        // cloud's list entry has no GPU count; `--gpus` enriches first, so it's found.
        let f = fleet();
        let two = SelectArgs { gpus: Some(2), ..Default::default() };
        assert_eq!(names(&f, &two).await.unwrap(), ["arena8-bloom", "arena8-cloud"]);
        // Still unknown after enriching (the VM): never matches.
        let none = SelectArgs { gpus: Some(0), on: Some("hetzner".into()), ..Default::default() };
        assert!(names(&f, &none).await.unwrap_err().contains("(1 with an unknown GPU count)"));
    }

    #[tokio::test]
    async fn a_command_that_changes_pods_needs_names_or_all_before_listing() {
        let f = fleet();
        for a in [SelectArgs::default(), SelectArgs { gpus: Some(1), ..Default::default() }] {
            let e = select(&f, &cfg(), &a, Unscoped::Refuse("stop")).await.unwrap_err().to_string();
            assert_eq!(e, "name the pods to stop (names, ids or a range like apple..mayor), or pass --all");
        }
        // A malformed range fails before listing too.
        assert!(select(&f, &cfg(), &args(&["cloud..apple"]), Unscoped::All).await.is_err());
        assert_eq!(*f.lists.lock().unwrap(), 0, "nothing was listed");
    }

    /// REVIEW sf SF3 (money): one Shadeform pod anywhere in a `pods stop --all` selection makes
    /// the whole stop bail up front (`stop_refusal` over `sel.pods`) — so the routine nightly
    /// `pods stop --all --wipe-ok` (balance.rs counts on it) stops *nothing*: every RunPod pod
    /// keeps billing all night because an overflow VM exists. The refusal also counts pods that
    /// aren't stop targets at all: a Shadeform instance already `deleting` (listed TERMINATED,
    /// billing nothing) blocks the batch the same way, unlike every other non-billing pod,
    /// which is "skip … — not running". A pod that can't be stopped must be skipped and said
    /// (and may fail the exit), not veto the rest of `--all`.
    #[tokio::test]
    async fn review_sf_sf3_a_shadeform_pod_does_not_veto_stop_all() {
        let mut vetoed = Vec::new();
        for status in ["RUNNING", "TERMINATED"] {
            let mut f = fleet();
            f.pods.push(pod("arena8-mayor", status, "shadeform", Some(1)));
            let r = handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true)
                .await
                .map_err(|e| e.to_string());
            let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
            let want: Vec<String> = ids(&["apple", "bloom", "cloud", "vm"]).into_iter().map(|id| format!("stop {id}")).collect();
            if got != want {
                vetoed.push(format!("Shadeform pod {status}: stopped {got:?}, not {want:?} ({r:?})"));
            }
        }
        assert!(vetoed.is_empty(), "a Shadeform pod vetoed `pods stop --all`:\n{}", vetoed.join("\n"));
    }

    /// SF3, the rest of it: `stop --all` with a billing Shadeform pod stops every other pod, says
    /// the Shadeform one is skipped, and exits non-zero (it still bills); an explicit stop of
    /// only that pod is refused with nothing stopped; a `deleting` one (TERMINATED) is just "not
    /// running" — exit 0.
    #[tokio::test]
    async fn sf3_stop_skips_shadeform_pods_exits_non_zero_and_refuses_them_alone() {
        let mut f = fleet();
        f.pods.push(pod("arena8-mayor", "RUNNING", "shadeform", Some(1)));
        let r = handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await;
        let e = r.unwrap_err().to_string();
        assert!(e.contains("1 pod(s) still billing") && e.contains("arena8-mayor") && e.contains("no stop"), "{e}");
        assert_eq!(f.calls().len(), 4, "every other running pod stopped: {:?}", f.calls());
        // Only the Shadeform pod: refused, nothing stopped.
        let mut f = fleet();
        f.pods.push(pod("arena8-mayor", "RUNNING", "shadeform", Some(1)));
        let e = handle_pods(pods_cmd(&["stop", "mayor"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
        assert!(e.starts_with("can't stop arena8-mayor — Shadeform has no stop"), "{e}");
        assert!(f.calls().is_empty());
        // Already deleting: not a stop target — the rest stop and the exit is 0.
        let mut f = fleet();
        f.pods.push(pod("arena8-mayor", "TERMINATED", "shadeform", Some(1)));
        handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap();
        assert_eq!(f.calls().len(), 4);
        // `start` of only the Shadeform pod: refused the same way, nothing sent.
        let e = handle_pods(pods_cmd(&["start", "mayor"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
        assert!(e.starts_with("can't start arena8-mayor — Shadeform has no stop"), "{e}");
    }

    #[tokio::test]
    async fn stop_stops_exactly_the_selected_running_pods() {
        // The fleet's RunPod pods have no volume (the fake's spec), so stopping them needs
        // --wipe-ok (a stopped RunPod pod keeps no data); the Hetzner VM keeps its disk.
        let cases: &[(&[&str], &[&str])] = &[
            (&["stop", "bloom", "apple", "--wipe-ok"], &["apple", "bloom"]),
            (&["stop", "apple..cloud", "--exclude", "bloom", "--wipe-ok"], &["apple", "cloud"]),
            // zebra is EXITED: skipped (said, not stopped); the VM is `running` (any case).
            (&["stop", "--all", "--wipe-ok"], &["apple", "bloom", "cloud", "vm"]),
            (&["stop", "--all", "--on", "runpod", "--gpus", "2", "--wipe-ok"], &["bloom", "cloud"]),
            // the old spellings: one positional; `--all` narrowed by --include/--exclude
            (&["stop", "arena8-apple", "--wipe-ok"], &["apple"]),
            (&["stop", "--all", "--include", "bloom", "--include", "cloud", "--exclude", "cloud", "--wipe-ok"], &["bloom"]),
            // a dry run stops nothing (and needs no --wipe-ok to preview)
            (&["stop", "--all", "--dry-run"], &[]),
            // a VM keeps its disk: no --wipe-ok needed
            (&["stop", "vm"], &["vm"]),
        ];
        for (argv, stopped) in cases {
            let (f, r) = run_pods(argv).await;
            r.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
            let want: Vec<String> = ids(stopped).into_iter().map(|id| format!("stop {id}")).collect();
            assert_eq!(got, want, "{argv:?}");
        }
        for (argv, err) in [
            (&["stop"][..], "name the pods to stop"),
            (&["stop", "--exclude", "apple"], "name the pods to stop"),
            (&["stop", "bloon"], "did you mean arena8-bloom?"),
            (&["stop", "--all", "--exclude", "bloon"], "--exclude `bloon`"),
            (&["stop", "apple", "--all"], "pass target names or --all, not both"),
            // a volume-less RunPod pod loses everything: refused without --wipe-ok, even
            // with --yes (run_pods passes yes) — and nothing at all is stopped, VM included
            (&["stop", "apple", "vm"], "refusing to stop arena8-apple without --wipe-ok"),
            (&["stop", "--all"], "refusing to stop arena8-apple, arena8-bloom, arena8-cloud without --wipe-ok"),
        ] {
            let (f, r) = run_pods(argv).await;
            let e = r.unwrap_err();
            assert!(e.contains(err), "{argv:?}: {e}");
            assert!(f.calls().is_empty(), "{argv:?}");
        }
    }

    #[tokio::test]
    async fn reimage_reimages_exactly_the_selected_pods() {
        // reimage refuses without a readable public key, so give it one.
        let dir = std::env::temp_dir().join(format!("arena-sel-reimage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k"), "private").unwrap();
        std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest arena-test").unwrap();
        let cfg = Config::parse(&format!(
            "{}SHARED_SSH_KEY_PATH={}\nIMAGE=img:1\n",
            "MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(apple bloom cloud mayor vm zebra)\n",
            dir.join("k").display()
        ));
        let cases: &[(&[&str], &[&str])] = &[
            (&["reimage", "bloom", "zebra", "--skip-proxy", "--wipe-ok", "--no-setup"], &["bloom", "zebra"]),
            (&["reimage", "--all", "--exclude", "apple..cloud", "--skip-proxy", "--wipe-ok", "--no-setup"], &["vm", "zebra"]),
            (&["reimage", "apple..bloom", "--dry-run", "--no-setup"], &[]),
        ];
        for (argv, reimaged) in cases {
            let f = fleet();
            handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true)
                .await
                .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
            let want: Vec<String> = ids(reimaged).into_iter().map(|id| format!("reimage {id}")).collect();
            assert_eq!(got, want, "{argv:?}");
        }
        for argv in [&["reimage"][..], &["reimage", "--gpus", "1"], &["reimage", "appel"]] {
            let f = fleet();
            assert!(handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true).await.is_err(), "{argv:?}");
            assert!(f.calls().is_empty(), "{argv:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn terminate_takes_one_pod_by_the_same_matcher() {
        let (f, r) = run_pods(&["terminate", "bloom", "--skip-proxy"]).await;
        r.unwrap();
        assert_eq!(f.calls(), [("terminate", "id-arena8-bloom".to_string())]);
        let (f, r) = run_pods(&["terminate", "id-arena8-apple", "--skip-proxy", "--dry-run"]).await;
        r.unwrap();
        assert!(f.calls().is_empty());
        // No ranges, no `all` token, no typos, no ambiguity — and nothing terminated.
        for (argv, err) in [
            (&["terminate", "apple..cloud"][..], "takes one pod — `apple..cloud` is a range"),
            (&["terminate", "all"], "`all` isn't accepted"),
            (&["terminate", "blom"], "did you mean arena8-bloom?"),
        ] {
            let (f, r) = run_pods(argv).await;
            assert!(r.unwrap_err().contains(err), "{argv:?}");
            assert!(f.calls().is_empty(), "{argv:?}");
        }
        let mut f = fleet();
        f.pods.push(Pod { id: "id-second-bloom".into(), ..f.pods[1].clone() });
        let e = handle_pods(pods_cmd(&["terminate", "bloom"]), &f, Arc::new(FakeRemote::new()), &cfg(), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("`bloom` matches 2 pods") && e.contains("id-second-bloom"), "{e}");
        assert!(f.calls().is_empty());
        // A target and --all together is refused by the parser (it used to mean "all").
        assert!(parse(&["pods", "terminate", "apple", "--all"]).is_err());
    }

    #[tokio::test]
    async fn keys_target_machine_names_with_or_without_a_pod() {
        let f = fleet();
        let keys = |a: SelectArgs, known: Vec<String>| {
            let f = &f;
            async move {
                keys_targets(f, &cfg(), &a, &known).await.map(|(n, live)| (n, live.pods.len())).map_err(|e| e.to_string())
            }
        };
        // A range spans list names without a pod (mayor): a key can precede its pod.
        assert_eq!(
            keys(args(&["cloud..vm"]), vec![]).await.unwrap(),
            (vec!["arena8-cloud".to_string(), "arena8-mayor".into(), "arena8-vm".into()], 2)
        );
        // A spare list name alone (revoke a terminated pod's key) is fine; a typo is not.
        assert_eq!(keys(args(&["mayor"]), vec![]).await.unwrap(), (vec!["arena8-mayor".to_string()], 0));
        assert!(keys(args(&["mayr"]), vec![]).await.unwrap_err().contains("did you mean arena8-mayor?"));
        // A name that left the list but still holds a key (or a CSV row) can still be
        // targeted; a near-miss of it is suggested.
        let known = vec!["arena8-old".to_string()];
        assert_eq!(keys(args(&["old"]), known.clone()).await.unwrap(), (vec!["arena8-old".to_string()], 0));
        assert!(keys(args(&["olf"]), known.clone()).await.unwrap_err().contains("did you mean arena8-old?"));
        assert!(keys(args(&["old"]), vec![]).await.unwrap_err().contains("no pod has that name"));
        // --all = every current pod (not the spare names or key holders), narrowed as usual.
        let all = SelectArgs { all: true, exclude: vec!["vm".into()], ..Default::default() };
        assert_eq!(keys(all, known).await.unwrap().0, ["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-zebra"]);
        assert!(keys(SelectArgs::default(), vec![]).await.unwrap_err().contains("name the machines"));
        // --gpus fetches the details a listing omits, as `select` does (cloud's count).
        let two = SelectArgs { all: true, gpus: Some(2), ..Default::default() };
        assert_eq!(keys(two, vec![]).await.unwrap().0, ["arena8-bloom", "arena8-cloud"]);
        // Without a provisioning key the command stops before anything (no network).
        let Cmd::Keys(k) = parse(&["keys", "gen", "apple", "--dry-run"]).unwrap() else { unreachable!() };
        let e = handle_keys(k, &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
        assert!(e.contains("set OPENROUTER_PROVISIONING_KEY first"), "{e}");
    }

    /// A temp keys dir (per process + test), removed when dropped.
    struct KeysDir(std::path::PathBuf);
    impl Drop for KeysDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn keys_dir(tag: &str) -> KeysDir {
        let d = std::env::temp_dir().join(format!("arena-keys-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        KeysDir(d)
    }

    /// `keys <argv…>` against the fleet, `api` and `dir`.
    async fn keys_cmd(argv: &[&str], api: &FakeKeys, dir: &std::path::Path) -> std::result::Result<(), String> {
        let Cmd::Keys(k) = parse(&[&["keys"][..], argv].concat()).unwrap_or_else(|e| panic!("{argv:?}: {e}")) else {
            unreachable!()
        };
        keys_with(k, &fleet(), Arc::new(FakeRemote::new()), &cfg(), api, dir, true).await.map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn keys_gen_rotate_revoke_act_on_exactly_the_resolved_machines() {
        let dir = keys_dir("act");
        // bloom already has a key; an old cohort's key and an unrelated one must be ignored.
        let api = FakeKeys::with(&[("h-b", "arena8-bloom"), ("h-7", "arena7-apple"), ("h-x", "admin")]);
        // gen: a range + a spare list name; bloom is skipped (exists), the rest minted.
        keys_cmd(&["gen", "apple..cloud", "mayor", "--exclude", "cloud"], &api, &dir.0).await.unwrap();
        assert_eq!(api.calls(), ["create arena8-apple", "create arena8-mayor"]);
        let csv = std::fs::read_to_string(dir.0.join("openrouter_api_keys.csv")).unwrap();
        let rows: Vec<String> = arena_core::apikeys::parse_csv(&csv).into_iter().map(|(h, _)| h).collect();
        assert_eq!(rows, ["arena8-apple", "arena8-mayor"]);
        assert_eq!(std::fs::read_link(dir.0.join("openrouter_api_keys.csv")).unwrap(), std::path::PathBuf::from("arena8_openrouter_keys.csv"));
        // rotate: deletes each machine's key(s), mints fresh — only for the --on selection.
        api.calls.lock().unwrap().clear();
        keys_cmd(&["rotate", "--all", "--on", "runpod", "--gpus", "2"], &api, &dir.0).await.unwrap();
        assert_eq!(api.calls(), ["delete h-b", "create arena8-bloom", "create arena8-cloud"]);
        // revoke: exactly the named machine's key; the CSV is left as it was.
        api.calls.lock().unwrap().clear();
        keys_cmd(&["revoke", "mayor"], &api, &dir.0).await.unwrap();
        assert_eq!(api.calls(), ["delete h-new-arena8-mayor"]);
        assert!(std::fs::read_to_string(dir.0.join("openrouter_api_keys.csv")).unwrap().contains("arena8-mayor,"));
        // A machine that left the list and has no pod, but still has a key: revocable by
        // name (it's a key holder); a typo of it is still an error naming it.
        let api = FakeKeys::with(&[("h-o", "arena8-oldbox")]);
        keys_cmd(&["revoke", "oldbox"], &api, &dir.0).await.unwrap();
        assert_eq!(api.calls(), ["delete h-o"]);
        let e = keys_cmd(&["revoke", "oldbx"], &FakeKeys::with(&[("h-o", "arena8-oldbox")]), &dir.0).await.unwrap_err();
        assert!(e.contains("did you mean arena8-oldbox?"), "{e}");
        // Dry runs change nothing on either side.
        let api = FakeKeys::with(&[("h-b", "arena8-bloom")]);
        for argv in [&["gen", "--all", "--dry-run"][..], &["rotate", "apple", "--dry-run"], &["revoke", "--all", "--dry-run"]] {
            keys_cmd(argv, &api, &dir.0).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        assert!(api.calls().is_empty());
    }

    #[test]
    fn keys_dry_run_names_every_resolved_machine_and_what_happens() {
        let rows = vec![("arena8-apple".to_string(), "arena8-apple".to_string(), 0), ("arena8-bloom".into(), "arena8-bloom".into(), 1)];
        assert_eq!(
            keys_preview(KeysAction::Gen, 5.0, &rows),
            [
                "Dry-run — would mint a $5.00-cap key for 2 machine(s):",
                "  arena8-apple",
                "  arena8-bloom — already exists, would skip (`arena keys rotate arena8-bloom` replaces it)",
            ]
        );
        assert_eq!(keys_preview(KeysAction::Rotate, 2.5, &rows)[1..], ["  arena8-apple — none yet, would just mint", "  arena8-bloom — deletes 1 existing"]);
        assert_eq!(keys_preview(KeysAction::Revoke, 5.0, &rows)[1..], ["  arena8-apple — no such key, nothing to revoke", "  arena8-bloom — deletes 1"]);
    }

    /// `pods stop --all` stops every billing pod — a STARTING/PROVISIONING v2 pod (it
    /// bills) used to be missed, as only `RUNNING` matched — and leaves stopped ones alone.
    /// The same status rule applies to a stop of named pods (the merged selector path).
    #[tokio::test]
    async fn stop_all_targets_every_billing_status() {
        let fleet = || FakeProvider {
            pods: vec![
                pod("arena8-apple", "RUNNING", "runpod", Some(1)),
                pod("arena8-bloom", "STARTING", "runpod", Some(1)),
                pod("arena8-cider", "PROVISIONING", "runpod", Some(1)),
                pod("arena8-dune", "EXITED", "runpod", Some(1)),
                pod("arena8-elm", "TERMINATED", "runpod", Some(1)),
                pod("arena8-fig", "ERROR", "runpod", Some(1)),
                pod("arena8-gale", "running", "runpod", Some(1)),
            ],
            calls: Mutex::new(Vec::new()),
            lists: Mutex::new(0),
            enriched: vec![],
        };
        // (argv, stopped) — the fake's RunPod pods have no volume, so a real stop needs
        // --wipe-ok (lane E's gate); a dry run previews without it.
        let cases: &[(&[&str], &[&str])] = &[
            (&["stop", "--all", "--dry-run"], &[]),
            (&["stop", "--all", "--wipe-ok"], &["apple", "bloom", "cider", "fig", "gale"]),
            // named: a starting pod is stopped; an exited one is skipped (said, not stopped)
            (&["stop", "bloom", "dune", "--wipe-ok"], &["bloom"]),
            (&["stop", "dune", "elm"], &[]),
        ];
        for (argv, stopped) in cases {
            let f = fleet();
            handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg(), true)
                .await
                .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
            let want: Vec<String> = ids(stopped).into_iter().map(|id| format!("stop {id}")).collect();
            assert_eq!(got, want, "{argv:?}");
        }
    }

    /// `pods stop --help` states the rule the handler applies (`status::is_billing`), not
    /// the old RUNNING-only one: an ERROR / PROVISIONING pod is stopped too, and needs
    /// --wipe-ok like a running one — the help must not promise otherwise. The one-line
    /// summary in `arena pods --help` warns that a stop wipes the container disk.
    #[test]
    fn stop_help_states_the_billing_rule() {
        use clap::CommandFactory;
        let cli = super::Cli::command();
        let stop = cli.find_subcommand("pods").and_then(|p| p.find_subcommand("stop")).expect("pods stop");
        let about = stop.get_about().map(|s| s.to_string()).unwrap_or_default();
        let long = stop.get_long_about().map(|s| s.to_string()).unwrap_or_default();
        assert!(about.contains("billing") && !about.contains("running pods") && about.contains("WIPES"), "{about}");
        for want in ["starting", "provisioning", "ERROR", "--wipe-ok", "skipped"] {
            assert!(long.contains(want), "missing {want:?}: {long}");
        }
    }

    /// Finding #37: `-n` counts only --provider's pods (`provider_pod_count`), so "TOTAL
    /// number of pods" read as a fleet total — `-n 4` on a mixed fleet would leave 5 — and the
    /// `proxy` group called itself read-only although `proxy apply` writes and reloads nginx.
    #[test]
    fn help_says_what_n_counts_and_that_proxy_apply_writes() {
        use clap::CommandFactory;
        let cli = super::Cli::command();
        let pods = cli.find_subcommand("pods").expect("pods");
        for sub in ["create", "up"] {
            let cmd = pods.find_subcommand(sub).expect(sub);
            let arg = |id: &str| {
                cmd.get_arguments().find(|a| a.get_id() == id).and_then(|a| a.get_help()).map(|h| h.to_string()).unwrap()
            };
            let n = arg("count");
            assert!(!n.contains("TOTAL") && n.contains("ON --provider") && n.contains("other providers don't count"), "{sub}: {n}");
            assert!(arg("add").contains("--provider"), "{sub}");
        }
        let proxy = cli.find_subcommand("proxy").expect("proxy");
        let about = proxy.get_about().map(|s| s.to_string()).unwrap_or_default();
        assert!(!about.contains("read-only") && about.contains("write + reload (apply)"), "{about}");
    }

    /// The help stays short enough to be read (the operator's complaint: one-line summaries
    /// that ran to 500 characters). Walks the whole command tree: every command has a one-line
    /// summary (`about`: its line in the parent's command list) of at most 70 characters,
    /// every argument has help whose first line is at most 100 and whose `--help` text is at
    /// most 300, and every command's `--help` text (`long_about`) is at most 800. Details
    /// belong in docs/REFERENCE.md / FEATURE_MAP.md, not here.
    #[test]
    fn help_text_stays_short() {
        use clap::CommandFactory;
        const ABOUT_MAX: usize = 70;
        const ARG_HELP_MAX: usize = 100;
        const ARG_LONG_HELP_MAX: usize = 300;
        const LONG_ABOUT_MAX: usize = 800;
        // Justified exceptions to LONG_ABOUT_MAX, by command path (`"arena pods up"`): none.
        const LONG_ABOUT_EXCEPTIONS: &[&str] = &[];
        fn walk(cmd: &clap::Command, path: String, bad: &mut Vec<String>) {
            let len = |s: &str| s.chars().count();
            let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
            if about.trim().is_empty() || about.contains('\n') || len(&about) > ABOUT_MAX {
                bad.push(format!("{path}: about, {} chars: {about:?}", len(&about)));
            }
            let long = cmd.get_long_about().map(|s| s.to_string()).unwrap_or_default();
            if len(&long) > LONG_ABOUT_MAX && !LONG_ABOUT_EXCEPTIONS.contains(&path.as_str()) {
                bad.push(format!("{path}: long help, {} chars", len(&long)));
            }
            for arg in cmd.get_arguments() {
                let id = arg.get_id();
                let help = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
                let first = help.lines().next().unwrap_or_default();
                if first.trim().is_empty() || len(first) > ARG_HELP_MAX {
                    bad.push(format!("{path} --{id}: help, {} chars: {help:?}", len(first)));
                }
                let long = arg.get_long_help().map(|s| s.to_string()).unwrap_or_default();
                if len(&long) > ARG_LONG_HELP_MAX {
                    bad.push(format!("{path} --{id}: long help, {} chars", len(&long)));
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, format!("{path} {}", sub.get_name()), bad);
            }
        }
        let mut cli = super::Cli::command();
        cli.build();
        let mut bad = Vec::new();
        walk(&cli, "arena".to_string(), &mut bad);
        assert!(bad.is_empty(), "help text missing or too long (details go in docs/REFERENCE.md):\n{}", bad.join("\n"));
    }

    /// Shortening the help never drops a warning: the commands and flags that wipe, delete or
    /// terminate still say so, in the words an operator scans for.
    #[test]
    fn dangerous_commands_keep_their_warnings() {
        use clap::CommandFactory;
        let cli = super::Cli::command();
        let find = |path: &[&str]| {
            path.iter().fold(&cli, |c, name| c.find_subcommand(name).unwrap_or_else(|| panic!("no command {path:?}")))
        };
        let text = |s: Option<&clap::builder::StyledStr>| s.map(|s| s.to_string()).unwrap_or_default();
        for (path, word) in [(&["pods", "stop"][..], "WIPES"), (&["pods", "restart"], "WIPES"), (&["pods", "reimage"], "WIPES")] {
            let about = text(find(path).get_about());
            assert!(about.contains(word), "{path:?}: {about}");
        }
        for (path, word) in [
            (&["pods", "terminate"][..], "IRREVERSIBLE"),
            (&["pods", "up"], "TERMINATED"),
            (&["pods", "reap"], "REAP_ENABLED=1"),
            (&["pods", "reap", "run"], "TERMINATE"),
            (&["archive", "down"], "EVERY BACKUP"),
            (&["teardown"], "deletes nothing"),
        ] {
            let long = text(find(path).get_long_about());
            assert!(long.contains(word), "{path:?}: {long}");
        }
        for (path, id, word) in [
            (&["pods", "terminate"][..], "all", "EVERY"),
            (&["pods", "stop"], "wipe_ok", "WIPED"),
            (&["pods", "restart"], "wipe_ok", "WIPED"),
            (&["pods", "set-branch"], "hard", "DESTRUCTIVE"),
            (&["pods", "setup"], "force", "DISCARDS"),
            (&["pods", "up"], "check", "TERMINATE"),
            (&["pods", "reap", "run"], "apply", "TERMINATE"),
            (&["archive", "down"], "delete_volumes", "DELETE"),
            (&["archive", "forget"], "delete_archived", "for good"),
            (&["pods", "migrate", "finish"], "discard_stopped", "losing"),
            (&["update"], "skip_attestation", "NOT check build provenance"),
        ] {
            let arg = find(path).get_arguments().find(|a| a.get_id() == id).unwrap_or_else(|| panic!("{path:?} {id}"));
            let help = text(arg.get_help());
            assert!(help.contains(word), "{path:?} --{id}: {help}");
        }
    }

    #[tokio::test]
    async fn a_selection_flag_after_runs_command_is_refused_not_widened() {
        // `run hostname -t apple` parses as the command `hostname -t apple` with NO targets
        // (= every pod): refused before anything is listed or run.
        for argv in [
            &["run", "hostname", "-t", "apple"][..],
            &["run", "-t", "apple", "hostname", "--exclude", "bloom"],
            &["run", "uptime", "--on=vast"],
            &["run", "rm", "-rf", "/tmp/x", "--dry-run"],
        ] {
            let (f, r) = run_pods(argv).await;
            let e = r.unwrap_err();
            assert!(e.contains("after the command would be part of the command") && e.contains("Nothing was run"), "{argv:?}: {e}");
            assert_eq!(*f.lists.lock().unwrap(), 0, "{argv:?}");
        }
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (cmd, bad) in [
            (&["hostname", "-t", "apple"][..], Some("`-t`")),
            (&["x", "--target=apple", "--gpus", "2"], Some("`--target=apple --gpus`")),
            (&["x", "--include", "a", "--dryrun"], Some("`--include --dryrun`")),
            (&["pytest", "-x", "--background"], Some("`--background`")),
            (&["pytest", "--bg"], Some("`--bg`")),
            (&["sleep", "--bgcolor"], None),
            // a quoted command is one word; flags that only look alike are left alone
            (&["tmux kill-session -t lab"], None),
            (&["tar", "-tvf", "x.tar"], None),
            (&["git", "branch", "--all"], None),
            (&["apt-get", "install", "-y", "jq"], None),
            (&["ls", "--only-dirs"], None),
        ] {
            let got = misplaced_run_flags(&words(cmd));
            match bad {
                None => assert_eq!(got, None, "{cmd:?}"),
                Some(b) => assert!(got.as_deref().is_some_and(|e| e.starts_with(b)), "{cmd:?}: {got:?}"),
            }
        }
    }

    #[test]
    fn run_refuses_a_command_that_starts_with_a_pods_name() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let names = [("devtest-hz4", "hz4"), ("devtest-apple", "apple")];
        for (cmd, bad) in [
            // the `-t` forgotten: the pod's name (full or short) taken as the command
            (&["devtest-hz4", "git status"][..], Some(("devtest-hz4", "devtest-hz4"))),
            (&["apple", "ls", "-la"], Some(("apple", "devtest-apple"))),
            (&["apple ls"], Some(("apple", "devtest-apple"))),
            // ordinary commands, a name inside a command, the `command` escape, other words
            (&["git", "status"], None),
            (&["ls apple"], None),
            (&["command", "apple", "x"], None),
            (&["apples"], None),
            (&["devtest-hz"], None),
        ] {
            let got = pod_name_as_command(&words(cmd), &names);
            match bad {
                None => assert_eq!(got, None, "{cmd:?}"),
                Some((first, full)) => {
                    let e = got.unwrap_or_else(|| panic!("{cmd:?}: not refused"));
                    assert!(e.starts_with(&format!("`{first}` is a pod's name ({full})")), "{cmd:?}: {e}");
                    assert!(e.contains("Nothing was run") && e.contains(&format!("-t {first} '")), "{cmd:?}: {e}");
                }
            }
        }
        assert_eq!(pod_name_as_command(&[], &names), None);
        assert!(pod_name_as_command(&words(&["apple"]), &names).is_some_and(|e| e.contains("on all 2 selected pod(s)")));
    }

    #[test]
    fn every_old_spelling_still_parses() {
        let ok: &[&[&str]] = &[
            &["pods", "stop", "apple"],
            &["pods", "stop", "--all", "--include", "apple", "--exclude", "bloom"],
            &["pods", "reimage", "apple", "bloom"],
            &["pods", "reimage", "--all", "--exclude", "apple"],
            &["pods", "terminate", "apple"],
            &["pods", "terminate", "--all"],
            &["pods", "setup"],
            &["pods", "setup", "bulk", "apple", "--dry-run"],
            &["pods", "copy-keys", "apple", "--include", "bloom", "--exclude", "cloud"],
            &["pods", "cp", "f.txt", "--include", "apple", "--exclude", "bloom"],
            &["pods", "cp", "f.txt", "/root/x/", "-r"],
            &["pods", "backup"],
            &["pods", "backup", "apple", "--no-pull"],
            &["pods", "set-branch", "main", "apple"],
            &["pods", "set-branch", "main", "--all", "--hard"],
            &["pods", "init-branches", "--week", "1", "--day", "2"],
            &["pods", "pull", "w1d2", "--dry-run"],
            &["pods", "run", "nvidia-smi", "-L"],
            &["pods", "test"],
            &["pods", "test", "--deep", "apple", "--json"],
            &["keys", "gen", "apple", "bloom", "--limit", "5"],
            &["keys", "gen", "--all", "--copy"],
            &["keys", "rotate", "apple"],
            &["keys", "rotate", "--all"],
            &["keys", "revoke", "apple"],
            &["keys", "revoke", "--all"],
            // and the new syntax
            &["pods", "run", "-t", "apple..mayor", "--exclude", "bloom", "--gpus", "2", "--on", "runpod", "nvidia-smi"],
            &["pods", "run", "--target", "apple,bloom", "--target", "cloud", "echo", "hi"],
            &["pods", "cp", "f.txt", "-t", "apple"],
            &["pods", "pull", "-t", "apple..bloom"],
            &["pods", "pull", "apple..bloom", "--exclude", "cloud", "--label", "w1d2"],
            &["pods", "pull", "bravo", "--dry-run"],
            &["pods", "test", "all", "--on", "vast"],
            &["pods", "init-branches", "apple", "bloom"],
            &["pods", "backup", "apple..cloud", "--exclude", "bloom"],
        ];
        for argv in ok {
            parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        // `run`'s targets are flags: everything after the command is the command.
        match parse(&["pods", "run", "-t", "apple", "echo", "-t", "x"]).unwrap() {
            Cmd::Pods(PodCmd::Run { sel, command, .. }) => {
                assert_eq!((sel.targets, command), (vec!["apple".to_string()], vec!["echo".into(), "-t".into(), "x".into()]));
            }
            _ => panic!("not pods run"),
        }
        // The legacy narrowing of --all by --include, and the parsed selection itself.
        let Cmd::Pods(PodCmd::Stop { sel, .. }) = parse(&["pods", "stop", "--all", "--include", "apple"]).unwrap() else {
            panic!("not pods stop")
        };
        assert_eq!(sel.args(), SelectArgs { targets: vec!["apple".into()], ..Default::default() });
        let Cmd::Keys(KeysCmd::Gen { sel, .. }) = parse(&["keys", "gen", "apple", "--on", "runpod"]).unwrap() else {
            panic!("not keys gen")
        };
        assert_eq!(sel.args(), SelectArgs { targets: vec!["apple".into()], on: Some("runpod".into()), ..Default::default() });
        for bad in [&["pods", "terminate", "apple", "--all"][..], &["pods", "run", "--gpus", "two", "x"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}

#[cfg(test)]
mod tests {

    /// `pods stop`/`start` skip a Shadeform pod (it has no stop — it bills until deleted) and act on
    /// the rest; only a selection of nothing else is refused outright, and the stop's exit stays
    /// non-zero while a skipped one still bills (review SF3).
    #[test]
    fn stop_and_start_skip_a_shadeform_pod_and_refuse_only_it_alone() {
        use super::Config;
        let fleet = arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k"), false).unwrap();
        let sf = arena_core::Pod { name: "devtest-alpha".into(), provider: "shadeform".into(), ..Default::default() };
        let rp = arena_core::Pod { name: "devtest-bloom".into(), provider: "runpod".into(), ..Default::default() };
        let (no_stop, rest) = super::split_no_stop(fleet.as_ref(), vec![&rp, &sf]);
        assert_eq!((super::names_of(&no_stop), rest.iter().map(|p| p.name.as_str()).collect::<Vec<_>>()), (vec!["devtest-alpha".to_string()], vec!["devtest-bloom"]));
        assert!(no_stop[0].1.starts_with("Shadeform has no stop") && no_stop[0].1.contains("arena pods terminate"), "{}", no_stop[0].1);
        for what in ["stop", "start"] {
            let r = super::no_stop_refusal(&no_stop, what);
            assert!(r.starts_with(&format!("can't {what} devtest-alpha — Shadeform has no stop")), "{r}");
        }
        let left = super::no_stop_left(&no_stop).unwrap();
        assert!(left.starts_with("1 pod(s) still billing") && left.contains("devtest-alpha"), "{left}");
        assert_eq!(super::no_stop_left(&[]), None);
        let (none, all) = super::split_no_stop(fleet.as_ref(), vec![&rp]);
        assert!(none.is_empty() && all.len() == 1);
    }

    /// `update --check` exits 10 when a newer release exists, apart from up to date (0) and
    /// couldn't check (1); the version it compares is this build's, with its commit after it.
    #[test]
    fn update_check_exits_10_for_a_newer_release_and_this_build_has_a_version() {
        use super::{exit_code, UpdateAvailable, UPDATE_AVAILABLE_EXIT};
        use std::process::ExitCode;
        assert_eq!(UPDATE_AVAILABLE_EXIT, 10);
        assert_eq!(exit_code(&Err(UpdateAvailable.into())), ExitCode::from(10));
        assert_eq!(exit_code(&Err(anyhow::anyhow!("GET …/releases/latest: HTTP 403"))), ExitCode::FAILURE);
        assert!(arena_core::update::Version::parse(env!("ARENA_VERSION")).is_some(), "{}", env!("ARENA_VERSION"));
        assert!(!env!("ARENA_COMMIT").is_empty() && !env!("ARENA_TARGET").is_empty());
        let log = arena_core::update::entries(super::CHANGELOG);
        assert!(log.len() >= 3, "the compiled-in changelog has its entries");
        assert!(log.iter().all(|e| e.heading.len() > 13 && e.heading.as_bytes()[4] == b'-' && e.heading.contains(" · ")), "headings are `YYYY-MM-DD · …`");
    }

    #[test]
    fn replace_keeps_the_old_pod_unless_the_proxy_routes_to_the_new_one() {
        use super::{replace_cleanup_blocker, ProxySync, Written};
        use arena_core::proxy::{ChangeCounts, Forward};
        let fwd = |provider: &str, pod_id: &str| Forward {
            name: "arena8-apple".into(),
            public_port: 7000,
            target_ip: "1.1.1.1".into(),
            target_port: 22000,
            provider: Some(provider.into()),
            pod_id: Some(pod_id.into()),
        };
        let synced = |routed: Vec<Forward>, failed: &[&str]| ProxySync::Synced {
            written: Written::Reloaded,
            counts: ChangeCounts::default(),
            removed: vec![],
            pending: 0,
            failed_providers: failed.iter().map(|s| s.to_string()).collect(),
            routed,
        };
        // (sync outcome, proxied?, terminate allowed?)
        let cases: Vec<(ProxySync, bool, bool)> = vec![
            (synced(vec![fwd("runpod", "new")], &[]), true, true),
            (ProxySync::Skipped("no proxy configured (SSH_PROXY_HOST unset)".into()), false, true),
            // an off-list machine has no forward that could lead to the old pod
            (synced(vec![], &[]), false, true),
            // still routed to the old pod (its listing said so), or kept-stale (not routed):
            (synced(vec![fwd("runpod", "old")], &[]), true, false),
            (synced(vec![], &["runpod"]), true, false),
            (synced(vec![], &[]), true, false),
            // the same pod id on another provider isn't the replacement
            (synced(vec![fwd("vast", "new")], &[]), true, false),
            (ProxySync::Skipped("nginx not found on this host".into()), true, false),
            (ProxySync::Failed("nginx reload failed:\nline two".into()), true, false),
        ];
        for (sync, configured, may_terminate) in cases {
            let got = replace_cleanup_blocker(&sync, configured, "arena8-apple", "runpod", "new");
            assert_eq!(got.is_none(), may_terminate, "{sync:?} -> {got:?}");
            if let Some(why) = got {
                assert!(!why.contains('\n'), "{why}");
            }
        }
        let why = replace_cleanup_blocker(&synced(vec![], &["runpod"]), true, "arena8-apple", "runpod", "new").unwrap();
        assert!(why.contains("runpod failed to list"), "{why}");
    }

    #[test]
    fn rename_failure_hint_never_suggests_proxy_apply_mid_prefix_rename() {
        use super::{rename_failure_hint, RenameRequest};
        let prefix = rename_failure_hint(&RenameRequest::FromPrefix("arena7".into()), 1, 2);
        assert!(prefix.starts_with("1 of 2 rename(s) done"), "{prefix}");
        assert!(prefix.contains("re-run the rename") && prefix.contains("arena7-*"), "{prefix}");
        assert!(prefix.contains("do NOT run `arena proxy apply`") && prefix.contains("cron"), "{prefix}");
        // A single rename that failed changed nothing, so `proxy apply` is harmless there.
        let one = rename_failure_hint(&RenameRequest::One { old: "a".into(), new: "b".into() }, 0, 1);
        assert!(one.contains("or `arena proxy apply`"), "{one}");
    }

    #[test]
    fn replace_stage_names_suffix_canonical() {
        use super::replace_stage_names;
        let (new, old) = replace_stage_names("arena8-apple");
        assert_eq!(new, "arena8-apple-new");
        assert_eq!(old, "arena8-apple-old");
    }

    fn rename_pod_fixture(id: &str, name: &str) -> arena_core::Pod {
        arena_core::Pod {
            id: id.into(),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
            ..Default::default()
        }
    }

    fn rename_list() -> Vec<String> {
        ["apple", "bloom", "cloud", "@james-gpu"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rename_from_prefix_moves_whole_cohort() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![
            rename_pod_fixture("a", "arena8-apple"),
            rename_pod_fixture("b", "arena8-bloom"),
            rename_pod_fixture("j", "james-gpu"),
        ];
        let plan = plan_renames("arena9", &rename_list(), &pods, &RenameRequest::FromPrefix("arena8".into())).unwrap();
        let got: Vec<(&str, &str)> = plan.iter().map(|r| (r.old.as_str(), r.new.as_str())).collect();
        assert_eq!(got, vec![("arena8-apple", "arena9-apple"), ("arena8-bloom", "arena9-bloom")]);
    }

    #[test]
    fn rename_from_prefix_refuses_all_if_any_off_list() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![rename_pod_fixture("a", "arena8-apple"), rename_pod_fixture("z", "arena8-zebra")];
        let err = plan_renames("arena9", &rename_list(), &pods, &RenameRequest::FromPrefix("arena8".into())).unwrap_err();
        assert!(err.to_string().contains("arena9-zebra: not in MACHINE_NAME_LIST"), "{err}");
    }

    #[test]
    fn rename_one_bare_new_name_and_taken_check() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![rename_pod_fixture("a", "arena9-apple"), rename_pod_fixture("b", "arena9-bloom")];
        let one = |old: &str, new: &str| RenameRequest::One { old: old.into(), new: new.into() };
        let plan = plan_renames("arena9", &rename_list(), &pods, &one("apple", "cloud")).unwrap();
        assert_eq!((plan[0].id.as_str(), plan[0].new.as_str()), ("a", "arena9-cloud"));
        let err = plan_renames("arena9", &rename_list(), &pods, &one("apple", "bloom")).unwrap_err();
        assert!(err.to_string().contains("already taken"), "{err}");
        // Absolute list entries resolve without the prefix.
        let plan = plan_renames("arena9", &rename_list(), &pods, &one("b", "james-gpu")).unwrap();
        assert_eq!(plan[0].new, "james-gpu");
    }
}

#[cfg(test)]
mod placement_cli_tests {
    use super::*;

    /// Placement never SSHes; an unscripted FakeRemote would fail any call loudly.
    fn fake_remote() -> std::sync::Arc<dyn arena_core::remote::Remote> {
        std::sync::Arc::new(arena_core::remote::FakeRemote::new())
    }
    use arena_core::placement::Order;
    use arena_core::{Error, Pod, Result as CoreResult};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A RunPod-named provider whose creates succeed except on the GPU types in `dry`
    /// (capacity). Records every create as `(name, gpu, cloud)` and counts list calls.
    #[derive(Default)]
    struct Recorder {
        dry: Vec<&'static str>,
        calls: Mutex<Vec<(String, String, String)>>,
        made: Mutex<Vec<Pod>>,
        lists: AtomicUsize,
        /// A second backend that fails to list (a bogus `VAST_API_KEY`), holding a pod nobody
        /// can see: the aggregate `list_pods` skips it, as the fleet provider's does.
        vast_down: bool,
        /// The $/h each create answers with (`None`: unpriced, as RunPod v1's REST create).
        price: Option<f64>,
        /// Each pod is RUNNING with an SSH endpoint the moment it's made, so `up`'s pipelines
        /// (with `--no-setup`) bring it up at once.
        up_at_once: bool,
        /// How long each listing takes (a slow backend, a RunPod 429 backoff); none by default.
        list_takes: std::time::Duration,
    }

    impl Recorder {
        fn dry(dry: &[&'static str]) -> Self {
            Recorder { dry: dry.to_vec(), ..Default::default() }
        }
        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for Recorder {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, spec: &PodSpec) -> String {
            format!("{} x{}, {}", spec.gpu_type, spec.gpu_count, spec.cloud_type)
        }
        async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if !self.list_takes.is_zero() {
                tokio::time::sleep(self.list_takes).await;
            }
            Ok(self.made.lock().unwrap().clone())
        }
        async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
            let mut out = vec![("runpod".to_string(), self.list_pods().await)];
            if self.vast_down {
                let e = Error::Provider {
                    kind: arena_core::ProviderErrorKind::Auth,
                    message: r#"list instances HTTP 404 Not Found: {"error":"auth_error","msg":"Invalid user key"}"#.into(),
                };
                out.push(("vast".into(), Err(e)));
            }
            out
        }
        async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
            self.calls.lock().unwrap().push((spec.name.clone(), spec.gpu_type.clone(), spec.cloud_type.clone()));
            if self.dry.contains(&spec.gpu_type.as_str()) {
                return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
            }
            let mut pod = Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() };
            pod.cost_per_hr = self.price;
            if self.up_at_once {
                let port = 22_000 + self.made.lock().unwrap().len() as u16;
                (pod.status, pod.ssh_ip, pod.ssh_port) = ("RUNNING".into(), Some("203.0.113.9".into()), Some(port));
            }
            self.made.lock().unwrap().push(pod.clone());
            Ok(pod)
        }
        async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
    }

    /// No RUNPOD_API_KEY (so prices are the preset estimates — no network) and no proxy (so
    /// the post-create sync is a skip note).
    fn cfg() -> Config {
        Config::parse(
            "MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\n\
             MACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n  \"cider\"\n)\n",
        )
    }

    fn create(names: &[&str], gpu: Option<&str>, cloud: Option<&str>, max_price: Option<f64>) -> PodCmd {
        PodCmd::Create {
            names: names.iter().map(|s| s.to_string()).collect(),
            count: None,
            add: None,
            gpu: gpu.map(String::from),
            gpus: None,
            server_type: None,
            cloud: cloud.map(String::from),
            max_price,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            skip_proxy: true,
            dry_run: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 0,
        }
    }

    fn call(name: &str, gpu: &str, cloud: &str) -> (String, String, String) {
        (name.into(), gpu.into(), cloud.into())
    }

    /// Live finding #9 (fleet-mixed): with Vast's key bogus, `create delta`, `up -n 4` and the
    /// `--provider vast` runs all planned names a failed provider might hold — a duplicate pod
    /// billed twice. Every name-picking path — explicit names, `-n`, `-a`, dry run or not,
    /// `create` and `up`, single spec or placement — now refuses, naming the provider and its
    /// key, before any create.
    #[tokio::test]
    async fn create_and_up_refuse_while_any_provider_fails_to_list() {
        let up = |names: &[&str], count: Option<usize>, gpu: Option<&str>, dry_run: bool| PodCmd::Up {
            names: names.iter().map(|s| s.to_string()).collect(),
            count,
            add: None,
            gpu: gpu.map(String::from),
            gpus: None,
            server_type: None,
            cloud: None,
            max_price: None,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: true,
            check: false,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 60,
            endpoint_timeout: None,
            interval: 10,
        };
        let mut cmds = Vec::new();
        for dry_run in [true, false] {
            for gpu in [None, Some("A4000,3090")] {
                let mut c = create(&["delta"], gpu, None, None);
                if let PodCmd::Create { dry_run: d, .. } = &mut c {
                    *d = dry_run;
                }
                cmds.push(c);
                let mut c = create(&[], gpu, None, None);
                if let PodCmd::Create { count, dry_run: d, .. } = &mut c {
                    (*count, *d) = (Some(4), dry_run);
                }
                cmds.push(c);
                let mut c = create(&[], gpu, None, None);
                if let PodCmd::Create { add, dry_run: d, .. } = &mut c {
                    (*add, *d) = (Some(1), dry_run);
                }
                cmds.push(c);
                cmds.push(up(&["alpha"], None, gpu, dry_run));
                cmds.push(up(&[], Some(4), gpu, dry_run));
            }
        }
        for cmd in cmds {
            let rec = Recorder { vast_down: true, ..Default::default() };
            let e = handle_pods(cmd, &rec, fake_remote(), &cfg(), true).await.unwrap_err();
            let msg = format!("{e:#}");
            assert!(msg.contains("vast failed to list") && msg.contains("refusing to pick machine names"), "{msg}");
            assert!(msg.contains("VAST_API_KEY was rejected"), "{msg}");
            assert!(rec.calls().is_empty(), "nothing created: {:?}", rec.calls());
        }
        // The same commands with every provider answering go ahead.
        let rec = Recorder::default();
        handle_pods(create(&["delta"], None, None, None), &rec, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(rec.calls().len(), 1);
    }

    #[test]
    fn only_several_options_or_a_cap_leave_the_single_spec_path() {
        let ov = |gpu: Option<&str>, cloud: Option<&str>| SpecOverrides {
            gpu: gpu.map(String::from),
            cloud: cloud.map(String::from),
            ..Default::default()
        };
        let route = |o: &SpecOverrides, cap, kt| {
            placement_request(&cfg(), "runpod", o.clone(), cap, Order::Cheapest, kt).map(|(_, placing)| placing)
        };
        // Today's flags (or none) stay on the single-spec path — `--keep-trying` included.
        // (Community: near the proxy has nothing to pin there.)
        for o in [ov(None, None), ov(Some("3090"), None), ov(Some("A4000"), Some("community"))] {
            assert!(route(&o, None, false).unwrap().is_none(), "{o:?}");
            assert!(route(&o, None, true).unwrap().is_none(), "{o:?}");
        }
        // One SECURE option with NEAR on (the default) is two to try — Europe, then anywhere —
        // unless `--keep-trying`, which waits on one pool: the single path, anywhere.
        let secure = ov(Some("A4000"), Some("secure"));
        let (req, _) = route(&secure, None, false).unwrap().expect("placement");
        assert_eq!(req.near.map(|n| n.region), Some(arena_core::near::Region::Europe));
        assert!(route(&secure, None, true).unwrap().is_none());
        let off = Config::parse("GPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nNEAR=any\n");
        assert!(placement_request(&off, "runpod", secure.clone(), None, Order::Cheapest, false).unwrap().1.is_none(), "NEAR=any");
        let bad = Config::parse("NEAR=mars\n");
        assert!(placement_request(&bad, "runpod", secure, None, Order::Cheapest, false).is_err(), "refused before anything");
        let (req, base) = route(&ov(Some("A4000,3090"), None), None, false).unwrap().unwrap();
        assert_eq!(req.gpus, ["NVIDIA RTX A4000", "NVIDIA GeForce RTX 3090"]);
        assert_eq!((base.gpu_type.as_str(), base.cloud_type.as_str()), ("NVIDIA RTX A4000", "COMMUNITY"));
        assert!(route(&ov(None, Some("community,secure")), None, false).unwrap().is_some());
        assert!(route(&ov(None, None), Some(0.5), false).unwrap().is_some());
        let e = route(&ov(Some("A4000,3090"), None), None, true).unwrap_err().to_string();
        assert!(e.contains("--retry-mins"), "{e}");
        let e = route(&ov(None, Some("community,spot")), None, false).unwrap_err().to_string();
        assert!(e.contains("`SPOT`"), "{e}");
    }

    #[test]
    fn a_list_that_collapses_to_one_option_creates_that_option_not_the_raw_string() {
        // Review fix: duplicates / stray commas parse to one option, so the single path runs —
        // and it must create the parsed option, not the raw flag text as a GPU id / tier.
        // (NEAR=any: near the proxy would make a SECURE option two — Europe, then anywhere.)
        let cfg = || Config::parse("MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nNEAR=any\n");
        let single = |gpu: Option<&str>, cloud: Option<&str>| {
            let o = SpecOverrides { gpu: gpu.map(String::from), cloud: cloud.map(String::from), ..Default::default() };
            let (ov, placing) = placement_request(&cfg(), "runpod", o, None, Order::Cheapest, false).unwrap();
            assert!(placing.is_none(), "{gpu:?} {cloud:?} is one option");
            let spec = spec_with_overrides(&cfg(), &ov);
            (spec.gpu_type, spec.cloud_type)
        };
        let a4000 = "NVIDIA RTX A4000";
        let rtx3090 = "NVIDIA GeForce RTX 3090";
        for (gpu, cloud, want) in [
            (Some("A4000,a4000"), Some("community,COMMUNITY"), (a4000, "COMMUNITY")),
            (Some("NVIDIA RTX A4000,"), None, (a4000, "COMMUNITY")),
            (None, Some("community,"), (a4000, "COMMUNITY")),
            (Some("3090, 3090"), Some("secure,SECURE"), (rtx3090, "SECURE")),
            (Some(" 3090 "), Some(" secure "), (rtx3090, "SECURE")),
            // Unchanged: plain single flags, and absent flags keep config's values.
            (Some("3090"), Some("secure"), (rtx3090, "SECURE")),
            (Some("NVIDIA GeForce RTX 3090"), None, (rtx3090, "COMMUNITY")),
            (None, None, (a4000, "COMMUNITY")),
        ] {
            let got = single(gpu, cloud);
            assert_eq!((got.0.as_str(), got.1.as_str()), want, "{gpu:?} {cloud:?}");
        }
    }

    #[tokio::test]
    async fn a_collapsing_list_on_create_sends_the_one_parsed_option() {
        let fake = Recorder::default();
        handle_pods(create(&["apple"], Some("A4000,a4000"), Some("community,COMMUNITY"), None), &fake, fake_remote(), &cfg(), true)
            .await
            .unwrap();
        assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]);
    }

    /// One press holds for every phase of the command — including one that only starts
    /// waiting after it (a fresh `tokio::signal::ctrl_c()` there would never fire).
    #[tokio::test(start_paused = true)]
    async fn a_ctrl_c_stays_pressed_for_every_later_phase() {
        let ctrl_c = Interrupt::manual();
        let later = ctrl_c.clone();
        assert!(!later.is_set());
        let waiting = tokio::spawn(ctrl_c.wait());
        ctrl_c.press();
        waiting.await.unwrap();
        assert!(later.is_set());
        tokio::time::timeout(std::time::Duration::from_secs(1), later.wait()).await.expect("seen at once by a later phase");
    }

    /// An `up --check` replacement waits for capacity the way the first create did.
    #[test]
    fn replacement_rounds_follow_the_create_flags() {
        use arena_core::placement::{next_round_in_window, Rounds};
        let s = std::time::Duration::from_secs;
        // (keep_trying, retry_mins, retry_secs) → rounds
        let cases = [
            ((false, 0, 60), Rounds { window: s(0), every: s(60) }),   // one try
            ((false, 5, 30), Rounds { window: s(300), every: s(30) }), // --retry-mins
            ((true, 5, 30), Rounds { window: s(300), every: s(30) }),  // --retry-mins wins, as in create_with_retry
            ((true, 0, 60), Rounds { window: s(CAPACITY_RETRY_SECS * u64::from(MAX_CAPACITY_ATTEMPTS)), every: s(CAPACITY_RETRY_SECS) }),
            ((false, 0, 0), Rounds { window: s(0), every: s(1) }),
        ];
        for ((kt, mins, secs), want) in cases {
            let got = replacement_rounds(kt, mins, secs);
            assert_eq!((got.window, got.every), (want.window, want.every), "keep_trying={kt} mins={mins} secs={secs}");
        }
        // --keep-trying: the first try + MAX_CAPACITY_ATTEMPTS retries, as `create_pods` makes.
        let r = replacement_rounds(true, 0, 60);
        let start = tokio::time::Instant::now();
        let (mut now, mut rounds) = (start, 1);
        while next_round_in_window(now, r.every, start + r.window) {
            now += r.every;
            rounds += 1;
        }
        assert_eq!(rounds, 1 + MAX_CAPACITY_ATTEMPTS);
    }

    /// The single-option retry loop starts no round after `--retry-mins` (the multi-option
    /// executor's rule): it used to check the window only *before* sleeping, so with a
    /// `--retry-secs` longer than what was left it slept past the window and created anyway.
    #[tokio::test(start_paused = true)]
    async fn single_option_retry_starts_no_round_after_the_window() {
        // (retry_mins, retry_secs, rounds expected): rounds at t = 0, secs, 2·secs, … ≤ window.
        for (mins, secs, want) in [(1, 600, 1), (1, 45, 2), (2, 60, 3), (1, 60, 2), (0, 30, 1)] {
            let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
            let started = tokio::time::Instant::now();
            let made = create_with_retry(&fake, &cfg(), vec!["arena8-apple".into()], 0, &SpecOverrides::default(), false, mins, secs, &Interrupt::manual(), NO_STREAM)
                .await
                .unwrap();
            assert!(made.is_empty());
            assert_eq!(fake.calls().len(), want, "--retry-mins {mins} --retry-secs {secs}");
            // It gave up right after the last round — no sleep just to give up.
            assert_eq!(started.elapsed(), std::time::Duration::from_secs(secs * (want as u64 - 1)), "{mins}/{secs}");
        }
    }

    /// The dashboard's add-pod form retries with `--retry-mins` up to a day (the human,
    /// 2026-10-30: "an auto-retry option"), and its runs panel's `[s]` sends Ctrl+C: between two
    /// rounds that stops the retries at once — no round after it, nothing made, the run going on
    /// to its end with what it has — however long the window had left.
    #[tokio::test(start_paused = true)]
    async fn ctrl_c_between_rounds_ends_a_day_long_retry_window_at_once() {
        let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
        let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
        let started = tokio::time::Instant::now();
        let press = async {
            tokio::time::sleep(std::time::Duration::from_secs(150)).await;
            ctrl_c.press();
        };
        let (made, ()) = tokio::join!(
            create_with_retry(&fake, &config, vec!["arena8-apple".into()], 0, &ov, false, 1440, 60, &ctrl_c, NO_STREAM),
            press
        );
        assert!(made.unwrap().is_empty());
        assert_eq!(fake.calls().len(), 3, "rounds at t = 0, 60, 120; none after the press");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(150), "stopped at the press, not at the next round");
    }

    /// A press while the fleet is re-listed before a round (slow: 20 s here) stops there too: it
    /// was seen only during the sleep, so the round after a slow listing still created (billed)
    /// after the dashboard's `[s]` said "stopping" (pre-merge review of #38, NIT 1).
    #[tokio::test(start_paused = true)]
    async fn ctrl_c_during_the_relist_creates_nothing_more() {
        let fake = Recorder { list_takes: std::time::Duration::from_secs(20), ..Recorder::dry(&["NVIDIA RTX A4000"]) };
        let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
        let started = tokio::time::Instant::now();
        let press = async {
            // Round 1 at t=0; the wait ends at t=60, the re-list runs to t=80: pressed at t=70,
            // during it.
            tokio::time::sleep(std::time::Duration::from_secs(70)).await;
            ctrl_c.press();
        };
        let (made, ()) = tokio::join!(
            create_with_retry(&fake, &config, vec!["arena8-apple".into()], 0, &ov, false, 1440, 60, &ctrl_c, NO_STREAM),
            press
        );
        assert!(made.unwrap().is_empty());
        assert_eq!(fake.calls().len(), 1, "round 1 only: no round after a press during the re-list");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(70), "stopped at the press, not after the listing");
    }

    /// The link the test above stands in for: a real SIGINT — what the dashboard's `[s]` sends
    /// the run's process group (`kill -INT -- -pgid`) — presses [`Interrupt::ctrl_c`] once a
    /// retry wait is listening, and the process goes on (it isn't killed by it). In a child
    /// process (this test binary again, running only [`interrupt_child`]), so no other test's
    /// latch can see the signal (review ai-review codex r2: Ctrl+C's path unverified).
    #[test]
    fn a_real_sigint_presses_the_retry_waits_ctrl_c() {
        use std::io::{BufRead, BufReader};
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "placement_cli_tests::interrupt_child", "--ignored", "--nocapture", "--test-threads=1"])
            .env("ARENA_INTERRUPT_CHILD", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut seen = Vec::new();
        for line in out.by_ref() {
            let line = line.unwrap();
            seen.push(line.clone());
            // (libtest's `test … ... ` has no newline: the line may start with it.)
            if line.ends_with("interrupt-child: listening") {
                break;
            }
        }
        assert!(seen.last().is_some_and(|l| l.ends_with("interrupt-child: listening")), "{seen:?}");
        let sent = std::process::Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
        assert!(sent.success());
        seen.extend(out.map_while(Result::ok));
        let status = child.wait().unwrap();
        assert!(seen.iter().any(|l| l == "interrupt-child: pressed"), "{seen:?}");
        assert!(status.success(), "the SIGINT was taken, not left to end the process: {status:?} {seen:?}");
    }

    /// Not a test on its own: [`a_real_sigint_presses_the_retry_waits_ctrl_c`]'s child. Listens
    /// as a retry wait does, says so, and reports the press (or gives up after 20 s).
    #[tokio::test]
    #[ignore = "a child process of a_real_sigint_presses_the_retry_waits_ctrl_c"]
    async fn interrupt_child() {
        if std::env::var("ARENA_INTERRUPT_CHILD").is_err() {
            return;
        }
        let ctrl_c = Interrupt::ctrl_c();
        let pressed = ctrl_c.wait(); // arms: the SIGINT listener is registered here
        println!("interrupt-child: listening");
        match tokio::time::timeout(std::time::Duration::from_secs(20), pressed).await {
            Ok(()) => println!("interrupt-child: pressed"),
            Err(_) => println!("interrupt-child: timed out"),
        }
        assert!(ctrl_c.is_set());
    }

    /// RunPod with every create dry until `wet_at`, next to a Vast that's throttled for the
    /// next `vast_throttled` listings (a 429, retryable) — or, `vast_refused`, rejects its key.
    struct Flaky {
        wet_at: tokio::time::Instant,
        made: Mutex<Vec<Pod>>,
        calls: Mutex<Vec<String>>,
        vast_throttled: AtomicUsize,
        vast_refused: bool,
    }

    impl Flaky {
        fn new(dry_for: u64) -> Self {
            Flaky {
                wet_at: tokio::time::Instant::now() + std::time::Duration::from_secs(dry_for),
                made: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                vast_throttled: AtomicUsize::new(0),
                vast_refused: false,
            }
        }
    }

    #[async_trait]
    impl Provider for Flaky {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
            Ok(self.made.lock().unwrap().clone())
        }
        async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
            let vast = if self.vast_refused {
                Err(Error::Provider { kind: arena_core::ProviderErrorKind::Auth, message: "vast list HTTP 401".into() })
            } else if self.vast_throttled.load(Ordering::SeqCst) > 0 {
                self.vast_throttled.fetch_sub(1, Ordering::SeqCst);
                Err(Error::Provider { kind: arena_core::ProviderErrorKind::RateLimited, message: "vast list HTTP 429".into() })
            } else {
                Ok(Vec::new())
            };
            vec![("runpod".to_string(), self.list_pods().await), ("vast".to_string(), vast)]
        }
        async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
            self.calls.lock().unwrap().push(spec.name.clone());
            if tokio::time::Instant::now() < self.wet_at {
                return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
            }
            let pod = Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() };
            self.made.lock().unwrap().push(pod.clone());
            Ok(pod)
        }
        async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
    }

    /// Review fix: the single-option retry loop (`create`/`up -n` without a `--gpu` list)
    /// re-lists like placement's rounds do — a round retries only the confirmed names not made
    /// yet (one that exists now is skipped), a listing missing a backend that may answer again
    /// skips the round instead of ending the run, and one that won't (a rejected key) ends it.
    #[tokio::test(start_paused = true)]
    async fn single_option_retry_rounds_relist_and_ride_out_a_throttled_backend() {
        let s = std::time::Duration::from_secs;
        let names = || vec!["arena8-apple".to_string(), "arena8-bloom".to_string()];
        // Dry until t=100. Round 2 (t=60): Vast is throttled through the re-list's retries
        // (15s) — skipped. Meanwhile someone else made arena8-bloom. Round 3 (t=135): apple only.
        let fake = Flaky::new(100);
        fake.vast_throttled.store(5, Ordering::SeqCst);
        let started = tokio::time::Instant::now();
        let other = async {
            tokio::time::sleep(s(30)).await;
            let bloom = Pod { id: "theirs".into(), name: "arena8-bloom".into(), provider: "runpod".into(), ..Default::default() };
            fake.made.lock().unwrap().push(bloom);
        };
        let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
        let (made, ()) =
            tokio::join!(create_with_retry(&fake, &config, names(), 2, &ov, false, 10, 60, &ctrl_c, NO_STREAM), other);
        let made: Vec<String> = made.unwrap().into_iter().map(|m| m.pod.name).collect();
        assert_eq!(made, ["arena8-apple"]);
        assert_eq!(*fake.calls.lock().unwrap(), ["arena8-apple", "arena8-apple"], "t=0 (dry, the round stops), then t=135");
        assert_eq!(started.elapsed(), s(60 + 15 + 60));

        // Vast throttled for good: the window ends the run, with no create after round 1.
        let fake = Flaky::new(0);
        fake.vast_throttled.store(usize::MAX, Ordering::SeqCst);
        let dry = Flaky { wet_at: tokio::time::Instant::now() + s(3600), ..fake };
        let made = create_with_retry(&dry, &cfg(), names(), 2, &SpecOverrides::default(), false, 3, 60, &Interrupt::manual(), NO_STREAM)
            .await
            .unwrap();
        assert!(made.is_empty());
        assert_eq!(dry.calls.lock().unwrap().len(), 1, "only round 1 created");

        // Vast's key rejected: no round runs on a listing that won't become whole.
        let refused = Flaky { vast_refused: true, ..Flaky::new(3600) };
        let err = create_with_retry(&refused, &cfg(), names(), 0, &SpecOverrides::default(), false, 3, 60, &Interrupt::manual(), NO_STREAM)
            .await
            .unwrap_err();
        assert!(err.error.to_string().contains("refusing to create") && err.error.to_string().contains("vast failed to list"), "{}", err.error);
        assert_eq!(refused.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_with_a_gpu_list_falls_back_and_later_names_skip_the_dry_pool() {
        let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
        handle_pods(create(&["apple", "bloom"], Some("A4000,3090"), None, None), &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(
            fake.calls(),
            [
                call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY"), // cheapest (~$0.17) first: dry
                call("arena8-apple", "NVIDIA GeForce RTX 3090", "COMMUNITY"),
                call("arena8-bloom", "NVIDIA GeForce RTX 3090", "COMMUNITY"), // A4000 blocked this round
            ]
        );
        assert_eq!(fake.made.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_single_gpu_and_cloud_create_is_unchanged() {
        // One option: the single-spec path — `--keep-trying` still accepted, one create per name.
        let fake = Recorder::default();
        let mut cmd = create(&["apple"], Some("3090"), Some("secure"), None);
        if let PodCmd::Create { keep_trying, order, .. } = &mut cmd {
            *keep_trying = true;
            *order = Order::Listed; // meaningless with one option
        }
        handle_pods(cmd, &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA GeForce RTX 3090", "SECURE")]);
    }

    #[tokio::test]
    async fn refusals_happen_before_anything_is_listed_or_created() {
        let fake = Recorder::default();
        let mut kt = create(&["apple"], Some("A4000,3090"), None, None);
        if let PodCmd::Create { keep_trying, .. } = &mut kt {
            *keep_trying = true;
        }
        assert!(handle_pods(kt, &fake, fake_remote(), &cfg(), true).await.is_err());
        let bad_tier = create(&["apple"], Some("A4000"), Some("community,all"), None);
        let e = handle_pods(bad_tier, &fake, fake_remote(), &cfg(), true).await.unwrap_err().to_string();
        assert!(e.contains("`ALL`"), "{e}");
        // A cap nothing fits under (A4000 ~$0.17, 3090 ~$0.22): refused, nothing created.
        let e = handle_pods(create(&["apple"], Some("A4000,3090"), None, Some(0.10)), &fake, fake_remote(), &cfg(), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("no option left to try"), "{e}");
        assert!(fake.calls().is_empty());
        assert_eq!(fake.lists.load(Ordering::SeqCst), 1, "only the cap case got as far as listing names");
    }

    #[tokio::test]
    async fn a_price_cap_filters_and_a_dry_run_creates_nothing() {
        // 2 GPUs per pod: A4000 ~$0.34 fits $0.40, 3090 ~$0.44 doesn't.
        let fake = Recorder::default();
        let mut cmd = create(&["apple"], Some("3090,A4000"), None, Some(0.40));
        if let PodCmd::Create { gpus, .. } = &mut cmd {
            *gpus = Some(2);
        }
        handle_pods(cmd, &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]);

        let fake = Recorder::default();
        let up = PodCmd::Up {
            names: vec![],
            count: Some(2),
            add: None,
            gpu: Some("A4000,3090".into()),
            gpus: None,
            server_type: None,
            cloud: Some("community,secure".into()),
            max_price: Some(0.30),
            order: Order::Listed,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run: true,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            check: true,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 600,
            endpoint_timeout: None,
            interval: 12,
        };
        handle_pods(up, &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn up_places_through_the_options_then_carries_on() {
        let fake = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
        let up = PodCmd::Up {
            names: vec!["apple".into()],
            count: None,
            add: None,
            gpu: Some("3090,A4000".into()),
            gpus: None,
            server_type: None,
            cloud: None,
            max_price: None,
            order: Order::Listed, // 3090 first despite the price
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run: false,
            no_wait: true,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: true,
            check: false,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 600,
            endpoint_timeout: None,
            interval: 12,
        };
        handle_pods(up, &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(
            fake.calls(),
            [call("arena8-apple", "NVIDIA GeForce RTX 3090", "COMMUNITY"), call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]
        );
    }

    /// The `up` in [`up_places_through_the_options_then_carries_on`]: 3090 then A4000.
    fn up_apple() -> PodCmd {
        PodCmd::Up {
            names: vec!["apple".into()],
            count: None,
            add: None,
            gpu: Some("3090,A4000".into()),
            gpus: None,
            server_type: None,
            cloud: None,
            max_price: None,
            order: Order::Listed,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            api_json: None,
            reap: crate::ReapAtCreate::default(),
            owner: crate::spend::OwnerArgs::default(),
            dry_run: false,
            no_wait: true,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: true,
            check: false,
            check_attempts: 2,
            retry_bad_hosts: false,
            lock: false,
            timeout: 600,
            endpoint_timeout: None,
            interval: 12,
        }
    }

    /// A balance reading `live`, with a state file of its own (removed on drop): inline
    /// surfaces read live only when the state can be saved.
    struct TestBal(money::BalanceCtx, std::path::PathBuf);
    impl std::ops::Deref for TestBal {
        type Target = money::BalanceCtx;
        fn deref(&self) -> &money::BalanceCtx {
            &self.0
        }
    }
    impl Drop for TestBal {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    fn balance_of(live: arena_core::balance::Live) -> TestBal {
        use arena_core::balance::{Account, FakeBalanceApi};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("arena-preflight-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let path = Some(Ok(dir.join("devtest").join(arena_core::snapshot::BALANCE_FILE)));
        TestBal(money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, live)), path }, dir)
    }

    /// Invariant 2 for `up`/`create`: a critical balance (under RunPod's minimum, under an
    /// hour's credit for the new pods) is said next to the plan — and changes nothing: with
    /// `--yes` the very same creates run as with no balance at all, dry runs create nothing.
    #[tokio::test]
    async fn up_preflight_warns_and_still_creates() {
        use arena_core::balance::Live;
        let poor = Live { usd: 0.10, provider_burn_per_hr: Some(0.47), spend_limit_per_hr: Some(80.0), under_balance: Some(true), reduced: false, owed: None };
        let bal = balance_of(poor);
        let plain = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
        handle_pods(up_apple(), &plain, fake_remote(), &cfg(), true).await.unwrap();
        let fake = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
        handle_pods_bal(up_apple(), &fake, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
        assert_eq!(fake.calls(), plain.calls());
        assert_eq!(fake.calls().len(), 2);
        // `create` likewise; a dry run creates nothing either way.
        let plain = Recorder::default();
        handle_pods(create(&["apple"], None, None, None), &plain, fake_remote(), &cfg(), true).await.unwrap();
        let fake = Recorder::default();
        handle_pods_bal(create(&["apple"], None, None, None), &fake, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
        assert_eq!((fake.calls(), fake.calls().len()), (plain.calls(), 1));
        let dry = Recorder::default();
        let mut cmd = create(&["apple"], None, None, None);
        if let PodCmd::Create { dry_run, .. } = &mut cmd {
            *dry_run = true;
        }
        handle_pods_bal(cmd, &dry, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
        assert!(dry.calls().is_empty());
        // What it said: the runway after, an hour's credit, underBalance.
        let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
        let req = arena_core::placement::Request::from_flags(Some("3090,A4000"), None, &base, None, Order::Listed).unwrap();
        let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
        let lines = money::balance_preflight(&fake, &cfg(), &bal, 1, Some(&plan), &base, None).await;
        // The options' prices are preset estimates (no key): raised to the known GPU's
        // stand-in (RunPod's recorded catalog), marked `~`.
        // The reading's age shows once it's a second old (`live 1s`): a loaded machine gets there
        // between `balance_of` and here, so it's taken out before comparing.
        let first = match lines[0].split_once("live ") {
            Some((head, rest)) => match rest.split_once("s ·") {
                Some((n, tail)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => format!("{head}live ·{tail}"),
                _ => lines[0].clone(),
            },
            None => lines[0].clone(),
        };
        assert!(first.starts_with("balance: runpod $0.10 live · $0.47/h now + ≤~$") && first.ends_with("‼ under 24h"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("⚠ RunPod needs ≥1h of credit for the new pods (≈~$")), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "‼ RunPod reports the account under its minimum balance — creates will likely fail");
    }

    #[tokio::test]
    async fn create_preflight_credit_and_spend_limit_lines() {
        use arena_core::balance::{BalancePolicy, Live};
        let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
        let req = arena_core::placement::Request::from_flags(Some("A4000,3090"), None, &base, None, Order::Cheapest).unwrap();
        let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
        // No key: the options carry preset estimates, so the bound is each option at the most
        // its GPU is known to cost (A4000 $0.32, RTX 3090 $0.50 recorded) — here $0.50.
        let policy = BalancePolicy::default();
        let dearest = plan
            .options
            .iter()
            .map(|o| o.price_per_pod.unwrap_or(0.0).max(arena_core::balance::stand_in_price(Some(&o.gpu), Some(o.gpu_count), &policy).unwrap()))
            .fold(0.0, f64::max);
        assert!((dearest - 0.5).abs() < 1e-9, "{plan:?}");
        // Plenty of money, but 400 new pods at the dearest option would pass the $80/h limit.
        let near_limit = Live { usd: 5_000.0, provider_burn_per_hr: Some(10.0), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None };
        let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(near_limit), 400, Some(&plan), &base, None).await;
        let want = format!(
            "⚠ planned ~${:.2}/h exceeds RunPod's spend limit $80.00/h — creates past it will be refused",
            10.0 + 400.0 * dearest
        );
        assert!(lines.contains(&want), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("1h of credit")), "{lines:?}");
        // $1 for 10 new pods: under an hour's credit.
        let thin = Live { usd: 1.0, provider_burn_per_hr: Some(0.0), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None };
        let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, Some(&plan), &base, None).await;
        let credit = format!(
            "⚠ RunPod needs ≥1h of credit for the new pods (≈~${:.2}); the account has $1.00 — creates will likely fail (402)",
            10.0 * dearest
        );
        assert!(lines.contains(&credit), "{lines:?}");
        // The single-spec path: nothing priced the A4000 — it stands in at the most it is
        // known to cost ($0.32/h), never at $0: the runway and the credit check use it, `~`.
        let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, None, &base, None).await;
        assert_eq!(
            lines,
            [
                "balance: runpod $1.00 live · $0.00/h now + ≤~$3.20/h for 10 new pod(s) (a stand-in that errs high — no live price) → <1h ‼ under 24h",
                "⚠ RunPod needs ≥1h of credit for the new pods (≈~$3.20); the account has $1.00 — creates will likely fail (402)",
            ]
        );
        // An option nobody priced counts at its stand-in in the `≤` bound (it may be the
        // dearest): an H100 SXM at RunPod's recorded $3.49, never the June preset's $2.79.
        let mut unpriced = plan.clone();
        unpriced.options[0].price_per_pod = None;
        unpriced.options[0].gpu = "NVIDIA H100 80GB HBM3".into();
        let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, Some(&unpriced), &base, None).await;
        assert!(lines[0].contains("+ ≤~$34.90/h for 10 new pod(s)"), "{lines:?}");
        // A live catalog price is taken as it is (it's what RunPod charges right now).
        let mut live_priced = plan.clone();
        for o in live_priced.options.iter_mut() {
            (o.price_per_pod, o.price_source) = (Some(0.2), Some(arena_core::placement::PriceSource::Live));
        }
        let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin), 10, Some(&live_priced), &base, None).await;
        assert!(lines[0].contains("+ ≤$2.00/h for 10 new pod(s) →"), "{lines:?}");
        // No account for this provider configured: nothing.
        assert!(money::balance_preflight(&Recorder::default(), &cfg(), &money::BalanceCtx::from_config(&cfg()), 10, Some(&plan), &base, None).await.is_empty());
    }

    /// Merge of wave 7 balance onto lane G's `up`: every path that makes a pod remembers the
    /// price its create answered with, for the balance (a later listing with no price —
    /// RunPod v1's REST list — still prices the pod): `create`, `up --no-wait`, and the
    /// streaming `up` whose pipelines start as each create returns — single spec and
    /// placement alike. A create with no price remembers nothing.
    #[tokio::test]
    async fn every_create_path_remembers_its_price_for_the_balance() {
        use arena_core::balance::{self, Account, Live};
        let live = Live { usd: 100.0, provider_burn_per_hr: Some(0.0), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None };
        let bal = balance_of(live);
        let remembered = |id: &str| {
            let (state, _) = balance::load(bal.state_path().unwrap());
            state.accounts.get(&Account::Runpod).and_then(|a| a.prices.get(id)).map(|(usd, _)| *usd)
        };
        let streaming = |gpu: Option<&str>| {
            let mut up = up_apple();
            if let PodCmd::Up { gpu: g, no_wait, timeout, interval, .. } = &mut up {
                (*g, *no_wait, *timeout, *interval) = (gpu.map(String::from), false, 60, 1);
            }
            up
        };
        let cases: [(&str, PodCmd, f64); 5] = [
            ("create, single spec", create(&["apple"], None, None, None), 0.21),
            ("create, placement", create(&["apple"], Some("3090,A4000"), None, None), 0.22),
            ("up --no-wait, placement", up_apple(), 0.23),
            ("up, placement (streaming)", streaming(Some("3090,A4000")), 0.24),
            ("up, single spec (streaming)", streaming(None), 0.25),
        ];
        for (case, cmd, price) in cases {
            let rec = Recorder { price: Some(price), up_at_once: true, ..Default::default() };
            handle_pods_bal(cmd, &rec, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap_or_else(|e| panic!("{case}: {e:#}"));
            let made = rec.made.lock().unwrap().clone();
            assert_eq!(made.len(), 1, "{case}");
            assert_eq!(remembered(&made[0].id), Some(price), "{case}");
        }
        // Unpriced: nothing new remembered (the last case's price stays as it was).
        let rec = Recorder { up_at_once: true, ..Default::default() };
        handle_pods_bal(streaming(None), &rec, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
        assert_eq!(remembered("id-arena8-apple"), Some(0.25));
    }

    #[tokio::test]
    async fn placement_plans_without_a_fleet_or_a_key() {
        // `offers` itself (every provider, `--all`, `gpus`): `offers::tests`.
        let base = spec_with_overrides(&cfg(), &SpecOverrides { gpus: Some(2), ..Default::default() });
        let req = arena_core::placement::Request::from_flags(Some("3090,A4000"), None, &base, Some(0.40), Order::Cheapest).unwrap();
        let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
        assert_eq!(plan.options.len(), 1);
        assert_eq!(plan.notes[0], "no RUNPOD_API_KEY — prices are preset estimates (~)");
        assert_eq!(
            plan_header(&plan),
            "Placement options on runpod (2 GPU(s) per pod, cheapest first, max $0.40/h per pod):"
        );
        // Vast without a key can't search: unpriced (so a cap leaves nothing), and it says why.
        let vast = plan_placement(&cfg(), "vast", &req, &base).await.unwrap();
        assert!(vast.options.is_empty() && vast.dropped.len() == 2, "{vast:?}");
        assert_eq!(vast.notes[0], "no VAST_API_KEY — vast options are unpriced (--max-price can't check them)");
    }

    /// The shared planner applies NEAR itself, so `offers` (whatever shape it takes) shows the
    /// order create/up run: SECURE options in Europe first; a community-only plan says why
    /// nothing is near; `NEAR=any` plans as before.
    #[tokio::test]
    async fn the_shared_planner_puts_near_options_first() {
        use arena_core::near::Region;
        let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
        let req = |cloud: &str| arena_core::placement::Request::from_flags(Some("A4000"), Some(cloud), &base, None, Order::Cheapest).unwrap();
        let plan = plan_placement(&cfg(), "runpod", &req("secure"), &base).await.unwrap();
        let near: Vec<Option<Region>> = plan.options.iter().map(|o| o.near).collect();
        assert_eq!(near, [Some(Region::Europe), None], "{plan:?}");
        assert!(arena_core::placement::render_plan(&plan).contains("WHERE"));
        let community = plan_placement(&cfg(), "runpod", &req("community"), &base).await.unwrap();
        assert_eq!(community.options.len(), 1);
        assert!(community.notes.iter().any(|n| n.starts_with("near (Europe): RunPod's COMMUNITY cloud can't be placed by data centre")), "{:?}", community.notes);
        let off = Config::parse("MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nNEAR=any\n");
        let plain = plan_placement(&off, "runpod", &req("secure"), &base).await.unwrap();
        assert!(plain.near.is_none() && plain.options.len() == 1 && !plain.notes.iter().any(|n| n.contains("near")), "{plain:?}");
        // The operator's own data-centre pin: no near option, and a note says so.
        let pinned = PodSpec { api_extra: serde_json::json!({"dataCenterIds": ["US-TX-3"]}).as_object().cloned(), ..base.clone() };
        let own = plan_placement(&cfg(), "runpod", &req("secure"), &pinned).await.unwrap();
        assert_eq!(own.options.len(), 1);
        assert!(own.notes.iter().any(|n| n.starts_with("near (Europe): off — CREATE_EXTRA_JSON / --api-json already says where")), "{:?}", own.notes);
    }

    #[test]
    fn placement_flags_parse() {
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        match parse(&["arena", "offers", "--gpu", "A4000,3090", "--max-price", "$0.5", "--gpus", "2", "--order", "listed", "--json"]).unwrap() {
            Cmd::Offers { gpu, max_price, gpus, order, all, json, .. } => {
                assert_eq!((gpu.as_deref(), max_price, gpus, order, all, json), (Some("A4000,3090"), Some(0.5), Some(2), Order::Listed, false, true));
            }
            _ => panic!("not offers"),
        }
        match parse(&["arena", "offers", "--provider", "hetzner", "--server-type", "cx33"]).unwrap() {
            Cmd::Offers { server_type, all, .. } => assert_eq!((server_type.as_deref(), all), (Some("cx33"), false)),
            _ => panic!("not offers"),
        }
        // A view is a provider (or `all`), and takes the filters; `--all` is `all`'s old
        // spelling, never beside a view; `-v` the notes.
        for view in offers::VIEWS {
            match parse(&["arena", "offers", view, "--gpu", "A4000", "--cloud", "secure", "--max-price", "1", "-v"]).unwrap() {
                Cmd::Offers { view: v, gpu, cloud, max_price, verbose, all, .. } => {
                    assert_eq!((v.as_deref(), gpu.as_deref(), cloud.as_deref(), max_price, verbose, all), (Some(view), Some("A4000"), Some("secure"), Some(1.0), true, false));
                }
                _ => panic!("not offers"),
            }
        }
        assert!(matches!(parse(&["arena", "offers", "--all", "--json", "--gpu", "B300 MIG"]).unwrap(), Cmd::Offers { all: true, json: true, view: None, .. }));
        assert!(parse(&["arena", "offers", "lambda"]).is_err());
        assert!(parse(&["arena", "offers", "runpod", "--all"]).is_err());
        match parse(&["arena", "pods", "create", "-n", "3", "--gpu", "A4000,3090", "--cloud", "community,secure"]).unwrap() {
            Cmd::Pods(PodCmd::Create { order, max_price, .. }) => assert_eq!((order, max_price), (Order::Cheapest, None)),
            _ => panic!("not create"),
        }
        assert!(matches!(parse(&["arena", "pods", "up", "-n", "1", "--max-price", "0.3"]).unwrap(), Cmd::Pods(PodCmd::Up { max_price: Some(_), .. })));
        for bad in [&["arena", "offers", "--max-price", "0"][..], &["arena", "offers", "--max-price", "cheap"], &["arena", "offers", "--order", "fastest"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}

/// Every pod-SSH path over a scripted `FakeRemote` (PLAN 1.B): each call carries its
/// budget, partial failures are reported per pod and fail the command, and one wedged pod
/// never holds up the others — on a paused clock, so budgets elapse instantly and exactly.
#[cfg(test)]
mod remote_tests {
    use super::{
        backup_fleet, copy_pod_files, copy_to_pod, cp_timeout, deep_check_fleet, duplicate_names, each_pod,
        handle_backup, handle_baseline, handle_copy, handle_deep_test, handle_full_backup, handle_init_branches, pull_label_and_targets, resolve_pull,
        handle_pods, handle_pull, handle_restore, handle_run, handle_set_branch, local_size, marker_present, probe_gpus,
        proxy_reaches_pod, render_deep_test, render_run, replace_copy_failed, restore_note, restore_notes, run_failure, run_fleet, run_preview, select,
        target_is_pod, BackupTally, Unpushed, Cli, Cmd, CopyLeg, CopyPlan, PodCmd, RestoreOpts, RestoreSource, RunResult, Select, SelectByFlag, Selected,
        Unscoped, BACKUP_TIMEOUT, BRANCH_TIMEOUT, CP_BASE_TIMEOUT, POD_COPY_TIMEOUT, RUN_TIMEOUT_SECS, TEST_TIMEOUT,
    };
    use super::{copy_failure_reason, discarded_note, fallback_lines, mirror_refusal_text, not_pushed_note, sync_time_label, unpushed_note, unredacted_note, Pulled};
    use super::{failure_why, local_backup_dir, push_first_note, take_pod_locks, take_said, telling_line, PodCall};
    use arena_core::selector::{Naming, SelectArgs};
    use arena_core::backup::{backup_command, checkout_command, checkout_command_limited, init_branch_command, BackupConfig, BACKUP_BUSY_EXIT};
    use arena_core::remote::{FakeRemote, FakeReply, Remote, RemoteCall, PROBE_TIMEOUT};
    use arena_core::ssh::{login_shell_wrap, SshTarget};
    use arena_core::{Config, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use clap::Parser;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::Instant;

    /// A provider that lists a fixed fleet (`name()` is `kind`: "runpod" turns on the
    /// replace path's RUNPOD_POD_ID identity checks).
    struct Fleet {
        kind: &'static str,
        pods: Vec<Pod>,
    }

    #[async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            self.kind
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(self.pods.clone())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised")
        }
        async fn stop_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
    }

    /// `devtest-<name>` at `10.0.0.1:<port>` — the FakeRemote host key is `10.0.0.1:<port>`.
    fn pod(name: &str, port: u16) -> Pod {
        Pod {
            id: format!("id-devtest-{name}"),
            name: format!("devtest-{name}"),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: Some("RTX A4000".into()),
            gpu_count: Some(1),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(port),
            ..Default::default()
        }
    }

    fn fleet_of(pods: &[(&str, u16)]) -> Fleet {
        Fleet { kind: "runpod", pods: pods.iter().map(|(n, p)| pod(n, *p)).collect() }
    }

    /// apple / bloom / cloud on ports 22001-22003.
    fn fleet() -> Fleet {
        fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003)])
    }

    fn host(port: u16) -> String {
        format!("10.0.0.1:{port}")
    }

    const REPO: &str = "/root/ARENA_materials";
    const KEY: &str = "/root/.ssh/id_ed25519";

    fn cfg() -> Config {
        Config::parse(&cfg_text())
    }

    fn cfg_text() -> String {
        format!(
            "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
        )
    }

    fn targets(f: &Fleet) -> Vec<(String, SshTarget)> {
        f.pods.iter().map(|p| (p.name.clone(), SshTarget::from_pod(p, &cfg()).unwrap())).collect()
    }

    /// The whole fleet, nothing named (what a bare `pods run` / `pods backup` resolves to).
    fn everyone(f: &Fleet) -> Selected {
        Selected::all(f.pods.clone())
    }

    /// `targets` resolved against `f` by the shared selector, as the command would.
    async fn picked(f: &Fleet, cfg: &Config, targets: &[&str]) -> Selected {
        select(f, cfg, &SelectArgs::exact(targets.iter().copied()), Unscoped::All).await.unwrap()
    }

    /// `pods <args…>` parsed as the CLI would.
    fn pods(args: &[&str]) -> PodCmd {
        let argv = ["arena", "pods"].iter().chain(args).copied();
        match Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd {
            Cmd::Pods(p) => p,
            _ => unreachable!(),
        }
    }

    /// The ports that got any call, sorted.
    fn ports(fake: &FakeRemote) -> Vec<u16> {
        let mut p: Vec<u16> =
            fake.calls().iter().filter_map(|c| c.host().rsplit_once(':').and_then(|(_, p)| p.parse().ok())).collect();
        p.sort();
        p.dedup();
        p
    }

    /// The (cmd, timeout) of every exec made to one host, in order.
    fn execs(fake: &FakeRemote, port: u16) -> Vec<(String, Option<Duration>)> {
        fake.calls_to(&host(port))
            .into_iter()
            .filter_map(|c| match c {
                RemoteCall::Exec { cmd, timeout, .. } => Some((cmd, timeout)),
                RemoteCall::Copy { .. } => None,
            })
            .collect()
    }

    /// `[n/total] rest` → `rest` (finishing order among pods that finish together isn't fixed).
    fn unnumbered(lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| l.split_once("] ").map_or(l.as_str(), |(_, r)| r).to_string()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn pods_run_partial_failure_is_non_zero_with_a_line_per_pod() {
        let script = |fake: &FakeRemote| {
            fake.script(&host(22001), [FakeReply::stdout("bash: no job control in this shell\nhello\n")]);
            fake.script(&host(22002), [FakeReply::exit(127, "zsh: command not found: nvidia-smi")]);
        };
        // Through the command: one wrapped exec per pod with the given budget; non-zero exit.
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let err = handle_run(fake.clone(), &cfg(), &everyone(&fleet()), "nvidia-smi -L", Duration::from_secs(60), false, true, true, false)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed");
        let wrapped = login_shell_wrap("nvidia-smi -L", Some("arena-env"));
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(wrapped.clone(), Some(Duration::from_secs(60)))], "port {port}");
        }

        // The per-pod report (sorted by name), block and compact layouts.
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let results = run_fleet(&remote, targets(&fleet()), &wrapped, Duration::from_secs(60)).await;
        let ok = |name: &str, text: &str| RunResult { name: name.into(), text: text.into(), ok: true, why: None };
        assert_eq!(
            results,
            [
                ok("devtest-apple", "hello"),
                RunResult {
                    name: "devtest-bloom".into(),
                    text: "exit 127: zsh: command not found: nvidia-smi".into(),
                    ok: false,
                    why: Some("exit 127: zsh: command not found: nvidia-smi".into()),
                },
                ok("devtest-cloud", ""),
            ]
        );
        let (lines, bad) = render_run(&results, false);
        assert_eq!(bad, 1);
        assert_eq!(
            lines,
            [
                "\n── devtest-apple ",
                "hello",
                "\n── devtest-bloom (FAILED)",
                "exit 127: zsh: command not found: nvidia-smi",
                "\n── devtest-cloud ",
                "",
                "\n2 ok, 1 failed",
            ]
        );
        let (lines, _) = render_run(&results, true);
        assert_eq!(lines[1], "devtest-bloom          ✗ exit 127: zsh: command not found: nvidia-smi");
    }

    /// Live BPF S9: `pods run` on a bare Hetzner VM (no zsh until setup) said only `exit 127:
    /// bash: line 1: zsh: command not found`. It says the command never ran, and the fix.
    #[tokio::test(start_paused = true)]
    async fn pods_run_on_a_pod_without_zsh_says_to_set_it_up() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(127, "bash: line 1: zsh: command not found")]);
        let remote: Arc<dyn Remote> = fake.clone();
        let wrapped = login_shell_wrap("echo x > ~/unsynced.txt", Some("arena-env"));
        let results = run_fleet(&remote, targets(&fleet()), &wrapped, Duration::from_secs(60)).await;
        let bloom = &results[1];
        assert_eq!(bloom.name, "devtest-bloom");
        assert!(!bloom.ok);
        assert_eq!(
            bloom.why.as_deref(),
            Some(
                "no zsh on the pod (setup installs it) — not set up yet? `arena pods setup devtest-bloom` \
                 (exit 127: bash: line 1: zsh: command not found)"
            )
        );
        assert!(bloom.text.starts_with("no zsh on the pod") && bloom.text.ends_with("exit 127: bash: line 1: zsh: command not found"), "{}", bloom.text);
        assert!(results[0].ok && results[2].ok, "{results:?}");
    }

    /// A failing command's stdout is part of its report (live: `pods run` showed only stderr,
    /// so a script's own output before it failed was lost), and no exit reads `Some(1)`.
    #[test]
    fn a_failed_run_keeps_its_stdout_and_says_exit_n() {
        use arena_core::ssh::SshOutput;
        let out = |code: Option<i32>, stdout: &str, stderr: &str| SshOutput {
            success: false,
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        // (output, full text, compact reason)
        let cases: &[(SshOutput, &str, &str)] = &[
            (out(Some(1), "step 1 ok\nstep 2 ok\n", "Traceback…\nValueError: bad\n"), "step 1 ok\nstep 2 ok\nexit 1: Traceback…\nValueError: bad", "exit 1: ValueError: bad"),
            (out(Some(1), "3 matches so far\n", ""), "3 matches so far\nexit 1", "exit 1 — last output: 3 matches so far"),
            (out(Some(2), "", "usage: x\n"), "exit 2: usage: x", "exit 2: usage: x"),
            (out(None, "", ""), "killed by a signal", "killed by a signal"),
        ];
        for (o, text, why) in cases {
            assert_eq!(run_failure(o), (text.to_string(), why.to_string()), "{o:?}");
            assert!(!text.contains("Some(") && !why.contains("Some("));
        }
        let results = vec![RunResult { name: "devtest-bloom".into(), text: cases[0].1.into(), ok: false, why: Some(cases[0].2.into()) }];
        let (full, bad) = render_run(&results, false);
        assert_eq!((bad, full[1].as_str()), (1, "step 1 ok\nstep 2 ok\nexit 1: Traceback…\nValueError: bad"));
        let (compact, _) = render_run(&results, true);
        assert_eq!(compact[0], "devtest-bloom          ✗ exit 1: ValueError: bad");
    }

    /// A stand-in `rsync` (a shell script in a temp dir) that logs each call's destination —
    /// its last argument — so a pull is observable without touching the network.
    struct StubRsync(std::path::PathBuf);

    impl StubRsync {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("arena-rsync-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("rsync");
            arena_core::remote::write_test_script(
                &script,
                "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
                 echo \"$*\" >> \"$(dirname \"$0\")/args.log\"\n",
            )
            .unwrap();
            StubRsync(dir)
        }
        fn path(&self) -> String {
            self.0.join("rsync").display().to_string()
        }
        /// The destinations so far as `<tier dir>/<pod>`, sorted; the log is emptied.
        fn take(&self) -> Vec<String> {
            let log = self.0.join("calls.log");
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            let mut got: Vec<String> = text
                .lines()
                .map(|l| {
                    let mut parts = l.trim_end_matches('/').rsplit('/');
                    let pod = parts.next().unwrap_or("");
                    format!("{}/{pod}", parts.next().unwrap_or(""))
                })
                .collect();
            got.sort();
            got
        }
        /// Every call's full argv (space-joined), sorted; the log is emptied.
        fn take_args(&self) -> Vec<String> {
            let log = self.0.join("args.log");
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            let mut got: Vec<String> = text.lines().map(String::from).collect();
            got.sort();
            got
        }
    }

    /// What a pod says when its repo is a plain directory at the configured path.
    fn repo_in_place() -> FakeReply {
        FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n"))
    }

    impl Drop for StubRsync {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `pods pull bravo` meant "every pod, into backup/bravo/" (live). Positionals are pods now;
    /// the old label spelling still works where it can't be a pod, and is refused where it
    /// could be either.
    #[test]
    fn pull_takes_pods_and_its_label_behind_a_flag() {
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom w2d2)\n");
        let naming = Naming::from_config(&cfg);
        let listed = vec![pod("bravo", 22001), pod("w9d9", 22002)];
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // (positionals, --label, -t, want: Ok((label, targets, noted)) or Err(tell))
        type Want<'a> = std::result::Result<(Option<&'a str>, &'a [&'a str], bool), &'a str>;
        type Case<'a> = (&'a [&'a str], Option<&'a str>, &'a [&'a str], Want<'a>);
        let cases: &[Case] = &[
            (&["bravo"], None, &[], Ok((None, &["bravo"], false))),
            (&["bravo", "apple..bloom"], Some("w1d3"), &[], Ok((Some("w1d3"), &["bravo", "apple..bloom"], false))),
            (&[], None, &[], Ok((None, &[], false))),
            // The old spelling, where it can only be the label: a note, and it still works.
            (&["w1d3"], None, &[], Ok((Some("w1d3"), &[], true))),
            (&["w1d3"], None, &["apple..bloom"], Ok((Some("w1d3"), &["apple..bloom"], true))),
            (&["w1d3", "bravo"], None, &[], Ok((Some("w1d3"), &["bravo"], true))),
            // With --label every positional is a pod (the selector judges it).
            (&["w1d3"], Some("w2d1"), &[], Ok((Some("w2d1"), &["w1d3"], false))),
            // Not label-shaped: a target — an old custom label fails in the selector, loudly.
            (&["mylabel"], None, &[], Ok((None, &["mylabel"], false))),
            (&["apple,w1d3"], None, &[], Ok((None, &["apple,w1d3"], false))),
            // Either a label or a pod (listed, or a MACHINE_NAME_LIST entry): refused.
            (&["w9d9"], None, &[], Err("is both a snapshot label and a pod's name")),
            (&["devtest-w9d9"], None, &[], Ok((None, &["devtest-w9d9"], false))),
            (&["w2d2"], None, &[], Err("`w2d2` is both")),
            (&["w1d3", "w1d4"], None, &[], Err("two snapshot labels")),
        ];
        for (pos, label, flag, want) in cases {
            let got = pull_label_and_targets(label.map(String::from), &s(pos), &s(flag), &naming, &listed);
            match (got, want) {
                (Ok(g), Ok((l, t, noted))) => {
                    assert_eq!((g.label.as_deref(), g.targets.clone(), g.note.is_some()), (*l, s(t), *noted), "{pos:?} {label:?}");
                    if let Some(n) = g.note {
                        assert!(n.contains("write `--label w1d3`"), "{n}");
                    }
                }
                (Err(e), Err(tell)) => assert!(e.contains(tell), "{pos:?}: {e}"),
                (got, _) => panic!("{pos:?} {label:?}: got {got:?}, want {want:?}"),
            }
        }
    }

    /// Through the command: what each spelling resolves to — the label, every pod it reaches,
    /// the deprecation note — exactly as the `pods pull` arm hands them on (then the dry runs
    /// themselves: nothing copied, no pod asked anything).
    #[tokio::test]
    async fn pods_pull_resolves_pods_and_says_where_a_label_goes() {
        let fake = Arc::new(FakeRemote::new());
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n\
             ARENA_START_DATE=2026-01-05\n"
        ));
        let every: Vec<String> = fleet().pods.iter().map(|p| p.name.clone()).collect();
        assert!(every.len() > 1);
        let resolve = |argv: &'static [&'static str]| {
            let cfg = cfg.clone();
            async move {
                let PodCmd::Pull { label, target_flag, sel, .. } = pods(argv) else { panic!("not pods pull") };
                resolve_pull(&fleet(), &cfg, label, sel, &target_flag)
                    .await
                    .map(|(label, sel, note)| (label, sel.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), note))
            }
        };
        // A pod, positional: that pod, the computed label, no note.
        assert_eq!(resolve(&["pull", "bloom"]).await.unwrap(), (None, vec!["devtest-bloom".to_string()], None));
        // The old spelling — a lone wNdM naming no pod: the label, EVERY pod (as it always meant),
        // and the note saying how to write it now.
        let (label, chosen, note) = resolve(&["pull", "w1d3"]).await.unwrap();
        assert_eq!((label.as_deref(), &chosen), (Some("w1d3"), &every));
        let note = note.expect("the deprecation note");
        assert!(note.contains("reading `w1d3` as the snapshot label") && note.contains("--label w1d3"), "{note}");
        // --label with the old -t: that pod, that label, no note.
        assert_eq!(resolve(&["pull", "-t", "bloom", "--label", "x"]).await.unwrap(), (Some("x".into()), vec!["devtest-bloom".into()], None));
        assert_eq!(resolve(&["pull", "bloom", "--label", "w1d3"]).await.unwrap(), (Some("w1d3".into()), vec!["devtest-bloom".into()], None));
        // A word that is no pod and no label: the error says how the command reads now.
        let msg = format!("{:#}", resolve(&["pull", "mylabel"]).await.unwrap_err());
        assert!(msg.contains("takes pods as its arguments now") && msg.contains("--label") && msg.contains("mylabel"), "{msg}");
        // The w1d3 dry run puts every pod under w1d3/ (the same resolution, through the arm).
        let base = std::env::temp_dir().join(format!("arena-pull-oldlabel-{}", std::process::id()));
        let PodCmd::Pull { label, target_flag, sel, .. } = pods(&["pull", "w1d3"]) else { panic!("not pods pull") };
        let (label, chosen, _) = resolve_pull(&fleet(), &cfg, label, sel, &target_flag).await.unwrap();
        handle_pull(&(fake.clone() as Arc<dyn Remote>), &cfg, label, &base.display().to_string(), None, None, false, false, &chosen, true, true, "rsync")
            .await
            .unwrap();
        assert!(!base.exists(), "a dry run writes nothing");
        handle_pods(pods(&["pull", "w1d3", "--dry-run"]), &fleet(), fake.clone(), &cfg, true).await.unwrap();
        handle_pods(pods(&["pull", "bloom", "--dry-run"]), &fleet(), fake.clone(), &cfg, true).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn pull_and_backup_rsync_exactly_the_selected_pods() {
        let stub = StubRsync::new("scoped");
        let base = stub.0.join("backups");
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\n",
            base.display()
        ));
        let base = base.display().to_string();
        // `pods pull apple..cloud --exclude bloom --label w1d1` — and its old spelling `pods pull
        // w1d1 -t apple..cloud --exclude bloom`: exactly apple + cloud, both tiers, through the
        // same selection the command resolves.
        let listed = fleet().pods.clone();
        let mut chosen_by = Vec::new();
        for argv in [&["pull", "apple..cloud", "--exclude", "bloom", "--label", "w1d1"][..], &["pull", "w1d1", "-t", "apple..cloud", "--exclude", "bloom"]] {
            let PodCmd::Pull { label, target_flag, mut sel, .. } = pods(argv) else { panic!("not pods pull") };
            let split = pull_label_and_targets(label, &sel.targets, &target_flag, &Naming::from_config(&cfg), &listed).unwrap();
            assert_eq!(split.label.as_deref(), Some("w1d1"), "{argv:?}");
            sel.targets = split.targets;
            let chosen = select(&fleet(), &cfg, &sel.args(), Unscoped::All).await.unwrap();
            chosen_by.push(chosen.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>());
        }
        assert_eq!(chosen_by, [["devtest-apple", "devtest-cloud"], ["devtest-apple", "devtest-cloud"]]);
        let chosen = picked(&fleet(), &cfg, &["apple", "cloud"]).await;
        // Each pod is asked where its repo is (a plain directory here: one job per tier).
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22003] {
            fake.script(&host(port), [repo_in_place()]);
        }
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
        assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
        assert_eq!(ports(&fake), [22001, 22003], "only the selected pods were asked");
        // `pods backup bloom` (with the pull): the git push AND the rsync reach bloom only.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::ok(), repo_in_place()]);
        let chosen = picked(&fleet(), &cfg, &["bloom"]).await;
        handle_full_backup(fake.clone(), &cfg, &chosen, false, None, false, true, &stub.path()).await.unwrap();
        assert_eq!(ports(&fake), [22002]);
        let got = stub.take();
        assert_eq!(got.len(), 3, "the snapshot's two passes and big: {got:?}");
        assert!(got.iter().all(|d| d.ends_with("/devtest-bloom")) && got.contains(&"big/devtest-bloom".to_string()), "{got:?}");
        // --no-pull: git only. A dry run: neither.
        let fake = Arc::new(FakeRemote::new());
        handle_full_backup(fake.clone(), &cfg, &chosen, true, None, false, true, &stub.path()).await.unwrap();
        assert_eq!((ports(&fake), stub.take()), (vec![22002], vec![]));
        let fake = Arc::new(FakeRemote::new());
        handle_full_backup(fake.clone(), &cfg, &chosen, false, None, true, true, &stub.path()).await.unwrap();
        assert!(fake.calls().is_empty() && stub.take().is_empty());
    }

    /// Review ai-review claude #10, checked: since FIX4A a pod with no SSH endpoint (stopped,
    /// booting) is in `Pulled::failed` — for the backup's per-pod fallback lines, which ask where
    /// a pod's copy went. Every reader was checked: `pods pull`'s exit and its error come from the
    /// failed rsync jobs (`failures`), where such a pod never is, and its summary counts jobs — not
    /// a failed copy, nor counted twice; `whole` (the resolve's gate on a copy) holds only pods
    /// all of whose rsyncs ran; `pods backup` (pull or `--no-pull`) copies instead only pods its
    /// git step reached, never such a pod, so no line names it a failed copy and the run's verdict
    /// is as without it. (The reaper's backup and its verify run their own rsync, and the archive's
    /// "pull running" reads the pod's lock: neither reads a `Pulled`.)
    #[tokio::test]
    async fn a_pod_with_no_endpoint_is_no_failed_copy_to_any_reader_of_the_pull() {
        use super::{pull_pods, LockMode, PullRun};
        let stub = StubRsync::new("no-endpoint");
        let base = stub.0.join("backups");
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\n",
            base.display()
        ));
        let base = base.display().to_string();
        let stopped = Pod { status: "EXITED".into(), ssh_ip: None, ssh_port: None, ..pod("bloom", 22002) };
        let both = Selected { pods: vec![pod("apple", 22001), stopped.clone()], named: true };
        let args = || PullRun { label: Some("w1d1".into()), max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
        let pull = |sel: Selected| {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [repo_in_place()]);
            let remote: Arc<dyn Remote> = fake;
            let (cfg, base, rsync) = (cfg.clone(), base.clone(), stub.path());
            async move { pull_pods(&remote, &cfg, &base, args(), &sel, &rsync, LockMode::Take(Duration::from_millis(300))).await.unwrap() }
        };
        let no_endpoint = [("devtest-bloom".to_string(), "no SSH endpoint to pull from".to_string())];
        let p = pull(both.clone()).await;
        assert!(p.failures.is_empty(), "no failed rsync: {:?}", p.failures);
        assert_eq!((p.failed.as_slice(), p.whole.as_slice()), (&no_endpoint[..], &["devtest-apple".to_string()][..]));
        assert!(p.copied_to("devtest-apple").is_some() && p.copied_to("devtest-bloom").is_none());
        p.result().unwrap();
        assert_eq!(stub.take().len(), 3, "apple's tiers only");
        // Every pod without one, fleet-wide (named, that's refused before anything: unchanged):
        // the same, from the early return.
        let p = pull(Selected { pods: vec![stopped.clone()], named: false }).await;
        assert!(p.failures.is_empty() && p.whole.is_empty() && stub.take().is_empty());
        assert_eq!(p.failed, no_endpoint);
        p.result().unwrap();
        // `pods pull` itself: exit 0.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake;
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &both, false, true, &stub.path()).await.unwrap();
        assert_eq!(stub.take().len(), 3);
        // `pods backup`, with the pull and without: apple pushed (and pulled), bloom neither, and
        // no line says a copy of bloom failed.
        for no_pull in [false, true] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), repo_in_place()]);
            let _ = take_said();
            handle_full_backup(fake.clone(), &cfg, &both, no_pull, None, false, true, &stub.path()).await.unwrap();
            assert_eq!(stub.take().len(), if no_pull { 0 } else { 3 }, "no_pull={no_pull}");
            assert!(!take_said().iter().any(|l| l.contains("devtest-bloom")), "no_pull={no_pull}");
        }
        let unpushed = [Unpushed { pod: "devtest-bloom".into(), why: "x".into(), branch: None, at_push: true, wrong_origin: false, busy: None }];
        assert_eq!(fallback_lines(&unpushed, &Ok(pull(both.clone()).await)), ["✗ devtest-bloom: git push failed (x); the fallback copy failed too: no SSH endpoint to pull from"]);
        let _ = stub.take();
    }

    /// disk-guard: with too little free on this box (under CONTROL_MIN_FREE_GB), a pod's copy is
    /// skipped whole — before the pod is asked anything or any of its dirs is made — said on its
    /// line, recorded for the daily health check, and a failure of the run: `pods pull`, and the
    /// fallback copy `pods backup --no-pull` makes of a pod whose push failed. A pod with room
    /// (here: onto its earlier copy, a zero floor) is copied as before.
    #[tokio::test]
    async fn a_copy_without_room_on_this_box_is_skipped_whole_and_fails_the_run() {
        let stub = StubRsync::new("noroom");
        let (base, state) = (stub.0.join("backups"), stub.0.join("state"));
        let at_floor = |gb: &str| {
            Config::parse(&format!(
                "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
                 GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
                 LOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB={gb}\n",
                base.display(),
                state.display()
            ))
        };
        let full = at_floor("100000");
        let base_s = base.display().to_string();
        let chosen = picked(&fleet(), &full, &["apple", "bloom"]).await;
        // Each pod's line: why, and what to do.
        let noted = |pods: &[&str]| {
            let said = take_said();
            for pod in pods {
                assert!(
                    said.iter().any(|l| l.starts_with(&format!("✗ {pod}: skipped: only "))
                        && l.contains("(floor 100000 GB, CONTROL_MIN_FREE_GB): this copy (~6 GB, a first copy, not measured) + the floor = 100006 GB needed")
                        && l.contains(" — nothing copied; free at least ")
                        && l.ends_with(" (`arena config check` names arena's biggest dirs there)")),
                    "{pod}: {said:?}"
                );
            }
        };

        // `pods pull`: both skipped; nothing asked, made or rsynced; the run fails naming them.
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let _ = take_said();
        let e = handle_pull(&remote, &full, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap_err();
        noted(&["devtest-apple", "devtest-bloom"]);
        for pod in ["devtest-apple", "devtest-bloom"] {
            assert!(format!("{e:#}").contains(&format!("{pod} (only ")), "{e:#}");
        }
        assert!(!base.exists(), "nothing written: not even the backup dir");
        assert!(stub.take().is_empty() && fake.calls().is_empty(), "no rsync, no pod asked");
        let record = state.join("devtest").join(arena_core::disk::SKIPS_FILE);
        let (skips, _) = arena_core::disk::Skips::load(&record);
        let got: Vec<(&str, &str, u64)> = skips.entries.iter().map(|e| (e.pod.as_str(), e.what.as_str(), e.count)).collect();
        assert_eq!(got, [("devtest-apple", "copy", 1), ("devtest-bloom", "copy", 1)]);
        assert!(skips.entries[0].why.starts_with("only ") && !skips.entries[0].why.contains("nothing copied"), "{:?}", skips.entries[0]);

        // `pods backup --no-pull`, bloom's push failing: its fallback copy is skipped the same
        // way, on its line, and the run fails.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(1, "error: failed to push some refs").with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n")]);
        let bloom = picked(&fleet(), &full, &["bloom"]).await;
        let _ = take_said();
        let e = handle_full_backup(fake.clone(), &full, &bloom, true, None, false, true, &stub.path()).await.unwrap_err();
        noted(&["devtest-bloom"]);
        assert!(format!("{e:#}").contains("devtest-bloom"), "{e:#}");
        assert_eq!(execs(&fake, 22002).len(), 1, "its backup only: no probe for a copy that can't happen");
        assert!(!base.exists() && stub.take().is_empty());
        assert_eq!(arena_core::disk::Skips::load(&record).0.entries.iter().find(|e| e.pod == "devtest-bloom").map(|e| e.count), Some(2));

        // Room: a later copy (bloom copied whole here before) needs ~0.1 GB per tier, the floor 0;
        // a copy that went through whole is marked so.
        for tier in ["w1d1/devtest-bloom", "big/devtest-bloom"] {
            std::fs::create_dir_all(base.join(tier)).unwrap();
            std::fs::write(base.join(tier).join("old.py"), "x").unwrap();
        }
        arena_core::disk::mark_copied(&base, "devtest-bloom").unwrap();
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &at_floor("0"), Some("w1d1".into()), &base_s, None, None, false, false, &bloom, false, true, &stub.path())
            .await
            .unwrap();
        assert_eq!(stub.take(), ["big/devtest-bloom", "w1d1/devtest-bloom", "w1d1/devtest-bloom"]);
        assert!(!arena_core::disk::was_copied(&base, "devtest-apple"), "apple was never copied");
        // Its reservation went with the run.
        let (held, _) = arena_core::disk::Reservations::load(&state.join(arena_core::disk::RESERVATIONS_FILE)).unwrap();
        assert!(held.held.is_empty(), "{held:?}");
    }

    /// disk-guard: a pull that copied a pod's whole home marks it, so its next copy here counts as
    /// a later one; one that didn't (a failed tier, one dir of the home) doesn't. A backup from
    /// before marking gets its pods marked once, before the first judged run's copies.
    #[tokio::test]
    async fn a_pod_copied_whole_is_marked_and_a_failed_one_isnt() {
        let stub = StubRsync::new("marked");
        let base = stub.0.join("backups");
        let text = format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n"
        );
        let cfg = Config::parse(&text);
        let both = picked(&fleet(), &cfg, &["apple", "bloom"]).await;
        let apple = Selected { pods: vec![both.pods[0].clone()], named: true };
        // bloom has no endpoint here: never copied, never marked.
        let both = Selected { pods: vec![both.pods[0].clone(), Pod { ssh_ip: None, ssh_port: None, ..both.pods[1].clone() }], named: true };
        let pull = |cfg: Config, dir: std::path::PathBuf, sel: Selected, remote_path: Option<&'static str>| {
            let stub = stub.path();
            async move {
                let fake = Arc::new(FakeRemote::new());
                fake.script(&host(22001), [repo_in_place()]);
                let remote: Arc<dyn Remote> = fake.clone();
                let dir = dir.display().to_string();
                handle_pull(&remote, &cfg, Some("w1d1".into()), &dir, None, remote_path.map(String::from), false, false, &sel, false, true, &stub).await
            }
        };
        let _ = pull(cfg.clone(), base.clone(), both.clone(), None).await;
        assert!(arena_core::disk::was_copied(&base, "devtest-apple"));
        assert!(!arena_core::disk::was_copied(&base, "devtest-bloom"));
        // One dir of the home isn't the home; `~/` (the cron's BACKUP_REMOTE_PATH) is.
        let part = stub.0.join("part");
        pull(cfg.clone(), part.clone(), apple.clone(), Some("ARENA_materials/results")).await.unwrap();
        assert!(!arena_core::disk::was_copied(&part, "devtest-apple"), "one dir of its home");
        let home = stub.0.join("home");
        pull(Config::parse(&format!("{text}BACKUP_REMOTE_PATH=~/\n")), home.clone(), apple.clone(), None).await.unwrap();
        assert!(arena_core::disk::was_copied(&home, "devtest-apple"), "its home");

        // A backup from before marking: bloom backed up there (both tiers), no marks. The first
        // judged run marks it — its next copy a later one, as before — then copies apple whole.
        let legacy = stub.0.join("legacy");
        for tier in ["w1d1/devtest-bloom", "big/devtest-bloom"] {
            std::fs::create_dir_all(legacy.join(tier)).unwrap();
            std::fs::write(legacy.join(tier).join("old.py"), "x").unwrap();
        }
        let judged = Config::parse(&format!("{text}ARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB=0\n", stub.0.join("state").display()));
        pull(judged, legacy.clone(), apple.clone(), None).await.unwrap();
        assert!(arena_core::disk::was_copied(&legacy, "devtest-bloom"), "marked: backed up before marking");
        assert!(arena_core::disk::was_copied(&legacy, "devtest-apple"), "copied whole");
        assert!(!arena_core::disk::was_copied(&legacy, "devtest-cloud"));
    }

    /// S17 + AD7: every pull rsync writes its temp files to its tier root's `.rsync-tmp`
    /// (`<dir>/<label>/.rsync-tmp`, `<dir>/big/.rsync-tmp`) — beside the pod trees the archive
    /// pushes, never in one — created owner-only once confirmed; a dry run shows it and creates
    /// nothing. A destination on another filesystem (a pod dir that is its own mount) gets no
    /// `--temp-dir` — rsync would rewrite it in place — and says so. Destinations are as before.
    #[tokio::test]
    async fn the_pull_writes_its_temp_files_outside_every_pushed_tree() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let stub = StubRsync::new("tempdir");
        let base = stub.0.join("backups");
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nSHARED_SSH_KEY_PATH=/nonexistent/k\n");
        let chosen = picked(&fleet(), &cfg, &["apple"]).await;
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place(), repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let base_s = base.display().to_string();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, true, true, &stub.path()).await.unwrap();
        assert!(!base.exists(), "a dry run creates nothing");
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
        let canon = std::fs::canonicalize(&base).unwrap();
        let (snap_tmp, big_tmp) = (canon.join("w1d1/.rsync-tmp"), canon.join("big/.rsync-tmp"));
        for tmp in [&snap_tmp, &big_tmp] {
            assert!(tmp.is_dir(), "{}", tmp.display());
            assert_eq!(std::fs::metadata(tmp).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let args = stub.take_args();
        assert_eq!(args.len(), 3, "{args:?}");
        for a in &args {
            let tmp = if a.contains("/big/devtest-apple") { &big_tmp } else { &snap_tmp };
            assert!(a.contains(&format!("--temp-dir={}", tmp.display())), "{a}");
        }
        assert_eq!(stub.take(), ["big/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-apple"], "destinations unchanged");
        assert!(arena_core::archive::sync::sort_labels([".rsync-tmp", "w1d1", "big"]).iter().all(|l| l != ".rsync-tmp"));
        // A pod's big dir on another filesystem (tmpfs): no --temp-dir for it, the rest unchanged.
        let shm = std::path::Path::new("/dev/shm");
        let other = shm.join(format!("arena-tempdir-{}", std::process::id()));
        if std::fs::create_dir_all(&other).is_ok() && std::fs::metadata(shm).unwrap().dev() != std::fs::metadata(&canon).unwrap().dev() {
            std::fs::remove_dir_all(canon.join("big/devtest-apple")).unwrap();
            std::os::unix::fs::symlink(&other, canon.join("big/devtest-apple")).unwrap();
            handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
            let args = stub.take_args();
            let big = args.iter().find(|a| a.contains("/big/devtest-apple")).unwrap();
            assert!(!big.contains("--temp-dir"), "across filesystems rsync would rewrite in place: {big}");
            assert!(args.iter().filter(|a| a.contains("/w1d1/devtest-apple")).all(|a| a.contains("--temp-dir=")), "{args:?}");
            let _ = std::fs::remove_dir_all(&other);
        }
    }

    /// One wedged pod can't hold the backup (or the cron behind it): its rsyncs are stopped
    /// at `BACKUP_TIMEOUT_SECS` and named in the failure — the cron log says which pod —
    /// while every other pod's tiers complete; the stopped rsyncs are really gone, not left
    /// running. A malformed budget fails before anything is copied.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_wedged_pods_rsync_is_stopped_at_its_budget_and_named() {
        let stub = StubRsync::new("wedged");
        // bloom's rsync records its pid and hangs like a wedged pod; the others log and exit.
        arena_core::remote::write_test_script(
            stub.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n  \
             */devtest-bloom/) echo $$ >> \"$(dirname \"$0\")/hung.pids\"; exec sleep 30;;\nesac\n\
             echo \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n",
        )
        .unwrap();
        let base = stub.0.join("backups").display().to_string();
        let with_budget = |budget: &str| {
            Config::parse(&format!(
                "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\n\
                 SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_TIMEOUT_SECS={budget}\n"
            ))
        };
        // 3s: room for the healthy pods' (instant) stubs even on a loaded test machine.
        let cfg = with_budget("3");
        let chosen = everyone(&fleet());
        // Each pod is first asked where its repo is (a plain directory here: one job per tier).
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22002, 22003] {
            fake.script(&host(port), [repo_in_place()]);
        }
        let remote: Arc<dyn Remote> = fake.clone();
        let started = std::time::Instant::now();
        let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(10), "returned at the budget, not when bloom's rsync would end");
        assert!(err.starts_with("2 rsync job(s) failed: "), "{err}");
        for tier in ["snapshot", "big"] {
            assert!(err.contains(&format!("devtest-bloom [{tier}] (timed out after 3s)")), "{err}");
        }
        assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
        // Both of bloom's rsyncs were stopped *before* the failure came back — not by a
        // background task that `main` returning (the runtime dropped) would cut short.
        let pids = std::fs::read_to_string(stub.0.join("hung.pids")).unwrap();
        assert_eq!(pids.lines().count(), 2, "{pids}");
        for pid in pids.lines() {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from));
            assert!(matches!(state.as_deref(), None | Some("Z") | Some("X")), "bloom's rsync {pid} still running ({state:?})");
        }
        // A budget that isn't whole seconds in range: refused up front, nothing copied.
        for bad in ["2h", "0", "86401"] {
            let err = handle_pull(&remote, &with_budget(bad), Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("BACKUP_TIMEOUT_SECS") && err.contains(&format!("`{bad}`")), "{err}");
        }
        assert!(stub.take().is_empty());
        assert_eq!(fake.calls().len(), 3, "a bad budget is refused before any pod is asked anything");
    }

    /// The repo jobs a linked (or unanswered) repo adds to a pull run under the same budget
    /// as the home jobs: a wedged pod — which doesn't answer the repo question either, so its
    /// repo is split out through the configured path — has all four rsyncs stopped and named
    /// (`[snapshot repo]`, `[big repo]` too), and none is left running.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_wedged_pods_repo_rsyncs_are_bounded_too() {
        let stub = StubRsync::new("wedged-repo");
        arena_core::remote::write_test_script(
            stub.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n  \
             */devtest-bloom/|*/devtest-bloom/*) echo $$ >> \"$(dirname \"$0\")/hung.pids\"; exec sleep 30;;\nesac\n\
             echo \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n",
        )
        .unwrap();
        let base = stub.0.join("backups").display().to_string();
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_TIMEOUT_SECS=3\n"
        ));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        fake.script(&host(22002), [FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection timed out")]);
        fake.script(&host(22003), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let started = std::time::Instant::now();
        let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &everyone(&fleet()), false, true, &stub.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(10), "returned at the budget, not when bloom's rsyncs would end");
        assert!(err.starts_with("4 rsync job(s) failed: "), "{err}");
        for tier in ["snapshot", "snapshot repo", "big", "big repo"] {
            assert!(err.contains(&format!("devtest-bloom [{tier}] (timed out after 3s)")), "{err}");
        }
        assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
        let pids = std::fs::read_to_string(stub.0.join("hung.pids")).unwrap();
        assert_eq!(pids.lines().count(), 4, "{pids}");
        for pid in pids.lines() {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from));
            assert!(matches!(state.as_deref(), None | Some("Z") | Some("X")), "bloom's rsync {pid} still running ({state:?})");
        }
    }

    /// A repo that setup linked onto the volume: the pod says so, and the pull leaves the link
    /// out of the home job and pulls the tree from its real path into the same place. A pod
    /// that doesn't answer is pulled through the home path (which follows a link); one whose
    /// repo is a plain directory gets exactly the old single job per tier. A dry run asks
    /// nobody anything.
    #[tokio::test]
    async fn a_pull_follows_a_repo_linked_onto_the_volume() {
        let stub = StubRsync::new("volume");
        let base = stub.0.join("backups").display().to_string();
        let text = format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
        );
        let cfg = Config::parse(&text);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n")]);
        fake.script(&host(22002), [FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
        fake.script(&host(22003), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let all = everyone(&fleet());
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &all, false, true, &stub.path()).await.unwrap();
        let args = stub.take_args();
        let of = |port: u16| -> Vec<&String> { args.iter().filter(|a| a.contains(&format!("-p {port} "))).collect() };
        // apple: home job without the link + the tree from the volume, per tier — the snapshot's
        // jobs each two passes (its files under the cap, then its .git uncapped).
        let apple = of(22001);
        assert_eq!(apple.len(), 6, "{apple:#?}");
        let homes: Vec<&&String> = apple.iter().filter(|a| a.contains("root@10.0.0.1: ")).collect();
        assert_eq!(homes.len(), 3, "{apple:#?}");
        assert!(homes.iter().all(|a| a.contains("--exclude /ARENA_materials ")), "{homes:#?}");
        let trees: Vec<&&String> = apple.iter().filter(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ")).collect();
        assert_eq!(trees.len(), 3, "{apple:#?}");
        assert!(trees.iter().any(|a| a.ends_with("/w1d1/devtest-apple/ARENA_materials/") && a.contains("--max-size=50M")), "{trees:#?}");
        assert!(
            trees.iter().any(|a| a.ends_with("/w1d1/devtest-apple/ARENA_materials/") && !a.contains("--max-size") && a.contains("--include */ --exclude *")),
            "the tree's .git pass: {trees:#?}"
        );
        assert!(trees.iter().any(|a| a.ends_with("/big/devtest-apple/ARENA_materials/") && !a.contains("--max-size")), "{trees:#?}");
        // bloom didn't answer: its repo comes through the home path (trailing slash follows a link).
        let bloom = of(22002);
        assert_eq!(bloom.len(), 6, "{bloom:#?}");
        assert_eq!(bloom.iter().filter(|a| a.contains("root@10.0.0.1:ARENA_materials/ ")).count(), 3, "{bloom:#?}");
        // cloud: a plain directory — one home job per tier, no exclude.
        let cloud = of(22003);
        assert_eq!(cloud.len(), 3, "{cloud:#?}");
        assert!(cloud.iter().all(|a| !a.contains("--exclude /ARENA_materials")), "{cloud:#?}");
        // A dry run: nobody asked, nothing run.
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &all, true, true, &stub.path()).await.unwrap();
        assert!(fake.calls().is_empty() && stub.take_args().is_empty());
        // A --remote-path inside the repo: the pod resolves the link on the way — one job per
        // tier, as given (the pods are still asked: only their answer can say).
        handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, Some("ARENA_materials/results".into()), false, false, &all, false, true, &stub.path())
            .await
            .unwrap();
        assert_eq!(fake.calls().len(), 3);
        assert_eq!(stub.take_args().len(), 9);
        // BACKUP_REMOTE_PATH=~/ (docs/BACKUPS.md's default, what the */15 cron reads) used to skip
        // the question and back up the link alone; it's the home: split like one.
        let linked = || FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n");
        let cron = Config::parse(&format!("{text}BACKUP_REMOTE_PATH=~/\n"));
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22002, 22003] {
            fake.script(&host(port), [linked()]);
        }
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &cron, Some("w1d3".into()), &base, None, None, false, false, &all, false, true, &stub.path()).await.unwrap();
        let args = stub.take_args();
        assert_eq!(args.len(), 18, "home + repo job, two tiers (the snapshot's in two passes), three pods: {args:#?}");
        assert_eq!(args.iter().filter(|a| a.contains("root@10.0.0.1:~/ ") && a.contains("--exclude /ARENA_materials ")).count(), 9, "{args:#?}");
        assert_eq!(args.iter().filter(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ")).count(), 9, "{args:#?}");
        // --remote-path naming the repo without a slash: the tree from its real path, into
        // <dest>/ARENA_materials/ where rsync would have put the link.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [linked()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let one = Selected::all(all.pods.iter().filter(|p| p.name == "devtest-apple").cloned().collect());
        handle_pull(&remote, &cfg, Some("w1d4".into()), &base, None, Some("ARENA_materials".into()), false, true, &one, false, true, &stub.path())
            .await
            .unwrap();
        let args = stub.take_args();
        assert_eq!(args.len(), 2, "{args:#?}");
        assert!(args.iter().all(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ") && a.ends_with("/w1d4/devtest-apple/ARENA_materials/")), "{args:#?}");
    }

    /// A stand-in rsync that also writes into its destination: `image.txt` with an old mtime
    /// (the image's), `work.txt` with a new one — so a snapshot exists afterwards, and a
    /// baseline has something to keep and something to drop. Logs every argv.
    fn writing_rsync(tag: &str) -> StubRsync {
        let stub = StubRsync::new(tag);
        arena_core::remote::write_test_script(
            stub.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$*\" >> \"$(dirname \"$0\")/args.log\"\n\
             mkdir -p \"$last\" && printf img > \"$last/image.txt\" && touch -d @1000000000 \"$last/image.txt\" && \
             printf work > \"$last/work.txt\" && printf carried > \"$last/carried.txt\" && \
             touch -d @1000000000 \"$last/carried.txt\"\n",
        )
        .unwrap();
        stub
    }

    /// The second day's snapshot hard-links to the first (and big never does); both of the
    /// snapshot's passes carry the link-dest.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_snapshot_links_to_the_first() {
        let stub = writing_rsync("linkdest");
        let base = stub.0.join("backups").display().to_string();
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n");
        let one = Selected::all(fleet().pods.into_iter().filter(|p| p.name == "devtest-apple").collect());
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place(), repo_in_place(), repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
        assert!(!stub.take_args().iter().any(|a| a.contains("--link-dest")), "nothing to link to yet");
        handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
        let args = stub.take_args();
        let first = std::fs::canonicalize(stub.0.join("backups/w1d1/devtest-apple")).unwrap();
        let linked: Vec<&String> = args.iter().filter(|a| a.contains(&format!("--link-dest={}", first.display()))).collect();
        assert_eq!(linked.len(), 2, "{args:#?}");
        assert!(linked.iter().all(|a| a.ends_with("/w1d2/devtest-apple/")), "{linked:#?}");
        assert!(args.iter().filter(|a| a.ends_with("/big/devtest-apple/")).all(|a| !a.contains("--link-dest")), "{args:#?}");
        assert!(!args.iter().any(|a| a.contains("--no-perms")), "a new snapshot is a plain -a pull: {args:#?}");
        // The same label again (the */15 cron, later that day): its files may be links into the
        // first snapshot, so their attributes are never changed in place. big isn't linked.
        handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
        let args = stub.take_args();
        let snap: Vec<&String> = args.iter().filter(|a| a.ends_with("/w1d2/devtest-apple/")).collect();
        assert_eq!(snap.len(), 2, "{args:#?}");
        assert!(snap.iter().all(|a| a.contains("--no-perms --no-owner --no-group")), "{snap:#?}");
        assert!(args.iter().filter(|a| a.ends_with("/big/devtest-apple/")).all(|a| !a.contains("--no-perms")), "{args:#?}");
        // A label that is one of the backup dir's own folders is refused.
        for bad in ["big", "baseline", "a/b"] {
            let err = handle_pull(&remote, &cfg, Some(bad.into()), &base, None, None, false, false, &one, true, true, &stub.path()).await.unwrap_err();
            assert!(err.to_string().contains("can't be a snapshot label"), "{bad}: {err}");
        }
    }

    /// `pods baseline` never reads a STOPPED pod: its listed endpoint may answer as another pod
    /// now. Refused before anything is asked of it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_pod_is_never_the_baseline_source() {
        let stub = writing_rsync("baseline-stopped");
        let dir = stub.0.join("backups").display().to_string();
        let mut f = fleet();
        f.pods[0].status = "EXITED".into();
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let err = handle_baseline(&f, &remote, &cfg(), "apple", Some(0), &dir, false, false, true, &stub.path(), 1_791_000_000)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("devtest-apple is stopped (EXITED)") && err.contains("arena pods start devtest-apple"), "{err}");
        assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "{:?}", fake.calls());
    }

    /// `pods baseline`: from a fresh pod only (its container's age known, under a day), into
    /// baseline/w<N>/fleet/ holding only the files older than the container — and none the pod
    /// lists as copied onto it since with an old mtime (a `pods replace` copy: `carried.txt`);
    /// an existing one needs --replace, which moves it aside. A dry run writes nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_baseline_comes_from_a_fresh_pod_and_keeps_only_the_image() {
        let stub = writing_rsync("baseline");
        let base = stub.0.join("backups");
        let dir = base.display().to_string();
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_REPO_PATH={REPO}\n"
        ));
        // Real time: the stub's `work.txt` is written now, after the container's creation.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let probe = |created: Option<u64>| {
            let c = created.map(|t| format!("arena-repo-container={t}\n")).unwrap_or_default();
            FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n{c}"))
        };
        // The pod's listing of what was copied onto it since its container was created.
        let copied = |paths: &[&str]| {
            let records: String = paths.iter().map(|p| format!("./{p}\0")).collect();
            FakeReply::stdout(&format!("arena-copied-begin\0{records}arena-copied-end\0"))
        };
        let run = |fake: &Arc<FakeRemote>, week: u32, replace: bool, dry: bool| {
            let remote: Arc<dyn Remote> = fake.clone();
            let (dir, stub) = (dir.clone(), stub.path());
            let cfg = cfg.clone();
            async move { handle_baseline(&fleet(), &remote, &cfg, "apple", Some(week), &dir, replace, dry, true, &stub, now).await }
        };
        // Refused: no creation time; a container two days old.
        for created in [None, Some(now - 2 * 86_400)] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [probe(created)]);
            let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
            assert!(err.starts_with("refusing the baseline: devtest-apple"), "{err}");
            assert!(stub.take_args().is_empty());
        }
        // Refused: what was copied onto it can't be listed (cut short, or find failed).
        for listing in [FakeReply::stdout("arena-copied-begin\0./carried.txt\0"), FakeReply::exit(1, "find: unknown predicate `-newercm'")] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [probe(Some(now - 600)), listing]);
            let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
            assert!(err.starts_with("refusing the baseline: couldn't list what was copied onto devtest-apple"), "{err}");
            assert!(stub.take_args().is_empty() && !base.join("baseline").exists());
        }
        // A dry run: asked, nothing written.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [probe(Some(now - 600)), copied(&["carried.txt"])]);
        run(&fake, 0, false, true).await.unwrap();
        assert!(stub.take_args().is_empty() && !base.join("baseline").exists());
        // The capture: both passes, then only the image's file stays — not what was written
        // since, nor what was copied in with its old mtime.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [probe(Some(now - 600)), copied(&["carried.txt"])]);
        run(&fake, 0, false, false).await.unwrap();
        assert_eq!(stub.take_args().len(), 2);
        let fleet0 = base.join("baseline/w0/fleet");
        assert!(fleet0.join("image.txt").is_file() && !fleet0.join("work.txt").exists());
        assert!(!fleet0.join("carried.txt").exists(), "copied onto the pod after its container: not the image's");
        let leftovers: Vec<_> = std::fs::read_dir(base.join("baseline/w0")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["fleet"], "no partial dir left");
        // Again: refused without --replace (nothing run); with it, the old one moved aside.
        let fake = Arc::new(FakeRemote::new());
        let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
        assert!(err.contains("already holds week 0's baseline") && err.contains("--replace"), "{err}");
        assert!(fake.calls().is_empty() && stub.take_args().is_empty());
        fake.script(&host(22001), [probe(Some(now - 600)), copied(&[])]);
        run(&fake, 0, true, false).await.unwrap();
        let mut names: Vec<String> =
            std::fs::read_dir(base.join("baseline/w0")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["fleet".to_string(), format!("fleet.replaced-{}", arena_core::restore::stamp(now))]);
        stub.take_args();
        // Week 1's links to week 0's.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [probe(Some(now - 600)), copied(&[])]);
        run(&fake, 1, false, false).await.unwrap();
        let w0 = std::fs::canonicalize(&fleet0).unwrap();
        assert!(stub.take_args().iter().all(|a| a.contains(&format!("--link-dest={}", w0.display()))));
    }

    /// A backups dir with pull's layout for apple: two snapshots and the big tier.
    fn backups(root: &std::path::Path) -> String {
        let put = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        put("backups/w1d1/devtest-apple/notes.txt", "old\n");
        put("backups/w1d2/devtest-apple/ARENA_materials/chapter1/work.py", "mine\n");
        put("backups/w1d2/devtest-apple/notes.txt", "newer\n");
        put("backups/big/devtest-apple/model.bin", "big\n");
        std::fs::create_dir_all(root.join("backups/w1d2/devtest-bloom")).unwrap(); // empty
        root.join("backups").display().to_string()
    }

    fn restore_opts(dir: &str) -> RestoreOpts {
        RestoreOpts {
            from: None,
            path: None,
            dir: dir.into(),
            timeout: Duration::from_secs(60),
            overwrite_newer: false,
            with_git: false,
            dry_run: false,
            yes: true,
        }
    }

    /// 2026-10-08T05:06:40Z — names the kept-files dir.
    const NOW: u64 = 1_791_436_000;

    /// Everything refusable is refused before the pod is touched (no probe, no rsync).
    #[tokio::test]
    async fn restore_refuses_a_missing_or_empty_backup_before_touching_the_pod() {
        let stub = StubRsync::new("restore-refuse");
        let dir = backups(&stub.0);
        let mut f = fleet();
        f.pods.push(Pod { id: "id-devtest-dune".into(), name: "devtest-dune".into(), status: "RUNNING".into(), ..Default::default() });
        std::fs::create_dir_all(stub.0.join("backups/w1d2/devtest-dune")).unwrap();
        std::fs::write(stub.0.join("backups/w1d2/devtest-dune/x"), "x").unwrap();
        let cases: &[(&str, RestoreOpts, &str)] = &[
            ("apple", restore_opts(&stub.0.join("nowhere").display().to_string()), "doesn't exist"),
            ("bloom", restore_opts(&dir), "no wNdM snapshot of devtest-bloom"),
            ("apple", RestoreOpts { from: Some("w9d9".into()), ..restore_opts(&dir) }, "backups of devtest-apple: w1d2, w1d1, big"),
            ("apple", RestoreOpts { from: Some("../w1d1".into()), ..restore_opts(&dir) }, "isn't a backup label"),
            ("apple", RestoreOpts { path: Some("../etc".into()), ..restore_opts(&dir) }, "must be a relative path"),
            ("apple", RestoreOpts { path: Some("ARENA_materials/chapter9".into()), ..restore_opts(&dir) }, "isn't in that backup"),
            ("dune", restore_opts(&dir), "no SSH endpoint"),
            ("zebra", restore_opts(&dir), "zebra"),
        ];
        for (pod, opts, says) in cases {
            let fake = Arc::new(FakeRemote::new());
            let remote: Arc<dyn Remote> = fake.clone();
            let e = handle_restore(&f, &remote, &cfg(), pod, opts.clone(), &stub.path(), NOW).await.unwrap_err();
            assert!(format!("{e:#}").contains(says), "{pod} {opts:?}: {e:#}");
            assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "{pod} {opts:?}");
        }
    }

    /// A STOPPED pod is never restored onto, dry run included: the listing may still show its
    /// last endpoint, which belongs to nothing now — whatever answers there could be another
    /// pod, and the push would put this pod's backup into its home. Refused before any probe.
    #[tokio::test]
    async fn a_stopped_pod_is_never_restored_onto() {
        let stub = StubRsync::new("restore-stopped");
        let dir = backups(&stub.0);
        let mut f = fleet();
        f.pods[0].status = "EXITED".into(); // its last endpoint still listed
        for dry_run in [false, true] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/root/ARENA_materials\n")]);
            let remote: Arc<dyn Remote> = fake.clone();
            let opts = RestoreOpts { dry_run, ..restore_opts(&dir) };
            let e = handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap_err().to_string();
            assert!(e.contains("devtest-apple: it is stopped (EXITED)") && e.contains("`arena pods start devtest-apple`"), "{e}");
            assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "dry run {dry_run}: {:?}", fake.calls());
        }
    }

    #[tokio::test]
    async fn restore_pushes_the_newest_snapshot_through_the_safe_argv() {
        let stub = StubRsync::new("restore-run");
        let dir = backups(&stub.0);
        let f = fleet();
        let linked = "arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n";
        // Default: the newest snapshot (w1d2), the whole of it, into apple's home.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(linked)]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &stub.path(), NOW).await.unwrap();
        let args = stub.take_args();
        assert_eq!(args.len(), 1, "{args:?}");
        let a = &args[0];
        assert!(a.ends_with(&format!("{dir}/w1d2/devtest-apple/ root@10.0.0.1:")), "{a}");
        // The volume is mounted there: what it replaces is kept ON it; newer files and .git
        // (a snapshot) stay as they are.
        for flag in [
            "--keep-dirlinks",
            "--no-owner",
            "--no-group",
            "--chmod=go-w",
            "--backup-dir=/workspace/.arena-restore/20261008T050640Z",
            "--update",
            "--exclude .git/",
            "--timeout=300",
            "-p 22001",
        ] {
            assert!(a.contains(flag), "missing {flag}: {a}");
        }
        assert!(!a.contains("--delete") && !a.contains("--force"), "{a}");
        // The pod was asked (read-only) where its repo is — once, before the rsync.
        assert!(matches!(&fake.calls()[..], [RemoteCall::Exec { cmd, timeout, .. }] if cmd.contains("arena-repo-real") && *timeout == Some(PROBE_TIMEOUT)));
        // No volume (or no answer): kept in the home; --overwrite-newer / --with-git as asked.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/root/ARENA_materials\narena-repo-mount=no\n")]);
        let remote2: Arc<dyn Remote> = fake.clone();
        let opts = RestoreOpts { overwrite_newer: true, with_git: true, ..restore_opts(&dir) };
        handle_restore(&f, &remote2, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
        let a = stub.take_args().remove(0);
        assert!(a.contains("--backup-dir=.arena-restore/20261008T050640Z") && !a.contains("--update") && !a.contains(".git/"), "{a}");
        // --from big, only a path: the all-files tier, `--relative` from its root.
        let opts = RestoreOpts { from: Some("big".into()), path: Some("model.bin".into()), ..restore_opts(&dir) };
        handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
        let a = stub.take_args().remove(0);
        assert!(a.contains("--relative") && a.ends_with(&format!("{dir}/big/devtest-apple/./model.bin root@10.0.0.1:")), "{a}");
        // A dry run copies nothing (the read-only question is still asked, for the plan).
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let opts = RestoreOpts { dry_run: true, yes: false, ..restore_opts(&dir) };
        handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
        assert!(stub.take_args().is_empty());
        assert_eq!(fake.calls().len(), 1);
        // …and with a pod whose disk was reset after the snapshot was written (the plan warns;
        // nothing else changes).
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(&format!("{linked}arena-repo-container=1\n"))]);
        let remote: Arc<dyn Remote> = fake.clone();
        let opts = RestoreOpts { dry_run: true, yes: false, ..restore_opts(&dir) };
        handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
        assert!(stub.take_args().is_empty());
    }

    /// A restore that pushed the repo's `.git` (the big tier) checks the repo afterwards: a
    /// repo git can't read any more fails the command, saying where the replaced files are.
    #[tokio::test]
    async fn a_restore_that_pushed_git_checks_the_repo() {
        let stub = StubRsync::new("restore-git");
        let dir = backups(&stub.0);
        std::fs::create_dir_all(stub.0.join("backups/big/devtest-apple/ARENA_materials/.git")).unwrap();
        std::fs::write(stub.0.join("backups/big/devtest-apple/ARENA_materials/.git/HEAD"), "ref\n").unwrap();
        let f = fleet();
        let linked = "arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n";
        let big = || RestoreOpts { from: Some("big".into()), ..restore_opts(&dir) };
        // Reads fine: done.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(linked), FakeReply::stdout("arena-git=ok\n")]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_restore(&f, &remote, &cfg(), "apple", big(), &stub.path(), NOW).await.unwrap();
        let calls = fake.calls();
        assert!(
            matches!(&calls[..], [_, RemoteCall::Exec { cmd, timeout, .. }] if cmd.contains("fsck --connectivity-only") && *timeout == Some(arena_core::restore::GIT_CHECK_TIMEOUT)),
            "{calls:?}"
        );
        assert!(!stub.take_args()[0].contains(".git/"), "the big tier carries .git");
        // Broken: the command fails and says where the replaced .git files are.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(linked), FakeReply::stdout("arena-git=broken\n")]);
        let remote: Arc<dyn Remote> = fake.clone();
        let e = handle_restore(&f, &remote, &cfg(), "apple", big(), &stub.path(), NOW).await.unwrap_err().to_string();
        assert!(e.contains("git can't read") && e.contains("/workspace/.arena-restore/20261008T050640Z/"), "{e}");
        // A snapshot without --with-git pushes no .git: nothing to check.
        std::fs::create_dir_all(stub.0.join("backups/w1d2/devtest-apple/ARENA_materials/.git")).unwrap();
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout(linked)]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &stub.path(), NOW).await.unwrap();
        assert_eq!(fake.calls().len(), 1, "only the probe");
        stub.take_args();
    }

    #[tokio::test]
    async fn a_failed_or_stuck_restore_says_nothing_was_deleted() {
        let stub = StubRsync::new("restore-fail");
        let dir = backups(&stub.0);
        let f = fleet();
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let script = |name: &str, body: &str| {
            let p = stub.0.join(name);
            arena_core::remote::write_test_script(&p, body).unwrap();
            p.display().to_string()
        };
        let failing = script("rsync-fails", "#!/bin/sh\necho 'rsync: connection unexpectedly closed' >&2\nexit 12\n");
        let e = handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &failing, NOW).await.unwrap_err().to_string();
        assert!(e.contains("exit 12") && e.contains("connection unexpectedly closed") && e.contains("nothing on the pod was deleted"), "{e}");
        // Stuck: stopped at its budget, reported as such.
        let stuck = script("rsync-stuck", "#!/bin/sh\nexec sleep 30\n");
        let opts = RestoreOpts { timeout: Duration::from_millis(500), ..restore_opts(&dir) };
        let started = std::time::Instant::now();
        let e = handle_restore(&f, &remote, &cfg(), "apple", opts, &stuck, NOW).await.unwrap_err().to_string();
        assert!(e.contains("timed out after 500ms") && e.contains("nothing on the pod was deleted"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn restore_says_where_the_repo_lands() {
        let root = std::env::temp_dir().join(format!("arena-restore-note-{}", std::process::id()));
        let src = root.join("w1d2/devtest-apple");
        std::fs::create_dir_all(src.join("ARENA_materials/chapter1")).unwrap();
        let probe = |real: &str, mounted: bool| arena_core::volume::RepoProbe {
            home: Some("/root".into()),
            real: Some(real.into()),
            volume_mounted: mounted,
            ..Default::default()
        };
        let note = |p: Option<&arena_core::volume::RepoProbe>, sub: Option<&str>| restore_note(REPO, "root", p, &src, sub);
        let linked = probe("/workspace/ARENA_materials", true);
        assert_eq!(note(Some(&linked), None).unwrap(), "(ARENA_materials/ lands in /workspace/ARENA_materials — the volume copy, through the pod's link)");
        assert!(note(Some(&linked), Some("ARENA_materials/chapter1")).is_some());
        assert_eq!(note(Some(&linked), Some("notes.txt")), None, "the repo isn't part of it");
        assert!(note(Some(&probe(REPO, true)), None).unwrap().contains("`arena pods setup` first puts it there"));
        assert_eq!(note(Some(&probe(REPO, false)), None), None, "no volume: nothing to say");
        assert!(note(None, None).unwrap().contains("couldn't ask the pod"));
        // A backup without the repo: nothing to say either.
        assert_eq!(restore_note(REPO, "root", Some(&linked), &root, None), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The plan's notes: a snapshot written after the pod's disk was reset is flagged, with the
    /// newest snapshot from before that; a snapshot's .git staying behind is said.
    #[test]
    fn restore_warns_about_a_snapshot_written_after_the_reset() {
        let root = std::env::temp_dir().join(format!("arena-restore-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, rel).unwrap();
        };
        put("w1d1/devtest-apple/ARENA_materials/.git/HEAD");
        put("w1d1/devtest-apple/ARENA_materials/work.py");
        let secs = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // The pod's disk is reset after w1d1 was written; the */15 pull then writes w1d2.
        std::thread::sleep(Duration::from_millis(1100));
        let reset = secs();
        std::thread::sleep(Duration::from_millis(1100));
        put("w1d2/devtest-apple/ARENA_materials/.git/HEAD");
        put("w1d2/devtest-apple/ARENA_materials/work.py");
        let src = root.join("w1d2/devtest-apple");
        let written = arena_core::restore::tree_stats(&src).unwrap().last_written;
        let probe = arena_core::volume::RepoProbe {
            home: Some("/root".into()),
            real: Some("/workspace/ARENA_materials".into()),
            volume_mounted: true,
            container_created: Some(reset),
        };
        let b = RestoreSource { base: &root, pod: "devtest-apple", label: "w1d2", src: &src, sub: None, last_written: written };
        let notes = restore_notes(REPO, "root", Some(&probe), &b, false);
        assert!(notes.iter().any(|n| n.starts_with("⚠ w1d2 was last written") && n.contains("--from w1d1")), "{notes:#?}");
        assert!(notes.iter().any(|n| n.contains(".git isn't restored")), "{notes:#?}");
        // The pod's container is older than every backup: nothing to flag; .git pushed: not said.
        let old = arena_core::volume::RepoProbe { container_created: Some(1), ..probe.clone() };
        let notes = restore_notes(REPO, "root", Some(&old), &b, true);
        assert!(!notes.iter().any(|n| n.starts_with('⚠') || n.contains(".git")), "{notes:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_parses() {
        match pods(&["restore", "apple", "--from", "big", "--path", "ARENA_materials", "--dir", "/b", "--timeout", "60", "--dry-run"]) {
            PodCmd::Restore { target, from, path, dir, timeout, overwrite_newer, with_git, dry_run } => {
                assert_eq!((target.as_str(), from.as_deref(), path.as_deref(), dir.as_deref(), timeout, dry_run), ("apple", Some("big"), Some("ARENA_materials"), Some("/b"), Some(60), true));
                assert!(!overwrite_newer && !with_git, "both off by default");
            }
            _ => panic!("not restore"),
        }
        match pods(&["restore", "apple", "--overwrite-newer", "--with-git"]) {
            PodCmd::Restore { overwrite_newer, with_git, .. } => assert!(overwrite_newer && with_git),
            _ => panic!("not restore"),
        }
        assert!(Cli::try_parse_from(["arena", "pods", "restore", "apple", "--timeout", "0"]).is_err());
        assert!(Cli::try_parse_from(["arena", "pods", "restore"]).is_err(), "one pod, named");
    }

    #[test]
    fn run_previews_name_every_resolved_pod() {
        let names = ["devtest-apple", "devtest-cloud"];
        assert_eq!(
            run_preview(true, "hostname", "<wrapped>", &names, Duration::from_secs(60)),
            "[dry-run] would run on 2 pod(s) (60s budget each): devtest-apple, devtest-cloud\n  <wrapped>"
        );
        assert_eq!(
            run_preview(false, "hostname", "<wrapped>", &names, Duration::from_secs(60)),
            "Run `hostname` on 2 pod(s) over SSH: devtest-apple, devtest-cloud"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ssh_fleet_commands_reach_exactly_the_selected_pods() {
        // PLAN 1.C through each SSH command family: the shared selector decides which pods
        // are reached — names, ranges, --exclude, `-t` for `run` — and nothing else is.
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
        ));
        let cases: &[(&[&str], &[u16])] = &[
            (&["run", "-t", "apple,cloud", "true"], &[22001, 22003]),
            (&["run", "-t", "bloom..cloud", "--", "true"], &[22002, 22003]),
            (&["run", "--exclude", "bloom", "true"], &[22001, 22003]),
            (&["test", "bloom"], &[22002]),
            (&["test", "--deep", "apple..bloom"], &[22001, 22002]),
            (&["backup", "apple", "--no-pull"], &[22001]),
            (&["backup", "--exclude", "apple", "--no-pull"], &[22002, 22003]),
            (&["init-branches", "cloud", "--week", "1", "--day", "2"], &[22003]),
            (&["set-branch", "main", "apple", "cloud"], &[22001, 22003]),
            (&["set-branch", "main", "--all", "--exclude", "bloom..cloud"], &[22001]),
        ];
        for (argv, want) in cases {
            let fake = Arc::new(FakeRemote::new());
            // `test --deep` on an empty reply FAILs its pods; only who was reached matters here.
            let _ = handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await;
            assert_eq!(ports(&fake), *want, "{argv:?}");
        }
        // Typos, a reversed range, a bad --on and a missing scope reach nobody.
        for argv in [
            &["run", "-t", "aple", "true"][..],
            &["backup", "--exclude", "clod", "--no-pull"],
            &["test", "--deep", "cloud..apple"],
            &["init-branches", "--on", "aws"],
            &["set-branch", "main"],
        ] {
            let fake = Arc::new(FakeRemote::new());
            assert!(handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await.is_err(), "{argv:?}");
            assert!(fake.calls().is_empty(), "{argv:?}");
        }
        // An empty argument (a script's unset `$POD`) is an error — never "every pod".
        for argv in [
            &["run", "-t", "", "true"][..],
            &["run", "-t", ",", "true"],
            &["run", "--include", " ", "true"],
            &["setup", ""],
            &["setup", "--force", "--include", ""],
            &["copy-keys", ""],
            &["copy-keys", "--include", ""],
            &["cp", "/nonexistent/f", "-t", ""],
            &["test", ""],
            &["test", "--deep", " , "],
            &["backup", "", "--no-pull"],
            &["init-branches", ""],
            &["set-branch", "main", "--all", "--exclude", ""],
        ] {
            let fake = Arc::new(FakeRemote::new());
            let e = handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await.unwrap_err().to_string();
            assert!(e.contains("an empty target ("), "{argv:?}: {e}");
            assert!(fake.calls().is_empty(), "{argv:?}");
        }
        // A named pod without an endpoint fails the command; swept in by default, it's skipped.
        let mut f = fleet();
        f.pods[0].ssh_port = None;
        let fake = Arc::new(FakeRemote::new());
        let err = handle_pods(pods(&["run", "-t", "apple", "true"]), &f, fake.clone(), &cfg, true).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "none of the selected pods (devtest-apple) has an SSH endpoint yet — check `arena pods list`"
        );
        handle_pods(pods(&["run", "true"]), &f, fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), [22002, 22003]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_pod_times_out_while_the_others_finish() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::hang()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let start = Instant::now();
        let results = run_fleet(&remote, targets(&fleet()), "true", Duration::from_secs(90)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(90), "ends at the budget, not the hang");
        let failed: Vec<(&str, &str)> = results.iter().filter(|r| !r.ok).map(|r| (r.name.as_str(), r.text.as_str())).collect();
        assert_eq!(failed, [("devtest-bloom", "timed out after 90s")]);
        let (lines, bad) = render_run(&results, true);
        assert_eq!((bad, lines[1].as_str()), (1, "devtest-bloom          ✗ timed out after 90s"));
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_and_pods_run_carry_their_budgets_through_the_command() {
        // `pods test`: 90s per pod; a wedged pod fails the command at that budget.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22003), [FakeReply::hang()]);
        let start = Instant::now();
        let plain = PodCmd::Test { deep: false, sel: Select::default(), json: false, verbose: false, cohort: crate::CohortScope::default() };
        let err = handle_pods(plain, &fleet(), fake.clone(), &cfg(), true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed");
        assert_eq!(start.elapsed(), TEST_TIMEOUT);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, timeout, .. }
            if cmd.contains("import torch") && *timeout == Some(TEST_TIMEOUT))));

        // `pods run --timeout 45`.
        let fake = Arc::new(FakeRemote::new());
        let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["nvidia-smi".into()], timeout: 45, dry_run: false, background: false };
        handle_pods(run, &fleet(), fake.clone(), &cfg(), true).await.unwrap();
        assert_eq!(fake.calls().len(), 3);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(Duration::from_secs(45)))));

        // A dry run reaches no pod.
        let fake = Arc::new(FakeRemote::new());
        let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["reboot".into()], timeout: 45, dry_run: true, background: false };
        handle_pods(run, &fleet(), fake.clone(), &cfg(), true).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn run_timeout_flag_goes_before_the_command_and_defaults_to_30_min() {
        let run = |args: &[&str]| match Cli::try_parse_from(args).map(|c| c.cmd) {
            Ok(Cmd::Pods(PodCmd::Run { command, timeout, .. })) => Ok((command, timeout)),
            Ok(_) => panic!("not a run"),
            Err(e) => Err(e),
        };
        assert_eq!(RUN_TIMEOUT_SECS, 1800);
        assert_eq!(run(&["arena", "pods", "run", "nvidia-smi", "-L"]).unwrap(), (vec!["nvidia-smi".into(), "-L".into()], 1800));
        assert_eq!(
            run(&["arena", "pods", "run", "--timeout", "60", "nvidia-smi", "-L"]).unwrap(),
            (vec!["nvidia-smi".into(), "-L".into()], 60)
        );
        // After the command, it's part of the command (like every other flag).
        assert_eq!(run(&["arena", "pods", "run", "echo", "--timeout", "5"]).unwrap().0, ["echo", "--timeout", "5"]);
        // Bounded: no zero, no "forever" spelled as a huge number.
        for bad in ["0", "86401"] {
            assert!(run(&["arena", "pods", "run", "--timeout", bad, "true"]).is_err(), "{bad}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn backup_classifies_each_pods_reply_and_a_wedged_pod_fails_at_the_budget() {
        let f = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004), ("echo", 22005)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("[autocommit-x 1a2b3c] arena backup\nPUSHED autocommit-x\n")]);
        fake.script(&host(22002), [FakeReply::hang()]);
        fake.script(&host(22003), [FakeReply::stdout("NO_CHANGES feature-y\n")]);
        fake.script(&host(22004), [FakeReply::stdout("SKIP main\n")]);
        fake.script(&host(22005), [FakeReply::exit(128, "fatal: Could not read from remote repository.")]);
        let remote: Arc<dyn Remote> = fake.clone();
        let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
        let mut lines = Vec::new();
        let start = Instant::now();
        let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
        let on_main = vec![("devtest-delta".to_string(), "main".to_string())];
        let failed = |pod: &str, why: &str| Unpushed { pod: pod.into(), why: why.into(), branch: None, at_push: false, wrong_origin: false, busy: None };
        assert_eq!(
            tally,
            BackupTally {
                pushed: 1,
                unchanged: 1,
                skipped: 1,
                failed: vec!["devtest-echo".to_string(), "devtest-bloom".to_string()],
                unpushed: vec![
                    failed("devtest-echo", "exit 128: fatal: Could not read from remote repository."),
                    failed("devtest-bloom", "timed out after 300s"),
                ],
                on_protected: on_main,
                uncommitted: vec![],
            }
        );
        assert_eq!(start.elapsed(), BACKUP_TIMEOUT);
        // The wedged pod reports last, after its budget; the rest as soon as they answered. The
        // pod on main has no line of its own: it's said once, after the run.
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[3], "[5/5] ✗ devtest-bloom: timed out after 300s");
        let mut first: Vec<String> = unnumbered(&lines[..3]);
        first.sort();
        assert_eq!(
            first,
            [
                "= devtest-cloud (no changes, on feature-y)",
                "✓ devtest-apple -> autocommit-x",
                "✗ devtest-echo (exit 128): fatal: Could not read from remote repository.",
            ]
        );

        // Through the command: each pod's own commit message, the budget, a non-zero result.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(1, "boom")]);
        let err = handle_backup(fake.clone(), &cfg(), true, None, &everyone(&fleet()), &mut crate::PodLocks::default(), &mut Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let want = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
            assert_eq!(execs(&fake, port), [(want, Some(BACKUP_TIMEOUT))]);
        }
        // Dry run: nothing reaches a pod.
        let fake = Arc::new(FakeRemote::new());
        handle_backup(fake.clone(), &cfg(), false, None, &everyone(&fleet()), &mut crate::PodLocks::default(), &mut Vec::new())
            .await
            .unwrap();
        assert!(fake.calls().is_empty());
    }

    /// RR3: `pods backup`'s git push releases each pod's backup lock the moment that pod's push
    /// ends — a wedged pod (here: one that hangs to the budget) keeps only its own.
    #[tokio::test(start_paused = true)]
    async fn rr3_backup_releases_each_pods_lock_as_its_push_ends() {
        let dir = std::env::temp_dir().join(format!("arena-rr3-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("NO_CHANGES feature-y\n")]);
        fake.script(&host(22002), [FakeReply::hang()]);
        let names: Vec<String> = f.pods.iter().map(|p| p.name.clone()).collect();
        let mut locks = crate::take_pod_locks(&d, &names, Duration::from_millis(10)).await;
        assert!(locks.holds("devtest-apple") && locks.holds("devtest-bloom"));
        let held = |n: &str| arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&d, n).unwrap()).unwrap().is_err();
        let free = |n: &str| crate::pods::lock::lock_settles_free(arena_core::pull::open_pod_lock(&d, n).unwrap());
        let (sel, c) = (everyone(&f), cfg());
        let mut unpushed = Vec::new();
        let backup = handle_backup(fake.clone(), &c, true, None, &sel, &mut locks, &mut unpushed);
        let check = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            (free("devtest-apple"), !held("devtest-bloom"))
        };
        let (_, (apple, bloom)) = tokio::join!(backup, check);
        assert!(apple, "apple's push ended, yet its lock was still held");
        assert!(!bloom, "bloom's push was still running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config whose backup dir is `base` — created, so the per-pod locks are really taken
    /// (`take_pod_locks` takes none in a dir that doesn't exist) — with a label to use.
    fn backup_cfg(base: &std::path::Path) -> Config {
        std::fs::create_dir_all(base).unwrap();
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\n",
            base.display()
        ))
    }

    /// A backup GitHub refuses over a secret the redaction doesn't know (exit 4).
    fn push_blocked(branch: &str) -> FakeReply {
        FakeReply::exit(4, "remote: error: GH013: Repository rule violations found")
            .with_stdout(&format!("BRANCH {branch}\nPUSH_BLOCKED OpenRouter API Key at ex.py:3\n"))
    }

    /// Live: GitHub refused three pods' pushes over a pasted key for ~20 hours, and the prod
    /// cron is `pods backup --no-pull` — so those pods had no copy anywhere but on themselves.
    /// Now every pod whose push fails, whatever the reason (a key, the network), is rsynced in
    /// the same run — both tiers, under its own lock — even under --no-pull; a pod whose push
    /// worked isn't; and the run still fails (a copy is no git backup). A copy that fails too
    /// is in the error beside the push's failure. Without --no-pull the pull covers every pod
    /// once — the failed ones aren't copied twice.
    #[tokio::test]
    async fn a_failed_push_is_copied_in_the_same_run_even_under_no_pull_and_the_run_still_fails() {
        let stub = StubRsync::new("fallback");
        let cfg = backup_cfg(&stub.0.join("backups"));
        let four = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
        fake.script(
            &host(22003),
            [FakeReply::exit(128, "fatal: Could not read from remote repository.").with_stdout("BRANCH autocommit-c\nPUSH_FAILED\n"), repo_in_place()],
        );
        // Review: a pod that fails before its push (here: no repo) fails the same way every run —
        // never rsynced every 15 minutes.
        fake.script(&host(22004), [FakeReply::exit(2, "sh: 1: cd: can't cd to /root/ARENA_materials")]);
        let err = handle_full_backup(fake.clone(), &cfg, &everyone(&four), true, None, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "3 pod(s) failed to back up");
        assert_eq!(fake.calls_to(&host(22004)).len(), 1, "no repo: not even asked where its repo is");
        let got = stub.take();
        for pod in ["devtest-bloom", "devtest-cloud"] {
            let theirs: Vec<&String> = got.iter().filter(|d| d.ends_with(&format!("/{pod}"))).collect();
            assert_eq!(theirs.len(), 3, "{pod}: the snapshot's two passes and big: {got:?}");
            assert!(got.contains(&format!("big/{pod}")), "{got:?}");
        }
        assert_eq!(got.len(), 6, "never the pod whose push worked, nor the one with no repo: {got:?}");
        // Each failed pod: its backup, then the copy's question where its repo is.
        assert_eq!(fake.calls_to(&host(22001)).len(), 1);
        let bloom = execs(&fake, 22002);
        assert_eq!(bloom[1], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)));

        // The copy fails too: both in the error (and on the pod's line, below).
        let failing = StubRsync::new("fallback-fail");
        arena_core::remote::write_test_script(
            failing.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             case \"$last\" in *devtest-bloom*) echo 'rsync: connection unexpectedly closed' >&2; exit 12;; esac\n",
        )
        .unwrap();
        let cfg = backup_cfg(&failing.0.join("backups"));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
        let err = handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &failing.path()).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("1 pod(s) failed to back up; 2 rsync job(s) failed: devtest-bloom ["), "{msg}");
        assert!(msg.contains("devtest-bloom [snapshot]") && msg.contains("devtest-bloom [big]"), "{msg}");

        // Without --no-pull: every pod pulled once.
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22003] {
            fake.script(&host(port), [FakeReply::stdout("NO_CHANGES autocommit-x\n"), repo_in_place()]);
        }
        fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
        let err = handle_full_backup(fake.clone(), &cfg_at(&stub), &everyone(&fleet()), false, None, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        let got = stub.take();
        assert_eq!(got.len(), 9, "{got:?}");
        assert_eq!(got.iter().filter(|d| d.ends_with("/devtest-bloom")).count(), 3, "{got:?}");
    }

    fn cfg_at(stub: &StubRsync) -> Config {
        backup_cfg(&stub.0.join("backups"))
    }

    /// Live (bsec B2): a push to a missing repo said `git push failed (exit 1: and the repository
    /// exists.)` — the last line of git's advice. The "why" (the per-pod line, set-branch's
    /// `NOT PUSHED FIRST (…)`) is now the first verdict line git or GitHub wrote, else the last
    /// non-empty one. Pure.
    #[test]
    fn a_failed_push_says_why_by_its_verdict_line_not_its_last() {
        let missing = "ERROR: Repository not found.\nfatal: Could not read from remote repository.\n\n\
                       Please make sure you have the correct access rights\nand the repository exists.\n";
        let cases: &[(&str, Option<&str>)] = &[
            (missing, Some("ERROR: Repository not found.")),
            ("ssh: connect to host github.com port 22: Connection timed out\r\nfatal: Could not read from remote repository.\r\n\r\nand the repository exists.\r\n", Some("ssh: connect to host github.com port 22: Connection timed out")),
            // Live (hz2): the deploy key moved away — ssh's line names the cause git's `fatal:` hides.
            ("Warning: Identity file /root/.ssh/id_ed25519 not accessible: No such file or directory.\nno such identity: /root/.ssh/id_ed25519: No such file or directory\ngit@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n", Some("git@github.com: Permission denied (publickey).")),
            ("To github.com:o/r.git\n ! [rejected]        autocommit-x -> autocommit-x (non-fast-forward)\nerror: failed to push some refs to 'github.com:o/r.git'\nhint: Updates were rejected\n", Some("! [rejected]        autocommit-x -> autocommit-x (non-fast-forward)")),
            ("remote: Resolving deltas: 100% (3/3)\nremote: error: File big.bin is 120.00 MB; this exceeds GitHub's file size limit\nTo github.com:o/r.git\n ! [remote rejected] x -> x (pre-receive hook declined)\nerror: failed to push some refs\n", Some("remote: error: File big.bin is 120.00 MB; this exceeds GitHub's file size limit")),
            ("To /root/gh013.git\n ! [remote rejected] x -> x (pre-receive hook declined)\n", Some("! [remote rejected] x -> x (pre-receive hook declined)")),
            ("error: src refspec x does not match any\n", Some("error: src refspec x does not match any")),
            // A harmless `Permission denied` earlier on (git add, a mount it can't read) isn't the push's.
            ("warning: could not open directory 'data/mnt/': Permission denied\nTo github.com:o/r.git\n ! [rejected]        x -> x (non-fast-forward)\n", Some("! [rejected]        x -> x (non-fast-forward)")),
            // No verdict: the last non-empty line, as before.
            ("Warning: Permanently added 'github.com' to the list of known hosts.\nsh: 1: cd: can't cd to /root/x\n\n", Some("sh: 1: cd: can't cd to /root/x")),
            // A verdict word mid-line is not a verdict line.
            ("hook says: error: no\nthe end\n", Some("the end")),
            ("", None),
            ("  \n\n", None),
        ];
        for (stderr, want) in cases {
            assert_eq!(telling_line(stderr), *want, "{stderr:?}");
        }
        let out = |code: i32, stderr: &str| -> PodCall {
            Ok(arena_core::ssh::SshOutput { success: false, code: Some(code), stdout: "PUSH_FAILED\n".into(), stderr: stderr.into() })
        };
        assert_eq!(failure_why(&out(1, missing)), "exit 1: ERROR: Repository not found.");
        assert_eq!(failure_why(&out(2, "")), "exit 2");
        assert_eq!(failure_why(&Err("timed out after 300s".into())), "timed out after 300s");
    }

    /// The per-pod lines of the fallback copy: why the push failed, and where the files went —
    /// or why the copy failed too.
    #[test]
    fn each_failed_push_says_where_its_files_went_or_why_they_didnt() {
        let u = |pod: &str, why: &str| Unpushed { pod: pod.into(), why: why.into(), branch: Some("autocommit-x".into()), at_push: true, wrong_origin: false, busy: None };
        let mut unpushed = vec![
            u("devtest-bloom", "GitHub push protection: OpenRouter API Key at ex.py:3"),
            u("devtest-cloud", "exit 128: fatal: Could not read from remote repository."),
        ];
        // One that failed before its push: its own failure line is all it gets.
        unpushed.push(Unpushed { pod: "devtest-delta".into(), why: "exit 2: no repo".into(), branch: None, at_push: false, wrong_origin: false, busy: None });
        let pulled: anyhow::Result<Pulled> = Ok(Pulled {
            failures: vec!["devtest-cloud [big]".into()],
            failed: vec![("devtest-cloud".into(), "[big] rsync exit 12".into())],
            snapshot_root: "/b/w41d5".into(),
            big_root: Some("/b/big".into()),
            whole: vec![],
        });
        assert_eq!(
            fallback_lines(&unpushed, &pulled),
            [
                "✗ devtest-bloom: git push failed (GitHub push protection: OpenRouter API Key at ex.py:3) → files copied to \
                 /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)",
                "✗ devtest-cloud: git push failed (exit 128: fatal: Could not read from remote repository.); the fallback copy \
                 failed too: [big] rsync exit 12",
            ]
        );
        let refused: anyhow::Result<Pulled> = Err(anyhow::anyhow!("refusing to pull (LOCAL_BACKUP_BIG_DIR is set, …)"));
        assert_eq!(
            fallback_lines(&unpushed[..1], &refused),
            ["✗ devtest-bloom: git push failed (GitHub push protection: OpenRouter API Key at ex.py:3); the fallback copy failed \
              too: refusing to pull (LOCAL_BACKUP_BIG_DIR is set, …)"]
        );
        assert!(fallback_lines(&[], &pulled).is_empty());
        assert!(fallback_lines(&unpushed[2..], &pulled).is_empty(), "never copied: never said to be");
    }

    /// FIX10: the backup's own last scan finding a key left after the redaction (exit 4,
    /// `PUSH_BLOCKED key left after redaction`) fails at the push like GitHub's refusal: the pod
    /// goes to the fallback copy, and its line says what was left and what to do — never that
    /// GitHub refused it, or that a pattern is missing.
    #[tokio::test]
    async fn a_key_left_after_the_redaction_fails_at_the_push_and_is_copied() {
        let f = fleet_of(&[("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(4, "").with_stdout("BRANCH autocommit-b\nPUSH_BLOCKED key left after redaction\n")]);
        let remote: Arc<dyn Remote> = fake.clone();
        let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
        let mut lines = Vec::new();
        let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
        assert_eq!(
            lines,
            ["[1/1] ✗ devtest-bloom: a key is still in the unpushed commits of autocommit-b after the redaction — in a commit's \
              author, committer or merged tag, a binary file or a symlink, or a redaction that didn't take — so nothing was \
              pushed. Take it out of those commits on devtest-bloom by hand"]
        );
        let why = "a key left in the unpushed commits after the redaction";
        let left = Unpushed { pod: "devtest-bloom".into(), why: why.into(), branch: Some("autocommit-b".into()), at_push: true, wrong_origin: false, busy: None };
        assert_eq!(tally.failed, ["devtest-bloom"]);
        assert_eq!(tally.unpushed, [left]);
        let pulled: anyhow::Result<Pulled> =
            Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
        assert_eq!(
            fallback_lines(&tally.unpushed, &pulled),
            [format!("✗ devtest-bloom: git push failed ({why}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
        );
    }

    /// [`backup_cfg`] with a configured repo, `o/r`: every backup checks each pod's `origin`.
    fn origin_cfg(base: &std::path::Path) -> Config {
        std::fs::create_dir_all(base).unwrap();
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n",
            base.display()
        ))
    }

    /// A backup the pod refused (exit 5): its `origin` is the upstream course repo, not `o/r`.
    fn wrong_origin() -> FakeReply {
        FakeReply::exit(5, "").with_stdout("WRONG_ORIGIN o/r https://github.com/ARENA-education/ARENA_materials\n")
    }

    const WRONG_WHY: &str = "origin is https://github.com/ARENA-education/ARENA_materials, not the configured o/r";

    /// Live (2026-10-17): a pod nobody set up had the upstream course repo as `origin`, and was
    /// never backed up to git, silently. Now every backup carries the configured repo, a pod whose
    /// origin isn't it fails on a line that says whose repo it has and what fixes it, and the run
    /// fails. Under `--no-pull` (the */15 cron) it is copied by rsync in the same run: its work
    /// reaches no git remote at all.
    #[tokio::test]
    async fn a_pod_whose_origin_isnt_the_configured_repo_fails_says_how_to_fix_it_and_is_copied() {
        let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        fake.script(&host(22002), [wrong_origin()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
        let mut lines = Vec::new();
        let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
        assert_eq!((tally.pushed, tally.failed.len()), (1, 1));
        let bloom = Unpushed { pod: "devtest-bloom".into(), why: WRONG_WHY.into(), branch: None, at_push: false, wrong_origin: true, busy: None };
        assert_eq!(tally.unpushed, std::slice::from_ref(&bloom));
        let mut got = unnumbered(&lines);
        got.sort();
        assert_eq!(
            got,
            [
                "✓ devtest-apple -> autocommit-a".to_string(),
                format!("✗ devtest-bloom: {WRONG_WHY} — not backed up to git; run `arena pods setup devtest-bloom`"),
            ]
        );

        // `pods backup --no-pull`: every pod's backup checks `o/r`; bloom fails the run and is copied.
        let stub = StubRsync::new("wrong-origin");
        let cfg = origin_cfg(&stub.0.join("backups"));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        fake.script(&host(22002), [wrong_origin(), repo_in_place()]);
        fake.script(&host(22003), [FakeReply::stdout("SKIP main\n")]);
        let err = handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let want = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), Some("o/r"));
            assert_eq!(execs(&fake, port)[0], (want, Some(BACKUP_TIMEOUT)), "{name}");
        }
        assert_eq!(execs(&fake, 22002)[1], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), "bloom's copy");
        let copied = stub.take();
        assert!(copied.len() == 3 && copied.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom, both tiers: {copied:?}");
        // Its line once copied.
        let pulled: anyhow::Result<Pulled> =
            Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
        assert_eq!(
            fallback_lines(&[bloom], &pulled),
            [format!("✗ devtest-bloom: not backed up to git ({WRONG_WHY}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
        );
        // No configured repo: no check asked for (an empty last argument).
        assert!(backup_command(REPO, Some(KEY), "m", None).ends_with(&format!("'{KEY}' ''")));
    }

    /// `init-branches` would push a participant's new branch to whatever `origin` is, and
    /// `set-branch` would fetch (and with `--hard` reset to) that repo's branch. A pod whose
    /// origin isn't the configured repo is refused by its push-first backup, so neither switches
    /// it: no copy, no switch, no init, said on its line with the fix. The others go ahead.
    #[tokio::test]
    async fn set_branch_and_init_branches_never_switch_a_pod_whose_origin_isnt_the_configured_repo() {
        let stub = StubRsync::new("wrong-origin-switch");
        let cfg = origin_cfg(&stub.0.join("backups"));
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), Some("o/r")), Some(BACKUP_TIMEOUT));
        let refused = format!("✗ devtest-bloom not switched: {WRONG_WHY} — not backed up to git; run `arena pods setup devtest-bloom`");
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
        fake.script(&host(22002), [wrong_origin()]);
        fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
        let _ = crate::take_said();
        let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "1 pod(s) NOT switched — each one's line says why: devtest-bloom"
        );
        assert!(crate::take_said().contains(&refused), "{refused}");
        assert_eq!(execs(&fake, 22002), [backup("bloom")], "bloom: refused, never copied or switched");
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        for (name, port) in [("apple", 22001), ("cloud", 22003)] {
            assert_eq!(execs(&fake, port), [backup(name), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
        }
        assert!(stub.take().is_empty(), "nothing copied");

        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
        fake.script(&host(22002), [wrong_origin()]);
        fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
        let err = handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &everyone(&fleet()), false, true, &stub.path()).await.unwrap_err();
        assert!(err.to_string().contains("NOT switched") && err.to_string().ends_with(": devtest-bloom"), "{err}");
        assert!(crate::take_said().contains(&refused));
        assert_eq!(execs(&fake, 22002), [backup("bloom")], "bloom never gets the init command");
        let bcfg = BackupConfig::from_config(&cfg, 1, 2);
        for (name, port) in [("apple", 22001), ("cloud", 22003)] {
            let init = init_branch_command(&bcfg, &format!("devtest-{name}"));
            // (The check is in the script the command runs under `sh -c`: its quotes escaped.)
            assert!(init.contains(r"origin_check '\''o/r'\''"), "the init command checks too");
            assert_eq!(execs(&fake, port), [backup(name), (init, Some(BRANCH_TIMEOUT))], "{name}");
        }
    }

    /// A stand-in `rsync` that runs the real one with the pull's own argv, the pod's side
    /// (`root@10.0.0.1:<path>`) read from the local tree `pod` instead. `None` without an rsync.
    fn local_pod_rsync(tag: &str, pod: &std::path::Path) -> Option<StubRsync> {
        let real = std::process::Command::new("sh").args(["-c", "command -v rsync"]).output().ok()?;
        let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
        if real.is_empty() {
            return None;
        }
        let stub = StubRsync::new(tag);
        arena_core::remote::write_test_script(
            stub.0.join("rsync"),
            format!(
                "#!/bin/sh\nfor a do\n  shift\n  case $a in root@10.0.0.1:*) a=\"{}/${{a#root@10.0.0.1:}}\";; esac\n  set -- \"$@\" \"$a\"\ndone\n\
                 exec {real} \"$@\"\n",
                pod.display()
            ),
        )
        .unwrap();
        Some(stub)
    }

    /// Live (2026-10-17): `pods backup` failed `[big]` on 9 pods every run, rsync exit 23 "could
    /// not make way for new symlink: ARENA_3.0" — a directory in the backup where the pods now
    /// have a symlink. Through the real pull and the real rsync: in both tiers the directory is
    /// moved aside — into `.arena-olddir-<day>/`, its name kept (review b5 #1) — nothing deleted,
    /// the rsync runs again, the pull succeeds, and the pod's line says so. The next pull, with
    /// each tier's own filters, leaves the holding directories as they are and says nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_that_became_a_symlink_on_the_pod_is_moved_aside_and_the_pull_succeeds() {
        let pod = std::env::temp_dir().join(format!("arena-pull-pod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pod);
        let Some(stub) = local_pod_rsync("makeway", &pod) else {
            eprintln!("no rsync here: skipped");
            return;
        };
        let put = |p: &std::path::Path, text: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(&pod.join("volume/ARENA_3.0/chapter1/new.py"), "today");
        put(&pod.join("notes.md"), "notes");
        std::os::unix::fs::symlink("volume/ARENA_3.0", pod.join("ARENA_3.0")).unwrap();
        let base = stub.0.join("backups");
        // What earlier pulls left: the directory, in the big tier and today's snapshot.
        put(&base.join("big/devtest-apple/ARENA_3.0/chapter1/old_work.py"), "yesterday's work");
        put(&base.join("w1d1/devtest-apple/ARENA_3.0/chapter1/old_work.py"), "this morning's work");
        let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/k\n"));
        let chosen = picked(&fleet(), &cfg, &["apple"]).await;
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let _ = crate::take_said();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap();
        let said = crate::take_said();
        for (tier, dir, old) in [("snapshot", "w1d1", "this morning's work"), ("big", "big", "yesterday's work")] {
            let dest = base.join(dir).join("devtest-apple");
            assert_eq!(std::fs::read_link(dest.join("ARENA_3.0")).unwrap(), std::path::Path::new("volume/ARENA_3.0"), "{tier}");
            let aside: Vec<String> = std::fs::read_dir(&dest)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".arena-olddir-") || n.contains(".olddir-"))
                .map(|n| format!("{n}/ARENA_3.0"))
                .collect();
            assert_eq!(aside, [format!(".arena-olddir-{}/ARENA_3.0", arena_core::pull::aside_day(arena_core::snapshot::unix_now()))], "{tier}");
            assert_eq!(std::fs::read_to_string(dest.join(&aside[0]).join("chapter1/old_work.py")).unwrap(), old, "{tier}");
            assert_eq!(std::fs::read_to_string(dest.join("notes.md")).unwrap(), "notes", "{tier}");
            let line = said.iter().find(|l| l.contains(&format!("✓ devtest-apple [{tier}] ("))).unwrap_or_else(|| panic!("{tier}: {said:#?}"));
            assert!(
                line.ends_with(&format!(
                    " — moved ARENA_3.0 aside to {} (a directory here, a symlink on the pod now; nothing deleted)",
                    aside[0]
                )),
                "{line}"
            );
        }
        // Again: nothing in the way, the holding directories as they were, in both tiers.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap();
        let said = crate::take_said();
        assert!(!said.iter().any(|l| l.contains("moved")), "{said:#?}");
        for (dir, old) in [("w1d1", "this morning's work"), ("big", "yesterday's work")] {
            let held = base.join(dir).join("devtest-apple").join(format!(".arena-olddir-{}/ARENA_3.0/chapter1/old_work.py", arena_core::pull::aside_day(arena_core::snapshot::unix_now())));
            assert_eq!(std::fs::read_to_string(held).unwrap(), old, "{dir}");
        }
        let _ = std::fs::remove_dir_all(&pod);
    }

    /// rsync's stderr is the only source of the path it couldn't replace, and a pod can put words
    /// there. A path outside the destination (`..`, absolute) or through a symlink in it is never
    /// acted on: everything stays where it was, the rsync isn't run again, the job fails as it
    /// would have, and its line says what was left alone and why.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pull_never_moves_anything_rsync_names_outside_its_destination() {
        let stub = StubRsync::new("makeway-spoof");
        arena_core::remote::write_test_script(
            stub.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             echo 'could not make way for new symlink: ../../outside/victim' >&2\n\
             echo 'could not make way for new symlink: linked/victim' >&2\n\
             echo 'could not make way for new regular file: /etc' >&2\nexit 23\n",
        )
        .unwrap();
        let base = stub.0.join("backups");
        let outside = stub.0.join("outside");
        std::fs::create_dir_all(outside.join("victim")).unwrap();
        std::fs::write(outside.join("victim/keep.txt"), "not ours").unwrap();
        std::fs::create_dir_all(base.join("big/devtest-apple")).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("big/devtest-apple/linked")).unwrap();
        let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/k\n"));
        let chosen = picked(&fleet(), &cfg, &["apple"]).await;
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let _ = crate::take_said();
        let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("2 rsync job(s) failed"), "{err}");
        assert_eq!(stub.take(), ["big/devtest-apple", "w1d1/devtest-apple"], "each job's first pass once: no retry");
        let names = |d: &std::path::Path| std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(&outside), ["victim"]);
        assert_eq!(names(&outside.join("victim")), ["keep.txt"]);
        assert!(std::fs::symlink_metadata(base.join("big/devtest-apple/linked")).unwrap().file_type().is_symlink());
        let said = crate::take_said();
        let big = said.iter().find(|l| l.contains("devtest-apple [big]")).unwrap();
        assert!(
            big.ends_with(
                " — rsync couldn't replace, and left as it is: ../../outside/victim (not a path inside the destination), \
                 linked/victim (linked on the way to it is a symlink here), /etc (not a path inside the destination)"
            ),
            "{big}"
        );
    }

    /// `set-branch` pushes each pod's branch first. One GitHub refuses (a secret the redaction
    /// doesn't know) can't be: its files are copied into LOCAL_BACKUP_DIR, then it's switched
    /// all the same — its commits stay in the local branch — and its line and the run's error
    /// say so loudly. The pods whose push worked are switched with nothing copied.
    #[tokio::test]
    async fn set_branch_pushes_each_branch_first_and_copies_a_pod_whose_branch_cant_be_before_switching() {
        let stub = StubRsync::new("setbranch-fallback");
        let cfg = cfg_at(&stub);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
        fake.script(&host(22002), [push_blocked("autocommit-devtest-w1d1-bloom"), repo_in_place(), FakeReply::ok()]);
        let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom");
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        // bloom: its backup, the copy (asked where its repo is), then the switch — in that order.
        assert_eq!(
            execs(&fake, 22002),
            [backup("bloom"), (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), (hard.clone(), Some(BRANCH_TIMEOUT))]
        );
        for (name, port) in [("apple", 22001), ("cloud", 22003)] {
            assert_eq!(execs(&fake, port), [backup(name), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
        }
        let got = stub.take();
        assert!(got.len() == 3 && got.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom copied: {got:?}");

        // Its line: which branch, why, where its files went, and where its commits are now — in
        // the branch, or after `--hard` onto that same branch, pinned by a ref (codex #3).
        let u = Unpushed {
            pod: "devtest-bloom".into(),
            why: "GitHub push protection: OpenRouter API Key at ex.py:3".into(),
            branch: Some("autocommit-devtest-w1d1-bloom".into()),
            at_push: true,
            wrong_origin: false,
            busy: None,
        };
        let to = "/b/w1d1/devtest-bloom (and /b/big/devtest-bloom)";
        assert_eq!(
            not_pushed_note(&u, to, Some("main")),
            " — ⚠ NOT PUSHED FIRST: branch autocommit-devtest-w1d1-bloom (GitHub push protection: OpenRouter API Key at ex.py:3); \
             its files were copied to /b/w1d1/devtest-bloom (and /b/big/devtest-bloom) first, and its commits stay on the pod in \
             that branch (switching deletes no branch)"
        );
        assert!(
            not_pushed_note(&u, to, Some("autocommit-devtest-w1d1-bloom")).ends_with(
                "and the --hard reset moves autocommit-devtest-w1d1-bloom to origin's, and its unpushed commits stay on the pod \
                 under refs/arena-discarded/<time>-autocommit-devtest-w1d1-bloom"
            ),
            "{}",
            not_pushed_note(&u, to, Some("autocommit-devtest-w1d1-bloom"))
        );
        assert_eq!(not_pushed_note(&u, to, None), not_pushed_note(&u, to, Some("main")));
        // What the switch did with the real keys of files git has redacted.
        assert_eq!(
            unredacted_note("RESTORED Grokking\nUNREDACTED_KEPT part4/exercises.py\nUNREDACTED_KEPT joep.py\n"),
            " — the real key(s) in 2 file(s) git has redacted are set aside in .git/arena-unredacted/ (a switch back puts them \
             back): part4/exercises.py, joep.py"
        );
        assert_eq!(unredacted_note("UNREDACTED_RESTORED joep.py\n"), " — real key(s) back in 1 file(s) git has redacted: joep.py");
        assert_eq!(unredacted_note("RESTORED Grokking\n"), "");
        // What init-branches left for the next backup to scan and push (FIX9B item 2).
        assert_eq!(
            unpushed_note("Switched to a new branch 'x'\nUNPUSHED 2 0123456789abcdef0123456789abcdef01234567\n"),
            " — 2 commit(s) origin doesn't have stay on it on the pod, unpushed (the branch went to origin at 0123456789, the \
             newest commit origin has): the next backup scans them for keys and pushes them"
        );
        assert_eq!(
            unpushed_note("UNPUSHED 3 none\n"),
            " — not pushed: origin has none of its 3 commit(s); the next backup scans them for keys and pushes the branch"
        );
        assert_eq!(unpushed_note("Switched to a new branch 'x'\nUNREDACTED_RESTORED a.py\n"), "");
    }

    /// A backup that found a merge in progress on the pod, its conflicts unresolved (a conflicted
    /// `git pull upstream main`; exit 6): nothing committed.
    fn busy_merge(branch: &str) -> FakeReply {
        FakeReply::exit(BACKUP_BUSY_EXIT, "").with_stdout(&format!("BRANCH {branch}\nBUSY {branch} merge unmerged\n"))
    }

    const MERGE_WHY: &str = "a merge is in progress on autocommit-b — not committed; resolve the conflicts and `git add` them, then \
                             `git commit` — or abort it (`git merge --abort`) on the pod";

    /// Review R3: the 15-min backup committed a participant's conflicted `git pull upstream main`
    /// — conflict markers and all, as a merge commit, pushed — and `git merge --abort` no longer
    /// worked. Now that pod's backup stops before anything is staged (`BUSY`, exit 6): its line
    /// says what is in progress and how to end it either way, it is copied by rsync in the same
    /// run like a failed push (both tiers, even under --no-pull: its uncommitted work is nowhere
    /// else), and the run fails. The other pods back up as always.
    #[tokio::test]
    async fn a_pod_mid_merge_is_left_as_it_is_said_how_to_end_it_and_copied() {
        // bloom's look at its merge (`resolve`) fails here: it is copied as before, never finished.
        let script = |fake: &FakeRemote| {
            fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
            fake.script(&host(22002), [busy_merge("autocommit-b"), FakeReply::exit(2, "sh: cd: no such dir"), repo_in_place()]);
            fake.script(&host(22003), [FakeReply::stdout("BRANCH autocommit-c\nNO_CHANGES autocommit-c\n")]);
        };
        // Each pod's line: bloom's says what is in progress and both ways to end it.
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let jobs = targets(&fleet()).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
        let mut lines = Vec::new();
        let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
        assert_eq!((tally.pushed, tally.unchanged, tally.failed.len()), (1, 1, 1));
        let mut got = unnumbered(&lines);
        got.sort();
        assert_eq!(
            got,
            [
                "= devtest-cloud (no changes, on autocommit-c)".to_string(),
                "✓ devtest-apple -> autocommit-a".to_string(),
                "✗ devtest-bloom: a merge is in progress on autocommit-b — not committed (a backup now would commit the merge's \
                 state, unresolved conflicts and all, and end the merge); resolve the conflicts and `git add` them, then `git \
                 commit` — or abort it (`git merge --abort`) on the pod"
                    .to_string(),
            ]
        );
        // Its record: what is in progress, failed at the push (so it's copied), said as what it is.
        let bloom = Unpushed {
            pod: "devtest-bloom".into(),
            why: MERGE_WHY.into(),
            branch: Some("autocommit-b".into()),
            at_push: true,
            wrong_origin: false,
            busy: Some("merge".into()),
        };
        assert_eq!(tally.unpushed, std::slice::from_ref(&bloom));

        // `pods backup --no-pull` (the cron's): bloom alone copied, both tiers; the run fails.
        let stub = StubRsync::new("busy");
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let err = handle_full_backup(fake.clone(), &cfg_at(&stub), &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        let copied = stub.take();
        assert!(copied.len() == 3 && copied.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom, both tiers: {copied:?}");
        let bloom_calls = execs(&fake, 22002);
        assert_eq!(bloom_calls.len(), 3, "its backup, its look, its copy");
        assert_eq!(bloom_calls[1].0, arena_core::resolve::look_command(REPO), "bloom's look");
        assert_eq!(bloom_calls[2], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), "bloom's copy");
        for port in [22001, 22003] {
            assert_eq!(execs(&fake, port).len(), 1, "{port}: its backup only");
        }
        // Its line after the copy: not "git push failed" — nothing was committed.
        let pulled: anyhow::Result<Pulled> =
            Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
        assert_eq!(
            fallback_lines(std::slice::from_ref(&bloom), &pulled),
            [format!("✗ devtest-bloom: not backed up to git ({MERGE_WHY}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
        );
        let refused: anyhow::Result<Pulled> = Err(anyhow::anyhow!("no space"));
        assert_eq!(
            fallback_lines(&[bloom], &refused),
            [format!("✗ devtest-bloom: not backed up to git ({MERGE_WHY}); the fallback copy failed too: no space")]
        );
    }

    /// `set-branch` backs up each pod's branch first; a pod mid-merge can't be (`BUSY`). Its files
    /// are copied first, as for a failed push, and then the switch: `--hard` ends the merge with
    /// the uncommitted changes it discards — said, with where the copy is and where on the pod the
    /// switch saved them first (`DISCARDED_SAVED`: review ai-review claude #2) — while the
    /// gentle switch and init-branches refuse to end it (their command's own refusal, on the pod's
    /// line with the copy's note), and the run never calls that pod switched. A pod whose copy
    /// failed too is not switched at all.
    #[tokio::test]
    async fn set_branch_copies_a_pod_mid_merge_first_and_its_line_names_the_merge() {
        let stub = StubRsync::new("busy-switch");
        let cfg = cfg_at(&stub);
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
        let copied_first = " — ⚠ NOT COMMITTED FIRST: a merge is in progress on autocommit-b, so nothing was committed; its files were \
                            copied to ";
        let stays = "its commits stay on the pod in that branch (switching deletes no branch)";

        // --hard: copied, then switched; the line says the switch ended the merge. Switched like
        // the rest — not a failure (review fb2 F2) — and one line after the run names it.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
        fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::stdout(SAVED_BLOOM)]);
        let _ = crate::take_said();
        handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
        let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
        assert_eq!(execs(&fake, 22002), [backup("bloom"), probe.clone(), hard]);
        assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")));
        let said = unnumbered(&crate::take_said());
        assert!(said.contains(&ENDED_BLOOM.to_string()), "{said:#?}");
        let line = said.iter().find(|l| l.starts_with("✓ devtest-bloom (on main)")).unwrap_or_else(|| panic!("{said:#?}"));
        assert!(line.starts_with(&format!("✓ devtest-bloom (on main){copied_first}")), "{line}");
        assert!(
            line.ends_with(&format!(
                " first, and the --hard switch ends the merge and discards its uncommitted changes to tracked files; {stays} — what \
                 it discarded was saved to .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it \
                 back)"
            )),
            "{line}"
        );

        // Gentle, and init-branches: copied, then the switch refuses to end the merge — said on
        // the pod's line with the copy's note, and counted as a failed switch, never a switched one.
        let refusal = |who: &str| {
            format!(
                "{who}: a merge is in progress on autocommit-b — not switched (a switch now would end the merge); resolve the \
                 conflicts and `git add` them, then `git commit` — or abort it (`git merge --abort`) on the pod, then switch again"
            )
        };
        for (who, init) in [("arena set-branch", false), ("arena init-branches", true)] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
            fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::exit(BACKUP_BUSY_EXIT, &refusal(who))]);
            let err = if init {
                handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &everyone(&fleet()), false, true, &stub.path()).await
            } else {
                handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await
            }
            .unwrap_err();
            let verb = if init { "init branch" } else { "switch branch" };
            assert_eq!(err.to_string(), format!("1 pod(s) failed to {verb}"), "{who}: never said to be switched");
            assert_eq!(execs(&fake, 22002).len(), 3, "{who}: backup, copy, switch");
            assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "{who}");
            let said = unnumbered(&crate::take_said());
            let line = said.iter().find(|l| l.starts_with("✗ devtest-bloom (exit 6): ")).unwrap_or_else(|| panic!("{who}: {said:#?}"));
            assert!(line.starts_with(&format!("✗ devtest-bloom (exit 6): {}{copied_first}", refusal(who))), "{line}");
            assert!(line.ends_with(&format!(" first, and {stays}")), "{line}");
        }

        // The copy fails too: not switched, its line says why both failed.
        let failing = StubRsync::new("busy-copyfail");
        arena_core::remote::write_test_script(
            failing.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             echo 'rsync: No space left on device (28)' >&2; exit 11\n",
        )
        .unwrap();
        let cfg = cfg_at(&failing);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place()]);
        let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet_of(&[("bloom", 22002)])), true, false, true, &failing.path())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-bloom");
        assert_eq!(execs(&fake, 22002), [backup("bloom"), probe], "never switched");
        let said = crate::take_said();
        let line = said.iter().find(|l| l.starts_with("✗ devtest-bloom not switched: ")).unwrap_or_else(|| panic!("{said:#?}"));
        assert!(
            line.starts_with(&format!(
                "✗ devtest-bloom not switched: its work couldn't be committed ({MERGE_WHY}) and copying its files into "
            )),
            "{line}"
        );
    }

    /// The line after a `--hard` run that ended bloom's merge after copying it.
    const ENDED_BLOOM: &str = "⚠ 1 pod(s) had work no backup had saved (an operation in progress, or changes, commits or untracked \
                               files on a branch no backup commits), copied before the --hard switch discarded or overwrote it — what \
                               it discarded is also saved on the pod, in .git/arena-discarded/<time>/, which no later copy overwrites \
                               (see each one's line): devtest-bloom";

    /// Review ai-review claude #2: the most a `--hard` save copies (`DISCARD_MAX_GB`, GB) reaches
    /// the switch's command from the config; a value it can't read stops the run before anything
    /// is backed up or switched. The gentle switch saves nothing, so never reads it.
    #[tokio::test]
    async fn hard_takes_its_save_limit_from_the_config_and_a_bad_one_stops_it_first() {
        let stub = StubRsync::new("discard-limit");
        let dir = stub.0.join("backups");
        std::fs::create_dir_all(&dir).unwrap();
        let text = |extra: &str| {
            format!(
                "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
                 GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
                 LOCAL_BACKUP_DIR={}\n{extra}",
                dir.display()
            )
        };
        let cfg = |extra: &str| Config::parse(&text(extra));
        let apple = everyone(&fleet_of(&[("apple", 22001)]));
        let backup = (backup_command(REPO, Some(KEY), "arena backup devtest-apple", None), Some(BACKUP_TIMEOUT));
        let pushed = || [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()];
        for (extra, kb) in [("", 1024 * 1024), ("DISCARD_MAX_GB=0.5\n", 512 * 1024)] {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), pushed());
            handle_set_branch(fake.clone(), &cfg(extra), "main", &apple, true, false, true, &stub.path()).await.unwrap();
            let hard = (checkout_command_limited(REPO, "main", Some(KEY), true, kb), Some(BRANCH_TIMEOUT));
            assert_eq!(execs(&fake, 22001), [backup.clone(), hard], "{extra:?}");
        }
        // From the environment alone, the file without it (Live 6, FIX6B item 5): the same.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), pushed());
        let from_env = Config::parse_with_env(&text(""), |k| (k == "DISCARD_MAX_GB").then(|| "0.5".to_string()));
        handle_set_branch(fake.clone(), &from_env, "main", &apple, true, false, true, &stub.path()).await.unwrap();
        let hard = (checkout_command_limited(REPO, "main", Some(KEY), true, 512 * 1024), Some(BRANCH_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup.clone(), hard]);
        let fake = Arc::new(FakeRemote::new());
        let err = handle_set_branch(fake.clone(), &cfg("DISCARD_MAX_GB=lots\n"), "main", &apple, true, false, true, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "DISCARD_MAX_GB must be a number of GiB above 0, a fraction too, like 1 or 0.5 (got `lots`)");
        assert!(execs(&fake, 22001).is_empty(), "nothing backed up or switched");
        fake.script(&host(22001), pushed());
        handle_set_branch(fake.clone(), &cfg("DISCARD_MAX_GB=lots\n"), "main", &apple, false, false, true, &stub.path()).await.unwrap();
        assert_eq!(execs(&fake, 22001), [backup, (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT))]);
    }

    /// What a `--hard` switch prints when it saved what it discarded ([`checkout_command`]).
    const SAVED_BLOOM: &str = "Switched to and reset branch 'main'\nDISCARDED_SAVED .git/arena-discarded/20261019T090102Z\n";

    /// Review ai-review codex #3: the refs a `--hard` switch pinned the commits it left on no branch
    /// with (`DISCARDED_PINNED`) are named on the pod's line, after where it saved the rest; with
    /// none, no words for them.
    #[test]
    fn a_hard_switchs_line_names_the_refs_it_pinned() {
        assert_eq!(discarded_note("Switched to and reset branch 'main'\n"), "");
        let saved = " — what it discarded was saved to .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it back";
        assert_eq!(discarded_note(SAVED_BLOOM), format!("{saved})"));
        let pinned = "DISCARDED_SAVED .git/arena-discarded/20261019T090102Z\r\nDISCARDED_PINNED refs/arena-discarded/20261019T090102Z\r\n\
                      DISCARDED_PINNED refs/arena-discarded/20261019T090102Z-main\n";
        assert_eq!(
            discarded_note(pinned),
            format!("{saved}; its commits kept by refs/arena-discarded/20261019T090102Z, refs/arena-discarded/20261019T090102Z-main)")
        );
    }

    /// Review fb2 F2: the morning's `set-branch main --all --hard --init-branches` skipped
    /// init-branches for the whole fleet when one pod was mid-merge — that pod was copied and
    /// switched, yet counted as an error, so every pod stayed on main, where no backup commits.
    /// Now it is switched like the rest (its line's ⚠ says what the copy holds) and the init goes
    /// on, on every pod.
    #[tokio::test]
    async fn hard_init_branches_goes_on_past_a_pod_mid_merge_it_switched() {
        use super::{resolve_week_day, set_branch_and_init, SetBranchRun};
        let stub = StubRsync::new("chain-busy");
        let cfg = cfg_at(&stub);
        let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
        let bcfg = BackupConfig::from_config(&cfg, week, day);
        let fake = Arc::new(FakeRemote::new());
        let pushed = |b: &str| FakeReply::stdout(&format!("BRANCH {b}\nPUSHED {b}\n"));
        let on_main = || FakeReply::stdout("SKIP main\n");
        fake.script(&host(22001), [pushed("autocommit-a"), FakeReply::ok(), on_main(), FakeReply::ok()]);
        fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::ok(), on_main(), FakeReply::ok()]);
        fake.script(&host(22003), [pushed("autocommit-c"), FakeReply::ok(), on_main(), FakeReply::ok()]);
        let _ = crate::take_said();
        let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
        set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet()), &how, &stub.path()).await.unwrap();
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
        let init = |name: &str| (init_branch_command(&bcfg, &format!("devtest-{name}")), Some(BRANCH_TIMEOUT));
        let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup("apple"), hard.clone(), backup("apple"), init("apple")]);
        assert_eq!(execs(&fake, 22002), [backup("bloom"), probe, hard.clone(), backup("bloom"), init("bloom")], "bloom: copied, switched, init'd");
        assert_eq!(execs(&fake, 22003), [backup("cloud"), hard, backup("cloud"), init("cloud")]);
        assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom copied");
        let said = unnumbered(&crate::take_said());
        assert!(said.contains(&ENDED_BLOOM.to_string()), "{said:#?}");
        assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom (on main) — ⚠ NOT COMMITTED FIRST: a merge is in progress on autocommit-b")), "{said:#?}");
        for name in ["apple", "bloom", "cloud"] {
            let line = format!("✓ devtest-{name} -> {}", bcfg.branch_for(&format!("devtest-{name}")));
            assert!(said.contains(&line), "{line}: {said:#?}");
        }
    }

    /// The reverse of [`hard_init_branches_goes_on_past_a_pod_mid_merge_it_switched`]: a pod that
    /// really wasn't switched — its copy failed, its `--hard` switch failed, or the gentle switch
    /// refused its merge (a BUSY pod not switched is a failure, as before) — is left out of the
    /// init, said in one line, and the run fails naming it. The pods that did switch are init'd
    /// all the same (review pr4: one pod used to stop the init for the whole fleet). With none
    /// switched, no init at all.
    #[tokio::test]
    async fn init_branches_leaves_out_a_pod_that_wasnt_switched_and_inits_the_rest() {
        use super::{resolve_week_day, set_branch_and_init, SetBranchRun};
        let stub = StubRsync::new("chain-busy-fail");
        let failing = StubRsync::new("chain-busy-copyfail");
        arena_core::remote::write_test_script(
            failing.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             echo 'rsync: No space left on device (28)' >&2; exit 11\n",
        )
        .unwrap();
        let pushed = || FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n");
        let refused = FakeReply::exit(BACKUP_BUSY_EXIT, "arena set-branch: a merge is in progress on autocommit-b — not switched");
        let left_out = "init-branches on the 1 pod(s) that switched — not on devtest-bloom, which didn't (see each one's line above)";
        // (case, hard, the rsync, bloom's replies after its backup, the run's error)
        let cases: Vec<(&str, bool, &StubRsync, Vec<FakeReply>, &str)> = vec![
            ("copy failed", true, &failing, vec![repo_in_place()], "1 pod(s) NOT switched — each one's line says why: devtest-bloom"),
            ("--hard failed", true, &stub, vec![repo_in_place(), FakeReply::exit(1, "fatal: couldn't find remote ref main")], "1 pod(s) failed to switch branch"),
            ("gentle refused", false, &stub, vec![repo_in_place(), refused], "1 pod(s) failed to switch branch"),
        ];
        for (case, hard, rsync, after, want) in cases {
            let cfg = cfg_at(rsync);
            let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
            let apple_init = (init_branch_command(&BackupConfig::from_config(&cfg, week, day), "devtest-apple"), Some(BRANCH_TIMEOUT));
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [pushed(), FakeReply::ok(), FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
            fake.script(&host(22002), std::iter::once(busy_merge("autocommit-b")).chain(after).collect::<Vec<_>>());
            let _ = crate::take_said();
            let how = SetBranchRun { branch: "main", hard, dry_run: false, yes: true, init_branches: true };
            let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("apple", 22001), ("bloom", 22002)])), &how, &rsync.path())
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), want, "{case}");
            assert!(!execs(&fake, 22002).iter().any(|(c, _)| c.contains("git checkout -b")), "{case}: bloom init'd");
            let apple = execs(&fake, 22001);
            assert_eq!((apple.len(), apple.last()), (4, Some(&apple_init)), "{case}: apple switched, then init'd");
            let said = crate::take_said();
            assert!(said.iter().any(|l| l == left_out), "{case}: {said:#?}");
            assert!(!said.iter().any(|l| l.starts_with("⚠ ") && l.contains("had work no backup had saved")), "{case}");
        }
        // Nobody switched: no init at all, said.
        let cfg = cfg_at(&failing);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place()]);
        let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
        let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("bloom", 22002)])), &how, &failing.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-bloom");
        assert_eq!(execs(&fake, 22002).len(), 2, "its backup and the copy's probe, nothing more");
        assert!(crate::take_said().iter().any(|l| l == "init-branches NOT run: no pod switched"));
    }

    /// Review pr4: a pod whose push failed for an ordinary reason — the network, a non-fast-forward,
    /// GitHub's push protection — was copied and switched like the rest, yet `set-branch main
    /// --hard --init-branches` then skipped the init for the whole fleet, leaving every pod on main
    /// where no backup commits (the */15 cron SKIPs them all). Now every pod that switched is
    /// init'd, the copied one too; the run still fails after the init, naming that pod.
    #[tokio::test]
    async fn init_branches_runs_on_every_switched_pod_even_one_copied_after_a_failed_push() {
        use super::{resolve_week_day, set_branch_and_init, SetBranchRun};
        let stub = StubRsync::new("chain-pushfail");
        let cfg = cfg_at(&stub);
        let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
        let bcfg = BackupConfig::from_config(&cfg, week, day);
        let pushed = |b: &str| FakeReply::stdout(&format!("BRANCH {b}\nPUSHED {b}\n"));
        let on_main = || FakeReply::stdout("SKIP main\n");
        let push_failed = |stderr: &str| FakeReply::exit(1, stderr).with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n");
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
        let init = |name: &str| (init_branch_command(&bcfg, &format!("devtest-{name}")), Some(BRANCH_TIMEOUT));
        let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
        let failures = [
            ("network", push_failed("ssh: connect to host github.com port 22: Connection timed out\nfatal: Could not read from remote repository.")),
            ("non-fast-forward", push_failed(" ! [rejected]        autocommit-b -> autocommit-b (fetch first)\nerror: failed to push some refs")),
            ("push protection", push_blocked("autocommit-b")),
        ];
        for (case, failed) in failures {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [pushed("autocommit-a"), FakeReply::ok(), on_main(), FakeReply::ok()]);
            fake.script(&host(22002), [failed, repo_in_place(), FakeReply::ok(), on_main(), FakeReply::ok()]);
            fake.script(&host(22003), [pushed("autocommit-c"), FakeReply::ok(), on_main(), FakeReply::ok()]);
            let _ = crate::take_said();
            let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
            let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet()), &how, &stub.path()).await.unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom", "{case}");
            assert_eq!(execs(&fake, 22002), [backup("bloom"), probe.clone(), hard.clone(), backup("bloom"), init("bloom")], "{case}");
            for (name, port) in [("apple", 22001), ("cloud", 22003)] {
                assert_eq!(execs(&fake, port), [backup(name), hard.clone(), backup(name), init(name)], "{case}: {name}");
            }
            assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "{case}: only bloom copied");
            let said = unnumbered(&crate::take_said());
            assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom (on main) — ⚠ NOT PUSHED FIRST: branch autocommit-b (")), "{case}: {said:#?}");
            for name in ["apple", "bloom", "cloud"] {
                let line = format!("✓ devtest-{name} -> {}", bcfg.branch_for(&format!("devtest-{name}")));
                assert!(said.contains(&line), "{case}: {line}: {said:#?}");
            }
            assert!(!said.iter().any(|l| l.starts_with("init-branches")), "{case}: nobody left out");
        }
        // An init that fails too: both said, set-branch's first.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place(), FakeReply::ok(), on_main(), FakeReply::exit(1, "fatal: the remote end hung up")]);
        let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
        let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("bloom", 22002)])), &how, &stub.path()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom; then init-branches: 1 \
             pod(s) failed to init branch"
        );
    }

    /// Review fb2 F3: a pod on main — where no backup commits — with a merge in progress or
    /// changes to tracked files was reset by `set-branch --hard` with no copy (its backup said
    /// only SKIP). Now its backup says so (`UNCOMMITTED`) and `--hard` copies it first, then
    /// switches it — its line saying what the copy holds, not a failure — and doesn't reset it if
    /// the copy fails. A clean pod on main isn't copied; the gentle switch and `pods backup`
    /// copy none of them, as before.
    #[tokio::test]
    async fn hard_copies_a_pod_on_main_with_uncommitted_work_before_resetting_it() {
        let stub = StubRsync::new("main-dirty");
        let cfg = cfg_at(&stub);
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
        let dirty = || FakeReply::stdout("UNCOMMITTED main changes\nSKIP main\n");
        let merging = || FakeReply::stdout("UNCOMMITTED main merge unmerged\nSKIP main\n");
        let clean = || FakeReply::stdout("SKIP main\n");
        let script = |fake: &FakeRemote| {
            fake.script(&host(22001), [dirty(), repo_in_place(), FakeReply::stdout(SAVED_BLOOM)]);
            fake.script(&host(22002), [clean(), FakeReply::ok()]);
            fake.script(&host(22003), [merging(), repo_in_place(), FakeReply::ok()]);
        };
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let _ = crate::take_said();
        handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
        let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup("apple"), probe.clone(), hard.clone()]);
        assert_eq!(execs(&fake, 22002), [backup("bloom"), hard.clone()], "clean: not copied");
        assert_eq!(execs(&fake, 22003), [backup("cloud"), probe.clone(), hard.clone()]);
        let copied = stub.take();
        assert!(!copied.is_empty() && copied.iter().all(|d| d.ends_with("/devtest-apple") || d.ends_with("/devtest-cloud")), "{copied:?}");
        let said = unnumbered(&crate::take_said());
        let line = |pod: &str| said.iter().find(|l| l.starts_with(&format!("✓ {pod} (on main)"))).cloned().unwrap_or_else(|| panic!("{said:#?}"));
        let reflog = "the --hard reset moves main to origin's, and its unpushed commits stay on the pod under refs/arena-discarded/<time>-main";
        let apple = line("devtest-apple");
        assert!(
            apple.starts_with(
                "✓ devtest-apple (on main) — ⚠ NOT COMMITTED FIRST: main has uncommitted changes to tracked files, and no backup \
                 commits on main; its files were copied to "
            ) && apple.ends_with(&format!(
                " first, and the --hard switch discards them; {reflog} — what it discarded was saved to \
                 .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it back)"
            )),
            "{apple}"
        );
        let cloud = line("devtest-cloud");
        assert!(
            cloud.starts_with("✓ devtest-cloud (on main) — ⚠ NOT COMMITTED FIRST: a merge is in progress on main, and no backup commits on main; ")
                && cloud.contains(" first, and the --hard switch ends the merge and discards its uncommitted changes to tracked files"),
            "{cloud}"
        );
        assert_eq!(line("devtest-bloom"), "✓ devtest-bloom (on main)");
        assert!(said.iter().any(|l| l.starts_with("⚠ 2 pod(s) had work no backup had saved") && l.ends_with(": devtest-apple, devtest-cloud")), "{said:#?}");

        // The copy fails: not reset.
        let failing = StubRsync::new("main-dirty-copyfail");
        arena_core::remote::write_test_script(
            failing.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             echo 'rsync: No space left on device (28)' >&2; exit 11\n",
        )
        .unwrap();
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [dirty(), repo_in_place(), FakeReply::ok()]);
        let err = handle_set_branch(fake.clone(), &cfg_at(&failing), "main", &everyone(&fleet_of(&[("apple", 22001)])), true, false, true, &failing.path())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-apple");
        assert_eq!(execs(&fake, 22001), [backup("apple"), probe], "never reset");
        let said = crate::take_said();
        assert!(
            said.iter().any(|l| l.starts_with(
                "✗ devtest-apple not switched: its work couldn't be committed (main has uncommitted changes to tracked files — no \
                 backup commits on main) and copying its files into "
            )),
            "{said:#?}"
        );

        // Gentle: nothing discarded, nothing copied.
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await.unwrap();
        let gentle = (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT));
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            assert_eq!(execs(&fake, port), [backup(name), gentle.clone()], "{name}");
        }
        assert!(stub.take().is_empty());

        // `pods backup --no-pull` (the cron's): skipped as always — no copy, no failure.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [dirty()]);
        fake.script(&host(22003), [merging()]);
        fake.script(&host(22002), [clean()]);
        handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap();
        assert!(stub.take().is_empty());
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port).len(), 1, "{port}: its backup only");
        }
    }

    /// Review pr4: on a branch no backup commits, commits no remote has and untracked files —
    /// `UNCOMMITTED main commits`, `UNCOMMITTED HEAD untracked` — are copied before `--hard` as
    /// changes are, `.git` and all, each pod's line saying what the copy holds (the commits are
    /// pushed nowhere, so NOT PUSHED; an untracked file is overwritten where origin's branch tracks
    /// it); switched, not failures. A clean pod isn't copied, and the gentle switch copies none.
    #[tokio::test]
    async fn hard_copies_a_pod_on_main_with_commits_or_untracked_files_first() {
        let stub = StubRsync::new("main-commits");
        let cfg = cfg_at(&stub);
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
        let script = |fake: &FakeRemote| {
            fake.script(&host(22001), [FakeReply::stdout("UNCOMMITTED main commits\nSKIP main\n"), repo_in_place(), FakeReply::ok()]);
            fake.script(&host(22002), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
            fake.script(&host(22003), [FakeReply::stdout("UNCOMMITTED HEAD untracked\nSKIP HEAD\n"), repo_in_place(), FakeReply::ok()]);
        };
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        let _ = crate::take_said();
        handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
        let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup("apple"), probe.clone(), hard.clone()]);
        assert_eq!(execs(&fake, 22002), [backup("bloom"), hard.clone()], "clean: not copied");
        assert_eq!(execs(&fake, 22003), [backup("cloud"), probe, hard]);
        let copied = stub.take();
        assert!(!copied.is_empty() && copied.iter().all(|d| d.ends_with("/devtest-apple") || d.ends_with("/devtest-cloud")), "{copied:?}");
        let said = unnumbered(&crate::take_said());
        let line = |pod: &str| said.iter().find(|l| l.starts_with(&format!("✓ {pod} (on main)"))).cloned().unwrap_or_else(|| panic!("{said:#?}"));
        let apple = line("devtest-apple");
        assert!(
            apple.starts_with(
                "✓ devtest-apple (on main) — ⚠ NOT PUSHED FIRST: main has commits no remote has, and no backup pushes main; its \
                 files were copied to "
            ) && apple.ends_with(
                " first, and that copy has its .git, those commits with it; the --hard reset moves main to origin's, and its \
                 unpushed commits stay on the pod under refs/arena-discarded/<time>-main"
            ),
            "{apple}"
        );
        let cloud = line("devtest-cloud");
        assert!(
            cloud.starts_with(
                "✓ devtest-cloud (on main) — ⚠ NOT COMMITTED FIRST: a detached HEAD has untracked files, and no backup commits on \
                 a detached HEAD; its files were copied to "
            ) && cloud.ends_with(
                " first, and the --hard switch overwrites any of them origin/main tracks; the commits of its detached HEAD stay \
                 on the pod under refs/arena-discarded/<time>"
            ),
            "{cloud}"
        );
        assert_eq!(line("devtest-bloom"), "✓ devtest-bloom (on main)");
        assert!(said.iter().any(|l| l.starts_with("⚠ 2 pod(s) had work no backup had saved") && l.ends_with(": devtest-apple, devtest-cloud")), "{said:#?}");
        // Gentle: nothing discarded or overwritten (git refuses to), nothing copied.
        let fake = Arc::new(FakeRemote::new());
        script(&fake);
        handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await.unwrap();
        let gentle = (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT));
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            assert_eq!(execs(&fake, port), [backup(name), gentle.clone()], "{name}");
        }
        assert!(stub.take().is_empty());
    }

    /// A [`Remote`] that, at every call, notes whether the pod's backup lock was held right then
    /// (a second open of its lock file can't take it), then answers as `inner` scripts it.
    struct LockProbe {
        inner: Arc<FakeRemote>,
        dir: String,
        /// `(port, the lock was held)` per exec, in order.
        seen: std::sync::Mutex<Vec<(u16, bool)>>,
    }

    impl LockProbe {
        fn held(&self, port: u16) -> Vec<bool> {
            self.seen.lock().unwrap().iter().filter(|(p, _)| *p == port).map(|(_, h)| *h).collect()
        }
    }

    #[async_trait]
    impl Remote for LockProbe {
        async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
            let pod = fleet().pods.into_iter().find(|p| p.ssh_port == Some(t.port)).map(|p| p.name).unwrap();
            let f = arena_core::pull::open_pod_lock(&self.dir, &pod).unwrap();
            let free = arena_core::pull::try_lock(f).unwrap().is_ok();
            self.seen.lock().unwrap().push((t.port, !free));
            self.inner.exec(t, cmd, timeout).await
        }
        async fn copy(&self, t: &SshTarget, l: &str, r: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
            self.inner.copy(t, l, r, timeout).await
        }
        async fn copy_recursive(&self, t: &SshTarget, l: &str, r: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
            self.inner.copy_recursive(t, l, r, timeout).await
        }
    }

    /// Review (high): a pod was switched — even with `--hard` — when its push failed and its copy
    /// failed too, and the switch ran with no lock held. Now each pod's lock is held from its
    /// backup through its switch (seen at every SSH call: the backup, the copy's question, the
    /// switch), and a pod with neither a push nor a copy is not switched at all, said, and the
    /// run fails. One whose backup failed before the push (no repo) isn't copied, so isn't
    /// switched either.
    #[tokio::test]
    async fn set_branch_never_switches_a_pod_with_neither_a_push_nor_a_copy_and_holds_each_lock_through_its_switch() {
        let failing = StubRsync::new("setbranch-copyfail");
        arena_core::remote::write_test_script(
            failing.0.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             case \"$last\" in *devtest-bloom*) echo 'rsync: No space left on device (28)' >&2; exit 11;; esac\n",
        )
        .unwrap();
        let base = failing.0.join("backups");
        let cfg = backup_cfg(&base);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
        fake.script(
            &host(22002),
            [FakeReply::exit(1, "error: failed to push some refs").with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n"), repo_in_place(), FakeReply::ok()],
        );
        fake.script(&host(22003), [FakeReply::exit(2, "sh: 1: cd: can't cd to /root/ARENA_materials"), FakeReply::ok()]);
        let probe = Arc::new(LockProbe { inner: fake.clone(), dir: base.display().to_string(), seen: Default::default() });
        let err = handle_set_branch(probe.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &failing.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("2 pod(s) NOT switched — each one's line says why: "), "{msg}");
        assert!(msg.contains("devtest-bloom") && msg.contains("devtest-cloud") && !msg.contains("devtest-apple"), "{msg}");
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup("apple"), (hard.clone(), Some(BRANCH_TIMEOUT))]);
        // bloom: backed up, its copy tried (and failed) — never switched.
        assert_eq!(execs(&fake, 22002), [backup("bloom"), (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT))]);
        // cloud: no repo — nothing to copy, never switched.
        assert_eq!(execs(&fake, 22003), [backup("cloud")]);
        // The lock, held at every call — the backup, the copy's question, the switch — and free after.
        assert_eq!(probe.held(22001), [true, true]);
        assert_eq!(probe.held(22002), [true, true]);
        assert_eq!(probe.held(22003), [true]);
        for pod in ["devtest-apple", "devtest-bloom", "devtest-cloud"] {
            let f = arena_core::pull::open_pod_lock(&base.display().to_string(), pod).unwrap();
            assert!(crate::pods::lock::lock_settles_free(f), "{pod}'s lock still held after the run");
        }
    }

    /// Review (high): a pod whose lock another backup held for the whole wait was switched anyway,
    /// unlocked, alongside that backup. Now it isn't touched at all: no backup, no switch, said,
    /// and the run fails. The others go ahead.
    #[tokio::test(start_paused = true)]
    async fn set_branch_never_switches_a_pod_another_backup_holds() {
        let dir = std::env::temp_dir().join(format!("arena-setbranch-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = backup_cfg(&dir);
        let d = dir.display().to_string();
        let held = arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&d, "devtest-bloom").unwrap()).unwrap().expect("free");
        let fake = Arc::new(FakeRemote::new());
        let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "1 pod(s) NOT switched — each one's line says why: devtest-bloom"
        );
        assert!(fake.calls_to(&host(22002)).is_empty(), "bloom touched: {:?}", fake.calls_to(&host(22002)));
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        for (name, port) in [("apple", 22001), ("cloud", 22003)] {
            let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
            assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
        }
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live (bsec B1): with the legacy relative `./backup`, set-branch took no per-pod lock (by
    /// design: RR5) — yet its dry run said each pod is switched "under the pod's backup lock". Now
    /// the dry run, and the run itself before its question, say in one line that no locks are
    /// taken and how to get them, whenever [`take_pod_locks`] takes none; with an absolute dir
    /// that exists, the lock claim stands and that line is never said. The locking is unchanged.
    #[tokio::test(start_paused = true)]
    async fn a_switch_without_per_pod_locks_says_so_and_never_claims_them() {
        let dir = std::env::temp_dir().join(format!("arena-switch-locks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let missing = dir.join("not-made-yet").display().to_string();
        let relative = "(no per-pod backup locks: LOCAL_BACKUP_DIR (./backup) is relative — set an absolute path to keep other \
                        backups out during the switch)"
            .to_string();
        let cases = [
            (cfg(), Some(relative.clone())),
            (
                Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nLOCAL_BACKUP_DIR={missing}\n")),
                Some(format!(
                    "(no per-pod backup locks: LOCAL_BACKUP_DIR ({missing}) doesn't exist — create it to keep other backups out \
                     during the switch)"
                )),
            ),
            (backup_cfg(&dir), None),
        ];
        let names = vec!["devtest-apple".to_string()];
        for (cfg, line) in &cases {
            let note = push_first_note(cfg, false);
            let dir = local_backup_dir(cfg);
            match line {
                Some(line) => {
                    assert!(note.ends_with(&format!(")\n{line}")), "{note}");
                    assert!(!note.contains("backup lock, held") && !note.contains("whose lock"), "{note}");
                }
                None => {
                    assert!(note.contains("keys redacted — under the pod's backup lock, held until its switch ends; a pod whose push"), "{note}");
                    assert!(note.contains("neither pushed nor copied, or whose lock another backup holds, is NOT switched"), "{note}");
                    assert!(!note.contains("no per-pod backup locks"), "{note}");
                }
            }
            // What's said is what's done.
            let locks = take_pod_locks(&dir, &names, Duration::ZERO).await;
            assert_eq!(locks.holds("devtest-apple"), line.is_none(), "{dir}");
            drop(locks);
        }
        assert!(!std::path::Path::new(&missing).exists(), "the locks never make the dir");

        // The run: the line once, before the question — set-branch and init-branches alike.
        take_said();
        handle_set_branch(Arc::new(FakeRemote::new()), &cfg(), "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap();
        handle_init_branches(Arc::new(FakeRemote::new()), &cfg(), Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap();
        let said = take_said();
        assert_eq!(said.iter().filter(|l| **l == relative).count(), 2, "{said:#?}");
        let locked = backup_cfg(&dir);
        handle_set_branch(Arc::new(FakeRemote::new()), &locked, "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap();
        handle_init_branches(Arc::new(FakeRemote::new()), &locked, Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap();
        let said = take_said();
        assert!(!said.iter().any(|l| l.contains("no per-pod backup locks")), "{said:#?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review F5: the dry run said every pod whose push fails "is copied … and then switched" — a
    /// pod mid-merge is copied, then the gentle switch (and init-branches) refuses to end the
    /// merge, while `--hard` ends it, discarding its uncommitted changes to tracked files only.
    /// Each note says which.
    #[test]
    fn the_dry_runs_note_says_what_becomes_of_a_pod_mid_merge() {
        let gentle = push_first_note(&cfg(), false);
        assert!(
            gentle.contains(
                "; a pod with a merge (cherry-pick, revert, `git am`, rebase, conflicts) in progress can't be committed: it is \
                 copied into ./backup too, and then NOT switched: the switch refuses to end it; a pod that can be neither pushed \
                 nor copied"
            ),
            "{gentle}"
        );
        let hard = push_first_note(&cfg(), true);
        assert!(
            hard.contains(
                "it is copied into ./backup too, and the --hard switch then ends it, discarding its uncommitted changes to tracked \
                 files; so is a pod on main/master or a detached HEAD, which no backup commits, with an operation in progress, \
                 changes to tracked files, commits no remote has or untracked files (the --hard switch overwrites any the new \
                 branch tracks); on every pod, what the --hard reset discards is saved there first, in \
                 .git/arena-discarded/<time>/; "
            ),
            "{hard}"
        );
        assert!(!hard.contains("refuses") && !hard.contains("untracked files stay"), "{hard}");
    }

    #[tokio::test(start_paused = true)]
    async fn set_branch_dry_run_runs_nothing_and_apply_runs_the_checkout_on_each_pod() {
        let fake = Arc::new(FakeRemote::new());
        handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, true, true, "rsync").await.unwrap();
        assert!(fake.calls().is_empty(), "dry run: {:?}", fake.calls());

        // Gentle, whole fleet: each pod's branch backed up (pushed) first, then the ff-only
        // checkout, each within its budget.
        handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true, "rsync").await.unwrap();
        let gentle = checkout_command(REPO, "main", Some(KEY), false);
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
            assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (gentle.clone(), Some(BRANCH_TIMEOUT))], "port {port}");
        }

        // --hard on one pod: only that pod, the destructive command (after its backup).
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "main", "bloom", "--hard"]), &fleet(), fake.clone(), &cfg(), true).await.unwrap();
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        assert!(hard.contains("reset --hard"));
        assert_eq!(fake.calls().len(), 2);
        assert_eq!(execs(&fake, 22002)[1], (hard, Some(BRANCH_TIMEOUT)));

        // Concurrent: a wedged pod costs the run one budget (not one per pod after it), fails it.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
        let start = Instant::now();
        let err =
            handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true, "rsync").await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
        assert_eq!(start.elapsed(), BRANCH_TIMEOUT);
        assert_eq!(fake.calls().len(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn init_branches_runs_each_pods_branch_command_within_the_budget() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::exit(1, "error: failed to push some refs")]);
        let err =
            handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to init branch");
        let bcfg = BackupConfig::from_config(&cfg(), 1, 2);
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let want = init_branch_command(&bcfg, &format!("devtest-{name}"));
            assert!(want.contains(&format!("autocommit-devtest-w1d2-{name}")));
            // The branch it's on pushed first (a no-op on main), then the new day's branch.
            let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
            assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (want, Some(BRANCH_TIMEOUT))]);
        }
        let fake = Arc::new(FakeRemote::new());
        handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), true, true, "rsync").await.unwrap();
        assert!(fake.calls().is_empty(), "dry run");
    }

    /// Live (bsec O1): init-branches onto a day branch that already exists said nothing of the
    /// keys set aside on it. Its command now puts them back ([`init_branch_command`]); its line
    /// says so, as set-branch's does.
    #[tokio::test(start_paused = true)]
    async fn init_branches_says_which_set_aside_keys_came_back() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::stdout("Switched to branch 'x'\nUNREDACTED_RESTORED ex/solutions.py\n")]);
        take_said();
        handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet_of(&[("apple", 22001)])), false, true, "rsync").await.unwrap();
        let said = take_said();
        assert!(
            said.iter().any(|l| l
                == "[1/1] ✓ devtest-apple -> autocommit-devtest-w1d2-apple — real key(s) back in 1 file(s) git has redacted: \
                    ex/solutions.py"),
            "{said:#?}"
        );
        assert!(init_branch_command(&BackupConfig::from_config(&cfg(), 1, 2), "devtest-apple").contains("unredacted_restore "));
    }

    /// Review claude #2 (FIX9B item 2): init-branches no longer pushes commits origin doesn't have
    /// (no backup has scanned them). Its line says so, and that the next backup scans and pushes
    /// them — the run still succeeds: the day's branch is made and on origin.
    #[tokio::test(start_paused = true)]
    async fn init_branches_says_what_it_left_for_the_next_backup() {
        let fake = Arc::new(FakeRemote::new());
        let at = "89abcdef0123456789abcdef0123456789abcdef";
        fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::stdout(&format!("Switched to a new branch 'x'\nUNPUSHED 2 {at}\n"))]);
        take_said();
        handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet_of(&[("apple", 22001)])), false, true, "rsync").await.unwrap();
        let said = take_said();
        assert!(
            said.iter().any(|l| l
                == "[1/1] ✓ devtest-apple -> autocommit-devtest-w1d2-apple — 2 commit(s) origin doesn't have stay on it on the pod, \
                    unpushed (the branch went to origin at 89abcdef01, the newest commit origin has): the next backup scans them for \
                    keys and pushes them"),
            "{said:#?}"
        );
    }

    /// A local file to copy, removed on drop.
    struct TmpFile(std::path::PathBuf);
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cp_runs_mkdir_then_copy_then_check_and_stops_a_pod_at_its_first_failure() {
        let file = TmpFile(std::env::temp_dir().join(format!("arena-cp-test-{}.txt", std::process::id())));
        std::fs::write(&file.0, "hello").unwrap();
        let local = file.0.to_string_lossy().into_owned();
        let f = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 5\n")]);
        fake.script(&host(22002), [FakeReply::exit(1, "mkdir: cannot create directory '/root/x': Permission denied")]);
        fake.script(&host(22003), [FakeReply::ok(), FakeReply::exit(1, "scp: /root/x/hello.txt: No space left on device")]);
        fake.script(&host(22004), [FakeReply::ok(), FakeReply::hang()]);
        let start = Instant::now();
        let err = handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x/hello.txt"), false, &everyone(&f), None, false, true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "3 pod(s) failed to receive the file");
        // 5 bytes to 4 pods: the base budget.
        assert_eq!(start.elapsed(), CP_BASE_TIMEOUT, "the stuck scp costs its budget, nothing more");

        let mkdir = RemoteCall::Exec { host: host(22001), cmd: "mkdir -p '/root/x'".into(), timeout: Some(PROBE_TIMEOUT) };
        let copy = |port| RemoteCall::Copy {
            host: host(port),
            local: local.clone(),
            remote: "/root/x/hello.txt".into(),
            recursive: false,
            timeout: Some(CP_BASE_TIMEOUT),
        };
        let apple = fake.calls_to(&host(22001));
        assert_eq!(apple[..2], [mkdir, copy(22001)]);
        assert!(
            matches!(&apple[2], RemoteCall::Exec { cmd, timeout, .. } if cmd.contains("wc -c") && *timeout == Some(PROBE_TIMEOUT)),
            "{apple:?}"
        );
        assert_eq!(apple.len(), 3);
        assert_eq!(fake.calls_to(&host(22002)).len(), 1, "a failed mkdir stops the pod before the copy");
        assert_eq!(fake.calls_to(&host(22003)).len(), 2, "a failed copy is not size-checked");
        assert_eq!(fake.calls_to(&host(22004))[1], copy(22004));
        assert_eq!(fake.calls_to(&host(22004)).len(), 2);

        // `-r`: the tree goes through copy_recursive, with no single-file size check (and
        // `-t` scopes it to one pod).
        let fake = Arc::new(FakeRemote::new());
        let local = file.0.to_str().unwrap();
        handle_pods(pods(&["cp", "-r", "-t", "apple", local, "/root/x/"]), &f, fake.clone(), &cfg(), true).await.unwrap();
        assert!(matches!(
            &fake.calls()[..],
            [RemoteCall::Exec { cmd, .. }, RemoteCall::Copy { recursive: true, remote, .. }] if cmd == "mkdir -p '/root/x'" && remote == "/root/x/"
        ), "{:?}", fake.calls());
    }

    #[tokio::test(start_paused = true)]
    async fn cp_says_why_a_pod_failed() {
        let plan = CopyPlan {
            local: "/l/hello.txt".into(),
            remote: "/root/x/hello.txt".into(),
            remote_parent: "/root/x".into(),
            recursive: false,
            expect_size: Some(5),
            basename: "hello.txt".into(),
            timeout: CP_BASE_TIMEOUT,
        };
        let t = SshTarget::from_pod(&pod("apple", 22001), &cfg()).unwrap();
        let fake = FakeRemote::new();
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
        assert_eq!(copy_to_pod(&fake, &t, &plan).await, Err("timed out after 600s".to_string()));
        fake.script(&host(22001), [FakeReply::exit(1, "Permission denied")]);
        assert_eq!(copy_to_pod(&fake, &t, &plan).await, Err("mkdir failed: Permission denied".to_string()));
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 3")]);
        assert!(copy_to_pod(&fake, &t, &plan).await.unwrap_err().starts_with("size mismatch after copy: 3B on pod vs 5B"));
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("MISPLACED")]);
        assert!(copy_to_pod(&fake, &t, &plan).await.unwrap_err().contains("is a directory on the pod"));
        // A check that can't run (here: wedged) doesn't override a successful scp.
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
        assert_eq!(copy_to_pod(&fake, &t, &plan).await, Ok(()));
    }

    #[test]
    fn cp_budget_scales_with_bytes_times_pods() {
        let base = CP_BASE_TIMEOUT.as_secs();
        let cases: &[(u64, usize, u64)] = &[
            (0, 1, base),
            (5, 4, base),
            (999_999, 1, base),
            (1_000_000, 1, base + 1),
            // The regression: 500 MB to 30 pods (~750s at 20 MB/s) no longer dies at 600s.
            (500_000_000, 30, base + 15_000),
            // Bounded at a day, never overflowing.
            (u64::MAX, 1000, 86_400),
        ];
        for &(bytes, pods, want) in cases {
            assert_eq!(cp_timeout(bytes, pods), Duration::from_secs(want), "{bytes}B × {pods}");
        }
    }

    #[test]
    fn local_size_is_a_file_or_a_trees_total() {
        let dir = TmpFile(std::env::temp_dir().join(format!("arena-cp-size-{}", std::process::id())));
        std::fs::create_dir_all(dir.0.join("sub/deeper")).unwrap();
        std::fs::write(dir.0.join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.0.join("sub/b.bin"), vec![0u8; 2000]).unwrap();
        std::fs::write(dir.0.join("sub/deeper/c"), vec![0u8; 30]).unwrap();
        assert_eq!(local_size(&dir.0.join("a.txt")), 100);
        assert_eq!(local_size(&dir.0), 2130);
        assert_eq!(local_size(&dir.0.join("missing")), 0);
        #[cfg(unix)]
        {
            // A symlinked dir isn't followed (a loop can't hang it); a symlinked file counts.
            std::os::unix::fs::symlink(&dir.0, dir.0.join("sub/loop")).unwrap();
            std::os::unix::fs::symlink(dir.0.join("a.txt"), dir.0.join("sub/a-link")).unwrap();
            assert_eq!(local_size(&dir.0), 2230);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cp_budget_follows_the_size_or_the_timeout_flag() {
        // 3 MB to 2 pods: base + 6s by default.
        let file = TmpFile(std::env::temp_dir().join(format!("arena-cp-budget-{}.bin", std::process::id())));
        std::fs::write(&file.0, vec![0u8; 3_000_000]).unwrap();
        let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let copy_timeouts = |fake: &FakeRemote| -> Vec<Option<Duration>> {
            fake.calls()
                .into_iter()
                .filter_map(|c| match c {
                    RemoteCall::Copy { timeout, .. } => Some(timeout),
                    _ => None,
                })
                .collect()
        };
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
        fake.script(&host(22002), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 3000000")]);
        let start = Instant::now();
        let err = handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x.bin"), false, &everyone(&f), None, false, true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to receive the file");
        let want = CP_BASE_TIMEOUT + Duration::from_secs(6);
        assert_eq!(start.elapsed(), want);
        assert_eq!(copy_timeouts(&fake), [Some(want), Some(want)]);

        // `--timeout` overrides it.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
        let start = Instant::now();
        let flag = Some(Duration::from_secs(30));
        handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x.bin"), false, &everyone(&f), flag, false, true)
            .await
            .unwrap_err();
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        assert_eq!(copy_timeouts(&fake), [flag, flag]);
    }

    #[test]
    fn cp_timeout_flag_parses_within_range() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        match parse(&["arena", "pods", "cp", "--timeout", "3600", "big.tar", "/root/"]).unwrap() {
            Cmd::Pods(PodCmd::Cp { timeout, .. }) => assert_eq!(timeout, Some(3600)),
            _ => panic!("not pods cp"),
        }
        assert!(matches!(parse(&["arena", "pods", "cp", "f"]).unwrap(), Cmd::Pods(PodCmd::Cp { timeout: None, .. })));
        for bad in ["0", "86401", "ten"] {
            assert!(parse(&["arena", "pods", "cp", "--timeout", bad, "f"]).is_err(), "{bad}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn list_probe_overrides_the_gpu_where_a_pod_answers_and_a_wedged_pod_costs_only_its_budget() {
        let fake = Arc::new(FakeRemote::new());
        let smi = format!(
            "NVIDIA RTX A5000, 3, 10, 24564, 40\nNVIDIA RTX A5000, 0, 10, 24564, 39\n{}\n",
            arena_core::metrics::SENTINEL
        );
        fake.script(&host(22001), [FakeReply::stdout(&smi)]);
        fake.script(&host(22002), [FakeReply::hang()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let mut pods = fleet().pods;
        let start = Instant::now();
        probe_gpus(&remote, &cfg(), &mut pods).await;
        assert_eq!(start.elapsed(), PROBE_TIMEOUT);
        let gpu = |i: usize| (pods[i].gpu_type.clone().unwrap(), pods[i].gpu_count.unwrap());
        assert_eq!(gpu(0), ("2×RTX A5000".to_string(), 2), "what nvidia-smi saw wins");
        assert_eq!(gpu(1), ("RTX A4000".to_string(), 1), "no answer: the provider's GPU stays");
        assert_eq!(gpu(2), ("RTX A4000".to_string(), 1), "no GPU rows: the provider's GPU stays");
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
    }

    /// Finding #24: under MIG nvidia-smi names the parent card (`NVIDIA B300 SXM6 AC`), so
    /// the table showed `1×B300 SXM6 AC`; a pod the provider lists as a MIG slice keeps the
    /// slice's name (the count still comes from the probe).
    #[tokio::test]
    async fn list_probe_keeps_a_mig_slices_name() {
        let fake = Arc::new(FakeRemote::new());
        let smi = format!("NVIDIA B300 SXM6 AC, 0, 16, 31232, 43\n{}\n", arena_core::metrics::SENTINEL);
        fake.script(&host(22001), [FakeReply::stdout(&smi)]);
        let remote: Arc<dyn Remote> = fake.clone();
        let mut mig = pod("apple", 22001);
        mig.gpu_type = Some("B300 MIG 34GB".into());
        let mut pods = vec![mig];
        probe_gpus(&remote, &cfg(), &mut pods).await;
        assert_eq!((pods[0].gpu_type.as_deref(), pods[0].gpu_count), (Some("B300 MIG 34GB"), Some(1)));
        assert_eq!(arena_core::fleet::gpu_label(&pods[0]), "1×B300 MIG 34GB");
    }

    /// Two pods with one name (e.g. a half-finished replace) each keep their own GPU: the
    /// probe used to key results by name, so one pod's readout landed on both rows.
    #[tokio::test]
    async fn list_probe_keeps_each_pods_gpu_when_two_pods_share_a_name() {
        let fake = Arc::new(FakeRemote::new());
        let smi = |gpu: &str| format!("{gpu}, 3, 10, 24564, 40\n{}\n", arena_core::metrics::SENTINEL);
        fake.script(&host(22001), [FakeReply::stdout(&smi("NVIDIA RTX A5000"))]);
        fake.script(&host(22002), [FakeReply::stdout(&smi("NVIDIA GeForce RTX 3070"))]);
        let remote: Arc<dyn Remote> = fake.clone();
        let mut twin = pod("apple", 22002);
        twin.id = "id-devtest-apple-2".into();
        let mut pods = vec![pod("apple", 22001), twin];
        probe_gpus(&remote, &cfg(), &mut pods).await;
        assert_eq!(pods[0].gpu_type.as_deref(), Some("1×RTX A5000"));
        assert_eq!(pods[1].gpu_type.as_deref(), Some("1×RTX 3070"));
    }

    #[tokio::test(start_paused = true)]
    async fn replace_identity_and_marker_probes_fail_closed_and_are_bounded() {
        let f = fleet_of(&[("apple", 22001), ("apple-new", 22009)]);
        let t = SshTarget::from_pod(&f.pods[0], &cfg()).unwrap();
        let fake = FakeRemote::new();
        fake.script(&host(22001), [FakeReply::stdout("id-devtest-apple\n"), FakeReply::stdout("id-someone-else\n")]);
        assert!(target_is_pod(&fake, &t, "id-devtest-apple", &f).await);
        assert!(!target_is_pod(&fake, &t, "id-devtest-apple", &f).await, "a recycled ip:port reaching another pod");
        fake.script(&host(22001), [FakeReply::hang()]);
        let start = Instant::now();
        assert!(!target_is_pod(&fake, &t, "id-devtest-apple", &f).await, "couldn't confirm = not the pod");
        assert_eq!(start.elapsed(), PROBE_TIMEOUT);

        let dest = "id-devtest-apple-new";
        fake.script(
            &host(22009),
            [
                FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")),
                FakeReply::stdout(&format!("ID=\nMARK=arena-replace-{dest}\n")),
                FakeReply::hang(),
            ],
        );
        assert!(marker_present(&f, &fake, dest, &cfg()).await);
        assert!(!marker_present(&f, &fake, dest, &cfg()).await, "RunPod without a pod id fails closed");
        let start = Instant::now();
        assert!(!marker_present(&f, &fake, dest, &cfg()).await);
        assert_eq!(start.elapsed(), PROBE_TIMEOUT);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
    }

    /// The via-local copy (live findings #13 and #30), driven through `copy_pod_files` with a
    /// stand-in rsync: two pods behind one public IP skip the direct copy (it hangs on the
    /// hairpin) instead of losing minutes to it; a direct copy that fails says its own reason
    /// and falls back; the staging copy — a participant's whole home on a shared box — is
    /// private, REMOVED once the copy landed, KEPT (with where and how big) when it didn't so
    /// a re-run continues, and never started when the disk can't take it.
    #[tokio::test]
    async fn the_via_local_copy_cleans_up_its_staging_and_never_fills_the_disk() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("arena-via-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A stand-in rsync: a dry run reports `size`; a pull writes a file into the stage; a
        // push (to `user@host:`) exits `push`.
        let stub = |name: &str, size: &str, push: u8| {
            let path = root.join(name);
            arena_core::remote::write_test_script(
                &path,
                format!(
                    "#!/bin/sh\ndry=; for a in \"$@\"; do last=$a; [ \"$a\" = --dry-run ] && dry=1; done\n\
                     if [ -n \"$dry\" ]; then printf 'Number of files: 3\\nTotal transferred file size: {size} bytes\\n'; exit 0; fi\n\
                     case \"$last\" in\n  *@*) [ {push} = 0 ] || echo 'rsync: connection unexpectedly closed' >&2; exit {push};;\n\
                     \x20 *) mkdir -p \"$last\" && printf 'work\\n' > \"$last/work.py\";;\nesac\n"
                ),
            )
            .unwrap();
            path.display().to_string()
        };
        let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
        let fake_for = |dest_host: &str| {
            let fake = FakeRemote::new();
            // (every step on either pod carries its identity check: the specific answers first)
            fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
            fake.on(&host(22001), "rsync", FakeReply::exit(255, "ssh: connect to host 10.0.0.2 port 22002: Connection timed out"));
            fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
            fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
            fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
            fake
        };
        let opts = |rsync: String| super::CopyOpts { fresh: false, rsync, stage_root: root.clone() };
        let stage = root.join(format!("arena-replace-{src}"));

        // Same public IP: no direct attempt; the copy lands and the stage is gone.
        let f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        let fake = fake_for(&host(22002));
        copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("ok", "1,000", 0))).await.unwrap();
        assert!(!execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "no direct copy through a shared IP");
        assert!(!stage.exists(), "the staging copy is removed once the copy landed");

        // Its own IP: the direct copy is tried, fails, falls back; the push fails — the stage
        // is kept, private, and the error says where it is and how big.
        let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        f.pods[1].ssh_ip = Some("10.0.0.2".into());
        let fake = fake_for("10.0.0.2:22002");
        let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("push-fails", "1,000", 12))).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(execs(&fake, 22001).iter().filter(|(c, _)| c.contains("rsync ")).count() == 1, "one direct attempt");
        assert!(msg.contains(&format!("kept at {}", stage.display())) && msg.contains("rm -rf"), "{msg}");
        assert!(msg.contains("connection unexpectedly closed"), "{msg}");
        assert!(stage.join("work.py").exists());
        assert_eq!(std::fs::metadata(&stage).unwrap().permissions().mode() & 0o777, 0o700, "a participant's home: private");
        std::fs::remove_dir_all(&stage).unwrap();

        // More than the disk can spare (the one free-space judge, CONTROL_MIN_FREE_GB — here 0:
        // only the measured copy must fit): refused before anything is staged.
        let fake = fake_for("10.0.0.2:22002");
        let judged = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=0\n", cfg_text()));
        let e = copy_pod_files(&judged, &f, &fake, src, dest, &opts(stub("huge", "999.00P", 0))).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains(&format!("not copying via local staging ({}): skipped: ", root.display())), "{msg}");
        assert!(msg.contains("GB, measured) would leave less than the floor (0 GB, CONTROL_MIN_FREE_GB) — nothing copied; free at least "), "{msg}");
        // How much to free and where, what's there (arena's biggest dirs, if any), and another
        // filesystem to stage on: none has room for ~999 PB.
        let room = |msg: &str| msg.contains("(arena's biggest dirs there: ") || msg.contains("(`arena config check` names arena's biggest dirs there)");
        assert!(room(&msg), "{msg}");
        let anywhere = "; or stage it on another filesystem for this run: `TMPDIR=<a dir on a disk with room> arena …`";
        assert!(msg.ends_with(anywhere), "{msg}");
        assert!(!stage.exists(), "nothing staged, nothing left behind");
        // A box already under the floor: refused before the home is sized (an rsync dry run over
        // all of it, for nothing) or anything is made — saying how far below the floor it is.
        let fake = fake_for("10.0.0.2:22002");
        let full = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=100000\n", cfg_text()));
        let ran = root.join("rsync-ran");
        let never = root.join("never");
        arena_core::remote::write_test_script(&never, format!("#!/bin/sh\ntouch {}\nexit 1\n", ran.display())).unwrap();
        let e = copy_pod_files(&full, &f, &fake, src, dest, &opts(never.display().to_string())).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("skipped: only ") && msg.contains("(floor 100000 GB, CONTROL_MIN_FREE_GB) — nothing copied; free at least "), "{msg}");
        assert!(room(&msg) && msg.ends_with(anywhere), "{msg}");
        assert!(!ran.exists(), "no rsync at all: nothing sized");
        assert!(!stage.exists());
        // A floor that can't be read fails the copy before anything is asked or copied (replace
        // and migrate refuse it before they create anything: `disk::floor_refusal`).
        let bad = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=2.5\n", cfg_text()));
        let fake = fake_for("10.0.0.2:22002");
        let e = copy_pod_files(&bad, &f, &fake, src, dest, &opts(stub("bad", "1,000", 0))).await.unwrap_err();
        assert!(format!("{e:#}").contains("CONTROL_MIN_FREE_GB must be a whole number of GB"), "{e:#}");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // A stage another run holds (a second replace/migrate of the same pod): refused, and that
        // run's stage left exactly as it is.
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("theirs.py"), "x").unwrap();
        let held = arena_core::disk::lock_dir(&stage).unwrap();
        let fake = fake_for("10.0.0.2:22002");
        let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("held", "1,000", 0))).await.unwrap_err();
        assert!(format!("{e:#}").contains("another arena run is copying id-devtest-apple through"), "{e:#}");
        assert!(stage.join("theirs.py").exists() && !stage.join("work.py").exists());
        // An earlier attempt's stage is kept when a later copy lands directly — it may hold what
        // the source has lost since — and stays visible (`config check`, `teardown --check`).
        drop(held);
        let fake = FakeRemote::new();
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on("10.0.0.2:22002", "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        fake.on("10.0.0.2:22002", "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
        copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("direct", "1,000", 0))).await.unwrap();
        assert!(execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "copied directly");
        assert!(stage.join("theirs.py").exists(), "an earlier attempt's stage is never removed by a later copy");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn a_direct_pod_copy_that_times_out_stops_instead_of_falling_back() {
        let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        f.pods[1].ssh_ip = Some("10.0.0.2".into()); // its own host: the direct copy is tried
        let fake = FakeRemote::new();
        // identity check, plant the marker (+ the source's sync record), the direct rsync
        // (wedged), clean the marker.
        fake.script(&host(22001), [FakeReply::stdout("id-devtest-apple"), FakeReply::stdout("arena-replica-sent=yes\n"), FakeReply::hang()]);
        fake.on("10.0.0.2:22002", "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        let start = Instant::now();
        let err = copy_pod_files(&cfg(), &f, &fake, "id-devtest-apple", "id-devtest-apple-new", &super::CopyOpts::new(false))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("direct pod-to-pod copy timed out after 7200s — NOT swapping"), "{msg}");
        assert_eq!(start.elapsed(), POD_COPY_TIMEOUT);
        let src = execs(&fake, 22001);
        assert_eq!(src.len(), 4, "{src:?}");
        assert!(src[2].0.contains("\nrsync ") && src[2].1 == Some(POD_COPY_TIMEOUT), "{src:?}");
        assert!(src[2].0.starts_with("if ! [ ") && src[2].0.contains("= 'id-devtest-apple' ]"), "only on the source itself: {src:?}");
        // The repo goes as its tree (a link onto a volume on either pod is resolved, not copied).
        assert!(src[2].0.contains("'/ARENA_materials'") && src[2].0.contains("'root@10.0.0.2:ARENA_materials/'"), "{src:?}");
        assert!(src[3].0.contains("rm -f ") && !src[3].0.contains("sent-to") && src[3].1 == Some(PROBE_TIMEOUT), "source marker cleaned, nothing recorded: {src:?}");
        // On the destination: no via-local push, no delivery check — the prepare, and the
        // tidy-up of what the partial sync moved aside, which keeps the old baseline.
        let dest = fake.calls_to("10.0.0.2:22002");
        assert!(
            matches!(&dest[..], [RemoteCall::Exec { cmd: prep, .. }, RemoteCall::Exec { cmd, .. }]
                if prep.contains("arena-replica-ready") && cmd.contains("arena-replica-kept") && cmd.contains("del=0") && !cmd.contains("touch")),
            "{dest:?}"
        );
        // It doesn't promise a plain re-run continues: `replace` refuses while `-new` exists.
        assert!(!msg.contains("Re-run"), "{msg}");
    }

    /// Review finding (critical): the original was reset since `-new`'s last sync — its home
    /// is the image again — and the re-sync mirrored that over `-new`, deleting the
    /// participant's files there (`migrate cutover` runs it by default). The source's record
    /// of the sync went with its home (or its container is newer than the stamp): the sync is
    /// refused before ANY transfer, `-new` is never tidied, and the error says what's left.
    #[tokio::test]
    async fn a_resync_from_a_reset_original_is_refused_before_anything_moves() {
        let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
        let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        f.pods[1].ssh_ip = Some("10.0.0.2".into());
        let dest_host = "10.0.0.2:22002";
        for (source, why) in [
            ("arena-replica-sent=no\narena-replica-container=1791000000\n", "has no record of devtest-apple-new's last sync"),
            ("arena-replica-sent=yes\narena-replica-container=1791500000\n", "was reset since devtest-apple-new's last sync"),
        ] {
            let fake = FakeRemote::new();
            fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout(source));
            fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
            fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-last-sync=1791457323\narena-replica-ready\n"));
            let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &super::CopyOpts::new(false)).await.unwrap_err().to_string();
            assert!(e.starts_with("NOT syncing devtest-apple -> devtest-apple-new") && e.contains(why), "{e}");
            assert!(e.contains("--no-final-sync") && e.contains("Nothing was copied"), "{e}");
            let on_src = execs(&fake, 22001);
            assert!(!on_src.iter().any(|(c, _)| c.contains("rsync ")), "no transfer: {on_src:?}");
            assert!(on_src.last().is_some_and(|(c, _)| c.contains("rm -f") && !c.contains("sent-to")), "marker cleaned, nothing recorded: {on_src:?}");
            let on_dest = fake.calls_to(dest_host);
            assert!(
                matches!(&on_dest[..], [RemoteCall::Exec { cmd, .. }] if cmd.contains("arena-replica-ready")),
                "only the prepare on -new — no tidy-up: {on_dest:?}"
            );
        }
        // A fresh -new (made by this command) has nothing to lose: no record needed.
        let fake = FakeRemote::new();
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-last-sync=1791457323\narena-replica-ready\n"));
        fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
        copy_pod_files(&cfg(), &f, &fake, src, dest, &super::CopyOpts::new(true)).await.unwrap();
        assert!(execs(&fake, 22001).iter().any(|(c, _)| c.contains("\nrsync ") && c.contains("--delete")));
    }

    /// Live R: a failed direct copy printed rsync's `code 255` summary, not why — the ssh line
    /// above it (here a missing pod-to-pod key) is the reason.
    #[test]
    fn a_failed_copy_names_the_ssh_error_not_rsyncs_summary() {
        let epilogue = "rsync: connection unexpectedly closed (0 bytes received so far) [sender]\n\
                        rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]\n";
        for (stderr, want) in [
            (format!("Permission denied (publickey).\r\n{epilogue}"), "Permission denied (publickey)."),
            (format!("Warning: Identity file /root/.ssh/id_ed25519 not accessible: No such file or directory.\nPermission denied (publickey).\n{epilogue}"), "Permission denied (publickey)."),
            (format!("ssh: connect to host 10.0.0.2 port 22002: Connection timed out\n{epilogue}"), "ssh: connect to host 10.0.0.2 port 22002: Connection timed out"),
            (format!("Warning: Permanently added '10.0.0.2' (ED25519) to the list of known hosts.\nHost key verification failed.\n{epilogue}"), "Host key verification failed."),
            // rsync's own complaint is the reason when it is one…
            ("rsync: [receiver] mkdir \"/root/x\" failed: No space left on device (28)\nrsync error: error in file IO (code 11) at main.c(791)\n".into(), "rsync: [receiver] mkdir \"/root/x\" failed: No space left on device (28)"),
            // …and with nothing but the epilogue (and the known-hosts notice), it's what there is.
            (format!("Warning: Permanently added '10.0.0.2' (ED25519) to the list of known hosts.\n{epilogue}"), "rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]"),
            (String::new(), "no error output"),
        ] {
            assert_eq!(copy_failure_reason(&stderr), want, "{stderr:?}");
        }
    }

    /// Live R: the reset refusal gave both times to the minute ("its container dates from
    /// 14:54 UTC, the sync from 14:54 UTC"), which reads as no reset at all. To the second now.
    #[test]
    fn the_reset_refusal_gives_its_times_to_the_second() {
        use arena_core::replica::MirrorRefusal;
        // A sync at 11:02:03, the container reset 20 s later — the same minute.
        let r = MirrorRefusal::SourceReset { container: 1_791_457_343, last_sync: 1_791_457_323 };
        let e = mirror_refusal_text(&r, "devtest-apple", "devtest-apple-new");
        assert!(e.contains("(its container dates from 2026-10-08 11:02:23 UTC, the sync from 2026-10-08 11:02:03 UTC)"), "{e}");
        let e = mirror_refusal_text(&MirrorRefusal::NoRecord { last_sync: 1_791_457_323 }, "a", "b");
        assert!(e.contains("has no record of b's last sync (2026-10-08 11:02:03 UTC)"), "{e}");
    }

    /// Live R: `cutover --no-final-sync` said "(180s ago)" of a sync 235 s old — the age was
    /// floored to minutes and printed in seconds. Now exact, in `up`'s format.
    #[test]
    fn the_last_sync_age_is_exact() {
        let at = 1_791_457_323; // 2026-10-08 11:02:03 UTC
        for (ago, want) in [
            (0, "2026-10-08 11:02 UTC (0s ago)"),
            (59, "2026-10-08 11:02 UTC (59s ago)"),
            (235, "2026-10-08 11:02 UTC (3m55s ago)"),
            (3 * 3600 + 120, "2026-10-08 11:02 UTC (3h02m ago)"),
        ] {
            assert_eq!(sync_time_label(at, at + ago), want, "{ago}");
        }
        // A clock behind the stamp is "0s", never an underflow.
        assert_eq!(sync_time_label(at, at - 5), "2026-10-08 11:02 UTC (0s ago)");
    }

    /// Review finding (critical): the destination's endpoint was used with no identity check
    /// before the mirroring push and the tidy-up — a recycled endpoint would have another
    /// pod's home mirrored over and its files tidied away — and a stopped `-new` (listed with
    /// a stale endpoint) wasn't refused. Now every step on it runs behind its RUNPOD_POD_ID in
    /// the same session: an endpoint that answers as another pod stops the copy at the
    /// prepare, before any transfer, and nothing else runs there.
    #[tokio::test]
    async fn the_destination_is_identity_checked_before_anything_is_written_to_it() {
        let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
        let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        f.pods[1].ssh_ip = Some("10.0.0.2".into());
        let dest_host = "10.0.0.2:22002";
        let fake = FakeRemote::new();
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on(dest_host, "arena-replica-ready", FakeReply::exit(97, "arena: this endpoint is not the expected pod - refusing"));
        let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &super::CopyOpts::new(false)).await.unwrap_err().to_string();
        assert!(e.contains("couldn't prepare devtest-apple-new") && e.contains("doesn't answer as id-devtest-apple-new"), "{e}");
        assert!(e.contains("nothing was copied"), "{e}");
        assert!(!execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "no push");
        let on_dest = fake.calls_to(dest_host);
        assert!(
            matches!(&on_dest[..], [RemoteCall::Exec { cmd, .. }] if cmd.contains("= '\\''id-devtest-apple-new'\\'' ]")),
            "only the guarded prepare, nothing after it: {on_dest:?}"
        );
        // Every later step carries the same check: the push's remote rsync, and the tidy-up.
        let fake = FakeRemote::new();
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
        copy_pod_files(&cfg(), &f, &fake, src, dest, &super::CopyOpts::new(false)).await.unwrap();
        let push = execs(&fake, 22001).into_iter().find(|(c, _)| c.contains("\nrsync ")).unwrap().0;
        assert!(push.contains("--rsync-path=") && push.contains("id-devtest-apple-new"), "{push}");
        let on_dest = fake.calls_to(dest_host);
        assert!(
            on_dest.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("arena-replica-kept") && cmd.contains("RUNPOD_POD_ID"))),
            "{on_dest:?}"
        );
        // A stopped -new is refused outright: its listed endpoint is the one it had.
        f.pods[1].status = "EXITED".into();
        let fake = FakeRemote::new();
        let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &super::CopyOpts::new(false)).await.unwrap_err().to_string();
        assert!(e.contains("the destination devtest-apple-new is stopped (EXITED)") && e.contains("`arena pods start devtest-apple-new`"), "{e}");
        assert!(fake.calls().is_empty(), "nothing reached: {:?}", fake.calls());
    }

    /// The via-local copy's "2 h per leg" is the LEG's budget, not each rsync's: a leg split
    /// into the home and a linked repo's tree must not get the budget twice. Real children
    /// (`sh -c sleep`, standing in for rsync) on a real clock, scaled down: a 2s leg whose
    /// first rsync takes 0.8s leaves the second 1.2s — too little for its 1.6s, which a
    /// per-rsync budget would have let finish (3 s for the leg) — and once the leg is spent
    /// the next rsync isn't started at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_via_local_legs_rsyncs_share_one_budget() {
        let nap = |secs: &str| vec!["-c".to_string(), format!("sleep {secs}")];
        let leg = CopyLeg::start(Duration::from_secs(2));
        let start = Instant::now();
        leg.rsync("sh", &nap("0.8")).await.expect("the home fits in the leg");
        let err = leg.rsync("sh", &nap("1.6")).await.unwrap_err().to_string();
        assert_eq!(err, "rsync timed out after 2s (the copy leg's budget, for all its rsyncs together)");
        let took = start.elapsed();
        assert!(took >= Duration::from_secs(2), "stopped at the leg's deadline, not before: {took:?}");
        assert!(took < Duration::from_millis(2800), "not the repo's own 1.6s on top of the home's: {took:?}");
        // Spent: refused without spawning (a missing program would be a spawn error instead).
        let err = leg.rsync("/nonexistent/rsync", &[]).await.unwrap_err().to_string();
        assert!(err.starts_with("rsync timed out after 2s"), "{err}");
        // A failed rsync is still its own error, with its stderr.
        let err = CopyLeg::start(Duration::from_secs(5))
            .rsync("sh", &["-c".into(), "echo nope >&2; exit 23".into()])
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "rsync failed: nope");
    }

    #[test]
    fn a_failed_replace_copy_says_new_is_left_running_and_how_to_continue() {
        let msg = replace_copy_failed("devtest-apple", "devtest-apple-new");
        assert!(msg.contains("devtest-apple-new is left running (billed)"), "{msg}");
        assert!(msg.contains("`arena pods migrate copy devtest-apple`"), "{msg}");
        assert!(msg.contains("`arena pods migrate cutover devtest-apple`"), "{msg}");
        assert!(msg.contains("`arena pods terminate devtest-apple-new`"), "{msg}");
    }

    #[tokio::test(start_paused = true)]
    async fn proxy_gate_probes_the_stable_port_within_the_probe_budget() {
        let cfg = Config::parse(
            "MACHINE_NAME_PREFIX=devtest\nSSH_PROXY_HOST=proxy.test\nSSH_PROXY_STARTING_PORT=9500\n\
             MACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n)\n",
        );
        let fake = FakeRemote::new();
        fake.script("proxy.test:9501", [FakeReply::stdout("id-new\n"), FakeReply::stdout("id-old\n"), FakeReply::hang()]);
        assert!(proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap());
        assert!(!proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap(), "still the old pod");
        let start = Instant::now();
        assert!(!proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap());
        assert_eq!(start.elapsed(), PROBE_TIMEOUT);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
    }

    #[tokio::test]
    async fn each_pod_reports_a_crashed_job_by_name() {
        let jobs = [("devtest-apple", false), ("devtest-bloom", true)]
            .into_iter()
            .map(|(name, crash)| {
                (name.to_string(), async move {
                    if crash {
                        panic!("boom");
                    }
                    7
                })
            })
            .collect();
        let mut seen = Vec::new();
        each_pod(jobs, |_, total, name, result| seen.push((total, name.to_string(), result))).await;
        seen.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(seen[0], (2, "devtest-apple".to_string(), Ok(7)));
        assert!(matches!(&seen[1], (2, name, Err(e)) if name == "devtest-bloom" && e.starts_with("task crashed: ")), "{seen:?}");
    }

    // ---- `pods test --deep` (PLAN 1.A) ----

    /// What the deep-check script prints on a healthy 2×A4000 pod, after some zshrc
    /// chatter (which the parser must skip).
    pub(crate) const DEEP_HEALTHY: &str = "\
Welcome back! conda env: arena-env
deep_check=1
load1=0.84
cpus=64
nproc=16
uptime_secs=1209600
smi=ok
smi_cuda=13.0
gpu.0.name=NVIDIA RTX A4000
gpu.0.driver=580.65.06
gpu.1.name=NVIDIA RTX A4000
gpu.1.driver=580.65.06
smi_gpus=2
python=/root/miniconda3/envs/arena-env/bin/python
torch=ok
torch_version=2.9.0+cu130
torch_cuda=13.0
cuda_available=true
device_count=2
tensor.0=ok
tensor.1=ok
peer.0-1=ok
peer.1-0=ok
nccl_ranks=2
nccl=ok
py_done=1
py_exit=0
disk.root_avail_kb=104857600
net.curl_exit=0
net.http=206
net.bytes=33554432
net.secs=0.712
deep_check_end=1
";

    /// A bad host: nvidia-smi is fine, CUDA init fails with error 999.
    pub(crate) const DEEP_CUINIT_999: &str = "\
deep_check=1
load1=1.20
cpus=64
smi=ok
smi_cuda=13.0
gpu.0.name=NVIDIA RTX A4000
gpu.0.driver=580.65.06
smi_gpus=1
python=/root/miniconda3/envs/arena-env/bin/python
torch=ok
torch_version=2.9.0+cu130
torch_cuda=13.0
cuda_available=false
cuda_error=error: RuntimeError: Unexpected error from cudaGetDeviceCount(). Did you run some cuda functions before calling NumCudaDevices() that might have already set an error? Error 999: unknown error
device_count=0
py_done=1
py_exit=0
disk.root_avail_kb=104857600
net.curl_exit=0
net.http=206
net.bytes=33554432
net.secs=0.712
deep_check_end=1
";

    fn deep_cfg() -> Config {
        Config::parse(
            "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nALLOWED_CUDA_VERSIONS=\"13.0\"\n",
        )
    }

    /// apple passes, bloom is on a cuInit-999 host, cloud hangs past the budget.
    fn script_deep(fake: &FakeRemote) {
        fake.script(&host(22001), [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(40))]);
        fake.script(&host(22002), [FakeReply::stdout(DEEP_CUINIT_999).after(Duration::from_secs(25))]);
        fake.script(&host(22003), [FakeReply::hang()]);
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_reports_pass_fail_and_timeout() {
        use arena_core::health::{deep_check_command, render_report, Status, DEEP_CHECK_TIMEOUT};
        let fake = Arc::new(FakeRemote::new());
        script_deep(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let start = Instant::now();
        let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
        assert_eq!(start.elapsed(), DEEP_CHECK_TIMEOUT, "ends at the hung pod's budget, not the hang");

        // One exec per pod: the script (base64, inside the conda login wrap), bounded.
        let want = deep_check_command(Some("arena-env"));
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(want.clone(), Some(DEEP_CHECK_TIMEOUT))], "port {port}");
        }

        let statuses: Vec<(&str, Status)> = results.iter().map(|h| (h.name.as_str(), h.status)).collect();
        assert_eq!(
            statuses,
            [("devtest-apple", Status::Pass), ("devtest-bloom", Status::Fail), ("devtest-cloud", Status::Fail)]
        );
        // The table, and — all three pods share 10.0.0.1 — the same-host hint for the two failures.
        assert_eq!(
            render_report(&results, false),
            "\
NAME           RESULT  GPUS         DRIVER     CUDA        NET  NOTES
devtest-apple  pass    2×RTX A4000  580.65.06  13.0  47.1 MB/s
devtest-bloom  fail    1×RTX A4000  580.65.06  13.0  47.1 MB/s  cuda: RuntimeError: Unexpected error from cudaGetDeviceCount(). Error 999: unknown error
devtest-cloud  fail    -            -          -             -  ssh: timed out after 150s

1 pass, 0 warn, 2 fail
same host? 10.0.0.1: 2 failing (devtest-bloom, devtest-cloud). A bad host breaks every pod on it; a different GPU type draws a different host.
"
        );
        // -v adds every check per pod.
        let verbose = render_report(&results, true);
        assert!(verbose.contains("── devtest-apple (pass)\n"), "{verbose}");
        assert!(verbose.contains("  ✓ driver        580.65.06 ≥ 580 (CUDA 13.0)\n"), "{verbose}");
        assert!(verbose.contains("  - peer_copy     needs CUDA\n"), "{verbose}");

        // --json: per-pod {name, provider, status, checks, facts}; no IPs.
        let v = serde_json::to_value(&results).unwrap();
        assert_eq!(v[0]["status"], "pass");
        assert_eq!(v[0]["facts"]["nccl"], "ok");
        assert_eq!(v[1]["provider"], "runpod");
        let cuda = v[1]["checks"].as_array().unwrap().iter().find(|c| c["name"] == "cuda").unwrap();
        assert_eq!(cuda["status"], "fail");
        assert!(cuda["detail"].as_str().unwrap().contains("Error 999: unknown error"));
        assert_eq!(v[2]["checks"], serde_json::json!([{"name": "ssh", "status": "fail", "detail": "timed out after 150s"}]));
        assert!(v[2]["facts"].is_null());
        assert!(!v.to_string().contains("10.0.0.1"));
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_exit_status_follows_failures_not_warnings() {
        use arena_core::health::DEEP_CHECK_TIMEOUT;
        let deep = |json: bool| PodCmd::Test { deep: true, sel: Select::default(), json, verbose: false, cohort: crate::CohortScope::default() };
        // Any FAIL fails the command (table or JSON), at the hung pod's budget.
        for json in [false, true] {
            let fake = Arc::new(FakeRemote::new());
            script_deep(&fake);
            let start = Instant::now();
            let err = handle_pods(deep(json), &fleet(), fake.clone(), &deep_cfg(), true).await.unwrap_err();
            assert_eq!(err.to_string(), "2 pod(s) failed the deep check", "json={json}");
            assert_eq!(start.elapsed(), DEEP_CHECK_TIMEOUT);
        }
        // Warnings alone (a slow network on bloom) don't.
        let slow = DEEP_HEALTHY.replace("net.secs=0.712", "net.secs=30.001").replace("net.bytes=33554432", "net.bytes=12000000");
        let script_slow = |fake: &FakeRemote| {
            fake.script(&host(22001), [FakeReply::stdout(DEEP_HEALTHY)]);
            fake.script(&host(22002), [FakeReply::stdout(&slow)]);
            fake.script(&host(22003), [FakeReply::stdout(DEEP_HEALTHY)]);
        };
        let fake = Arc::new(FakeRemote::new());
        script_slow(&fake);
        handle_deep_test(&fleet(), fake.clone(), &deep_cfg(), &everyone(&fleet()), false, true).await.unwrap();
        // ...and bloom really was a warning, not a pass.
        let fake = Arc::new(FakeRemote::new());
        script_slow(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
        assert_eq!(results[1].status, arena_core::health::Status::Warn, "{:#?}", results[1].checks);
        // A malformed MIN_DRIVER_VERSION fails before any pod is reached.
        let fake = Arc::new(FakeRemote::new());
        let bad = Config::parse("MACHINE_NAME_PREFIX=devtest\nMIN_DRIVER_VERSION=newest\n");
        let err = handle_pods(deep(false), &fleet(), fake.clone(), &bad, true).await.unwrap_err();
        assert!(err.to_string().contains("MIN_DRIVER_VERSION"), "{err}");
        assert!(fake.calls().is_empty());
    }

    /// disk-guard: the daily health check (`pods test --deep` over the cohort, nothing named)
    /// ends with this box — its disk, and the copies skipped for space in the last day, which
    /// fail the check even when every pod passes. A check of named pods says nothing about it.
    #[tokio::test]
    async fn the_health_check_counts_copies_skipped_for_space_as_failures() {
        let dir = std::env::temp_dir().join(format!("arena-deep-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = deep_cfg();
        cfg.values.insert("ARENA_STATE_DIR".into(), dir.display().to_string());
        cfg.values.insert("CONTROL_MIN_FREE_GB".into(), "0".into());
        cfg.values.insert("LOCAL_BACKUP_DIR".into(), dir.join("backup").display().to_string());
        let healthy = || {
            let fake = Arc::new(FakeRemote::new());
            for port in [22001, 22002, 22003] {
                fake.script(&host(port), [FakeReply::stdout(DEEP_HEALTHY)]);
            }
            fake
        };
        handle_deep_test(&fleet(), healthy(), &cfg, &everyone(&fleet()), false, false).await.unwrap();
        // A copy skipped an hour ago: the check fails, and says which and why.
        let now = arena_core::snapshot::unix_now();
        let why = "only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)";
        let record = dir.join("devtest").join(arena_core::disk::SKIPS_FILE);
        arena_core::disk::record_skips(&record, &[("devtest-bloom".into(), "copy".into(), why.into())], now - 3600, Duration::from_secs(5)).unwrap();
        let err = handle_deep_test(&fleet(), healthy(), &cfg, &everyone(&fleet()), false, false).await.unwrap_err();
        assert_eq!(err.to_string(), "copies of 1 pod(s) onto this box were skipped for space in the last day");
        let box_health = crate::disk::health(&cfg, now).unwrap();
        assert_eq!(box_health.skipped, 1);
        assert!(box_health.lines[0].contains("This box"), "{:?}", box_health.lines);
        assert!(box_health.lines.iter().any(|l| l.starts_with("  ✗ devtest-bloom: copy skipped at ") && l.ends_with(why)), "{:?}", box_health.lines);
        assert!(box_health.lines.iter().any(|l| l.starts_with("  ✓ ")), "the disk's own row: {:?}", box_health.lines);
        // Named pods: about them only.
        let apple = picked(&fleet(), &cfg, &["apple"]).await;
        handle_deep_test(&fleet(), healthy(), &cfg, &apple, false, false).await.unwrap();
        // A day on, no longer this check's.
        assert_eq!(crate::disk::health(&cfg, now + 86_400).unwrap().skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pods test --deep` leaves each verdict in the health cache `arena snapshot` reads —
    /// keyed by provider + id — and drops the record of a pod the listing no longer has.
    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_records_verdicts_in_the_health_cache() {
        use arena_core::health::{PodHealth, Status};
        use arena_core::snapshot::{HealthCache, Issue};
        let dir = std::env::temp_dir().join(format!("arena-deep-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = deep_cfg();
        cfg.values.insert("ARENA_STATE_DIR".into(), dir.display().to_string());
        let path = dir.join("devtest").join("health.json");
        // A record of a pod since terminated (runpod now lists without it).
        let gone = Pod { id: "id-gone".into(), name: "devtest-gone".into(), provider: "runpod".into(), ..Default::default() };
        let mut seed = HealthCache::new();
        seed.merge(&[PodHealth::unreachable(&gone, "timed out")], 1);
        seed.save(&path).unwrap();

        let fake = Arc::new(FakeRemote::new());
        script_deep(&fake);
        let err = handle_deep_test(&fleet(), fake.clone(), &cfg, &everyone(&fleet()), true, false).await.unwrap_err();
        assert_eq!(err.to_string(), "2 pod(s) failed the deep check", "recording never changes the verdict");

        let (cache, warning) = HealthCache::load(&path);
        assert_eq!(warning, None);
        let rec = |name: &str| cache.get(&pod(name, 0)).unwrap_or_else(|| panic!("{name}: {cache:#?}"));
        assert_eq!((rec("apple").status, rec("apple").issues.len()), (Status::Pass, 0));
        assert_eq!((rec("bloom").status, rec("bloom").issues.first()), (Status::Fail, Some(&Issue::GpuError)));
        assert_eq!((rec("cloud").status, rec("cloud").issues.as_slice()), (Status::Fail, &[Issue::Unreachable][..]));
        assert_eq!(rec("apple").gpu.as_deref(), Some("2×RTX A4000"), "the machine's own GPU view");
        assert!(cache.get(&gone).is_none(), "pruned: runpod listed OK without it");
        assert_eq!(cache.pods.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_names_scope_the_run_and_report_unreachable_pods() {
        use arena_core::health::Status;
        let remote = |fake: &Arc<FakeRemote>| -> Arc<dyn Remote> { fake.clone() };

        // Bare, full and id forms; only those pods are reached.
        let fake = Arc::new(FakeRemote::new());
        let sel = picked(&fleet(), &deep_cfg(), &["bloom", "id-devtest-cloud"]).await;
        let results = deep_check_fleet(&fleet(), &remote(&fake), &deep_cfg(), &sel).await.unwrap();
        assert_eq!(results.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["devtest-bloom", "devtest-cloud"]);
        assert!(fake.calls_to(&host(22001)).is_empty());

        // A typo fails loudly (through the command), touching nothing.
        let fake = Arc::new(FakeRemote::new());
        let err = handle_pods(pods(&["test", "--deep", "apple", "nope"]), &fleet(), fake.clone(), &deep_cfg(), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`nope`: no pod has that name or id"), "{err}");
        assert!(fake.calls().is_empty());

        // A named pod with no endpoint yet is a FAIL (it was asked about), never silently
        // skipped; ssh refusing the connection is a FAIL with ssh's reason; a script that
        // never started (exit 0, nothing printed) is a FAIL with the pod's stderr.
        let mut f = fleet();
        f.pods[0].ssh_port = None;
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused\n")]);
        fake.script(&host(22003), [FakeReply::exit(0, "\nzsh:1: command not found: base64\n")]);
        let sel = picked(&f, &deep_cfg(), &["apple", "bloom", "cloud"]).await;
        let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
        let got: Vec<(&str, Status, &str, &str)> = results
            .iter()
            .map(|h| (h.name.as_str(), h.status, h.checks[0].name.as_str(), h.checks[0].detail.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("devtest-apple", Status::Fail, "ssh", "no SSH endpoint yet (status RUNNING)"),
                (
                    "devtest-bloom",
                    Status::Fail,
                    "ssh",
                    "exit 255: ssh: connect to host 10.0.0.1 port 22002: Connection refused"
                ),
                (
                    "devtest-cloud",
                    Status::Fail,
                    "script",
                    "no output from the check script (zsh:1: command not found: base64)"
                ),
            ]
        );
        assert!(fake.calls_to(&host(22001)).is_empty());
        // A named STOPPED pod (its last endpoint still listed) is never probed: a FAIL that
        // says it's stopped and how to bring it back (live finding #15).
        let mut f = fleet();
        f.pods[0].status = "EXITED".into();
        let fake = Arc::new(FakeRemote::new());
        let sel = picked(&f, &deep_cfg(), &["apple"]).await;
        let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
        assert_eq!(results[0].checks[0].detail, "stopped (EXITED) — `arena pods start devtest-apple` brings it back");
        assert!(fake.calls().is_empty());
        // Without names, a pod without an endpoint is skipped (with a note), not failed.
        let fake = Arc::new(FakeRemote::new());
        let all: Vec<_> = (0..3).map(|_| FakeReply::stdout(DEEP_HEALTHY)).collect();
        for port in [22002, 22003] {
            fake.script(&host(port), all.clone());
        }
        let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &everyone(&f)).await.unwrap();
        assert_eq!(results.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["devtest-bloom", "devtest-cloud"]);

        // A stopped pod, or one up with only RunPod's SSH proxy, says so — not "yet" (live:
        // `test`/`run` said "no SSH endpoint yet" for an EXITED pod).
        let mut f = fleet();
        (f.pods[0].ssh_ip, f.pods[0].ssh_port, f.pods[0].status) = (None, None, "EXITED".into());
        (f.pods[1].ssh_ip, f.pods[1].ssh_port, f.pods[1].proxy_only) = (None, None, true);
        let fake = Arc::new(FakeRemote::new());
        let sel = picked(&f, &deep_cfg(), &["apple", "bloom"]).await;
        let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
        let why: Vec<&str> = results.iter().map(|h| h.checks[0].detail.as_str()).collect();
        assert_eq!(why[0], "stopped (EXITED) — `arena pods start devtest-apple` brings it back");
        assert!(why[1].starts_with("no public endpoint (RunPod ssh proxy only)"), "{why:?}");
        assert!(fake.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_reports_every_pod_when_two_share_a_name() {
        use arena_core::health::Status;
        // Two `devtest-apple`s (a double create): one on a cuInit-999 host, one healthy.
        // Whichever finishes last, both get their own row and the broken one fails the run.
        for (broken_first, healthy_after, broken_after) in [(true, 5, 1), (false, 1, 5)] {
            let mut f = fleet_of(&[("apple", 22001), ("apple", 22002)]);
            f.pods[0].id = "id-a".into();
            f.pods[1].id = "id-b".into();
            let (healthy_port, broken_port) = if broken_first { (22001, 22002) } else { (22002, 22001) };
            let fake = Arc::new(FakeRemote::new());
            let script = |fake: &FakeRemote| {
                fake.script(&host(healthy_port), [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(healthy_after))]);
                fake.script(&host(broken_port), [FakeReply::stdout(DEEP_CUINIT_999).after(Duration::from_secs(broken_after))]);
            };
            script(&fake);
            let remote: Arc<dyn Remote> = fake.clone();
            let results = deep_check_fleet(&f, &remote, &deep_cfg(), &everyone(&f)).await.unwrap();
            let got: Vec<(&str, &str, Status)> =
                results.iter().map(|h| (h.id.as_str(), h.name.as_str(), h.status)).collect();
            let verdict = |port| if port == healthy_port { Status::Pass } else { Status::Fail };
            assert_eq!(
                got,
                [("id-a", "devtest-apple", verdict(22001)), ("id-b", "devtest-apple", verdict(22002))],
                "broken_first={broken_first}"
            );
            script(&fake);
            let err = handle_deep_test(&f, fake.clone(), &deep_cfg(), &everyone(&f), false, false).await.unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) failed the deep check");
        }
        assert_eq!(
            duplicate_names(&fleet_of(&[("apple", 1), ("bloom", 2), ("apple", 3)]).pods),
            [("devtest-apple".to_string(), vec!["id-devtest-apple".to_string(), "id-devtest-apple".to_string()])]
        );
        assert!(duplicate_names(&fleet().pods).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_json_stdout_is_always_json() {
        // No pod with an endpoint: `[]` on stdout, the note on stderr.
        let (out, err) = render_deep_test(&[], true, false).unwrap();
        assert_eq!(out, "[]\n");
        assert_eq!(err, ["(no pods with an SSH endpoint)"]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), serde_json::json!([]));
        // Without --json the note is the output.
        assert_eq!(render_deep_test(&[], false, false).unwrap(), ("(no pods with an SSH endpoint)\n".into(), vec![]));
        // With results: parseable JSON on stdout, the summary on stderr.
        let fake = Arc::new(FakeRemote::new());
        script_deep(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
        let (out, err) = render_deep_test(&results, true, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert_eq!(err[0], "1 pass, 0 warn, 2 fail");
        // An empty fleet through the command itself: no failure, nothing reached.
        let empty = Fleet { kind: "runpod", pods: vec![] };
        let fake = Arc::new(FakeRemote::new());
        handle_deep_test(&empty, fake.clone(), &deep_cfg(), &everyone(&empty), true, false).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn pods_test_flags_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        assert!(matches!(
            parse(&["arena", "pods", "test"]).unwrap(),
            Cmd::Pods(PodCmd::Test { deep: false, ref sel, json: false, verbose: false, .. }) if sel.targets.is_empty()
        ));
        match parse(&["arena", "pods", "test", "--deep", "apple", "bloom", "--json", "-v"]).unwrap() {
            Cmd::Pods(PodCmd::Test { deep, sel, json, verbose, .. }) => {
                assert!(deep && json && verbose);
                assert_eq!(sel.targets, ["apple", "bloom"]);
            }
            _ => panic!("not pods test"),
        }
        // Plain `pods test` (the torch check) takes targets too now (PLAN 1.C) …
        match parse(&["arena", "pods", "test", "apple..bloom", "--exclude", "cloud"]).unwrap() {
            Cmd::Pods(PodCmd::Test { deep: false, sel, .. }) => {
                assert_eq!((sel.targets, sel.opts.exclude), (vec!["apple..bloom".to_string()], vec!["cloud".to_string()]));
            }
            _ => panic!("not pods test"),
        }
        // … but --json/-v still belong to --deep.
        for args in [&["arena", "pods", "test", "--json"][..], &["arena", "pods", "test", "-v"]] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    /// Which pods a fleet-wide git backup command reaches, and the one-line summaries for the
    /// pods it left out and the cohort pods it found on main (spec items 2 and 3).
    mod cohort_scope {
        use super::super::{
            cohort_only, non_cohort_line, on_protected_line, on_protected_lines, resolve_week_day, restored_note,
            set_branch_reminder, split_cohort, take_said, Cli, Cmd, CohortScope, PodCmd,
        };
        use super::*;

        /// A pod named exactly `full` (no prefix added) at `10.0.0.1:<port>`.
        fn box_pod(full: &str, port: u16) -> Pod {
            Pod { id: format!("id-{full}"), name: full.into(), ..pod("x", port) }
        }

        /// A shared account: two listed cohort pods, an extra `devtest-` pod off the list, a staff
        /// box listed as `@devtest-zebra` (it wears the prefix), a staff box `@staff-gpu`, a pod the
        /// provider named itself, and another cohort's pod.
        fn account() -> Fleet {
            Fleet {
                kind: "runpod",
                pods: vec![
                    pod("apple", 22001),
                    pod("bloom", 22002),
                    pod("extra", 22003),
                    box_pod("devtest-zebra", 22004),
                    box_pod("staff-gpu", 22005),
                    box_pod("quiet_blue_fox", 22006),
                    box_pod("devtest2-apple", 22007),
                ],
            }
        }

        fn account_cfg(extra: &str) -> Config {
            Config::parse(&format!(
                "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom @devtest-zebra @staff-gpu)\n\
                 BACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n{extra}"
            ))
        }

        /// The cohort's three; the line for the other four (in selection order: list slot, then name).
        const COHORT_PORTS: [u16; 3] = [22001, 22002, 22003];
        const LEFT_OUT: &str = "⊘ skipped 4 non-cohort pods (not devtest-…, or an @ staff entry): \
            devtest-zebra, staff-gpu, devtest2-apple, quiet_blue_fox — name one to include it, or pass --include-non-cohort";
        /// What a branch switch says first with [`account_cfg`]'s legacy `./backup` (no locks).
        const NO_LOCKS: &str = "(no per-pod backup locks: LOCAL_BACKUP_DIR (./backup) is relative — set an absolute path to keep \
            other backups out during the switch)";

        fn names(sel: &Selected) -> Vec<&str> {
            sel.pods.iter().map(|p| p.name.as_str()).collect()
        }

        /// A `split_cohort` case: the selection, whether it named pods, the pods kept, the names left out.
        type SplitCase = (Vec<Pod>, bool, Vec<&'static str>, Vec<&'static str>);

        /// An `on_protected_line` case: `(name, branch)` pods, whether the run was fleet-wide, the line.
        type OnMainCase = (&'static [(&'static str, &'static str)], bool, Option<&'static str>);

        #[test]
        fn a_fleet_wide_selection_keeps_this_cohorts_pods_and_names_the_rest() {
            let cfg = account_cfg("");
            let naming = Naming::from_config(&cfg);
            let pods = account().pods;
            // (selection, named?, kept, left out)
            let pick = |names: &[&str]| pods.iter().filter(|p| names.contains(&p.name.as_str())).cloned().collect::<Vec<_>>();
            let cases: Vec<SplitCase> = vec![
                // Fleet-wide: the `devtest-` pods, the off-list extra included; staff boxes (even
                // one wearing the prefix), the provider-named pod and another prefix are left out.
                (
                    pods.clone(),
                    false,
                    vec!["devtest-apple", "devtest-bloom", "devtest-extra"],
                    vec!["devtest-zebra", "staff-gpu", "quiet_blue_fox", "devtest2-apple"],
                ),
                // Named: exactly what was named, staff boxes included.
                (pick(&["devtest-zebra", "staff-gpu", "quiet_blue_fox"]), true, vec!["devtest-zebra", "staff-gpu", "quiet_blue_fox"], vec![]),
                (pick(&["devtest-apple"]), true, vec!["devtest-apple"], vec![]),
                // Fleet-wide with nothing to leave out, and an empty fleet.
                (pick(&["devtest-apple", "devtest-extra"]), false, vec!["devtest-apple", "devtest-extra"], vec![]),
                (vec![], false, vec![], vec![]),
            ];
            for (i, (pods, named, kept, out)) in cases.into_iter().enumerate() {
                let (sel, left) = split_cohort(&naming, Selected { pods, named });
                assert_eq!((names(&sel), sel.named, left), (kept, named, out.iter().map(|s| s.to_string()).collect::<Vec<_>>()), "case {i}");
            }
            // A pod without a name is left out by its id.
            let nameless = Pod { name: String::new(), ..box_pod("x", 22009) };
            assert_eq!(split_cohort(&naming, Selected::all(vec![nameless])).1, ["id-x"]);
        }

        /// The human (2026-10-19): fleet-wide = cohort pods only, with a way to take the others
        /// too. `--include-non-cohort` parses on all four commands (and `test`), and with it the selection
        /// comes back whole and nothing is said; without it, the cohort's and the one line.
        #[test]
        fn include_non_cohort_takes_every_pod_and_says_nothing() {
            let scope = |argv: &[&str]| -> CohortScope {
                use clap::Parser;
                let cli = Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                match cli.cmd {
                    Cmd::Pods(PodCmd::Backup { cohort, .. })
                    | Cmd::Pods(PodCmd::Pull { cohort, .. })
                    | Cmd::Pods(PodCmd::InitBranches { cohort, .. })
                    | Cmd::Pods(PodCmd::SetBranch { cohort, .. })
                    | Cmd::Pods(PodCmd::Test { cohort, .. }) => cohort,
                    _ => panic!("{argv:?}: not a cohort-scoped command"),
                }
            };
            for argv in [&["backup"][..], &["pull"], &["init-branches"], &["set-branch", "main", "--all"], &["test"], &["test", "--deep"]] {
                assert!(!scope(argv).include_non_cohort, "{argv:?}: off by default");
                let with = [argv, &["--include-non-cohort"]].concat();
                assert!(scope(&with).include_non_cohort, "{with:?}");
            }
            let cfg = account_cfg("");
            let every = || Selected::all(account().pods);
            let _ = take_said();
            let sel = cohort_only(&cfg, every(), CohortScope { include_non_cohort: true });
            assert_eq!(names(&sel), names(&every()), "every pod, in order");
            assert!(take_said().is_empty(), "nothing left out, nothing said");
            let sel = cohort_only(&cfg, every(), CohortScope::default());
            assert_eq!(names(&sel), ["devtest-apple", "devtest-bloom", "devtest-extra"]);
            // (In fleet order here: `select` orders a real run's by list slot, as LEFT_OUT has it.)
            let out = ["devtest-zebra", "staff-gpu", "quiet_blue_fox", "devtest2-apple"].map(String::from);
            assert_eq!(take_said(), [non_cohort_line("devtest", &out).unwrap()]);
        }

        /// The health check (spec health-cron) reads only, but runs CUDA, NCCL and a download on
        /// every GPU it reaches, and its daily cron line names no pod: fleet-wide (bare or `--all`)
        /// it reaches this cohort's pods only and says who it left out — never a staff box or
        /// another prefix's pod; the torch check (`pods test`) the same. `--include-non-cohort`
        /// takes every pod; a named staff box is checked, as named.
        #[tokio::test(start_paused = true)]
        async fn pods_test_deep_reaches_only_this_cohorts_pods_unless_told() {
            let cfg = account_cfg("");
            let run = |argv: &'static [&'static str]| {
                let cfg = cfg.clone();
                async move {
                    let fake = Arc::new(FakeRemote::new());
                    for port in 22001..=22007 {
                        fake.script(&host(port), [FakeReply::stdout(DEEP_HEALTHY)]);
                    }
                    let _ = take_said();
                    handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap();
                    let reached: Vec<u16> = (22001..=22007).filter(|port| !fake.calls_to(&host(*port)).is_empty()).collect();
                    (reached, take_said())
                }
            };
            for argv in [&["test", "--deep"][..], &["test", "--deep", "--all"], &["test"]] {
                assert_eq!(run(argv).await, (COHORT_PORTS.to_vec(), vec![LEFT_OUT.to_string()]), "{argv:?}");
            }
            assert_eq!(run(&["test", "--deep", "--include-non-cohort"]).await, ((22001..=22007).collect(), vec![]));
            assert_eq!(run(&["test", "--deep", "@staff-gpu"]).await, (vec![22005], vec![]));
        }

        #[test]
        fn the_left_out_pods_get_one_line() {
            let many: Vec<String> = (1..=8).map(|i| format!("box{i}")).collect();
            let cases: &[(&[String], Option<&str>)] = &[
                (&[], None),
                (
                    &many[..1],
                    Some("⊘ skipped 1 non-cohort pod (not devtest-…, or an @ staff entry): box1 — name one to include it, or pass --include-non-cohort"),
                ),
                (
                    &many[..2],
                    Some("⊘ skipped 2 non-cohort pods (not devtest-…, or an @ staff entry): box1, box2 — name one to include it, or pass --include-non-cohort"),
                ),
                (
                    &many,
                    Some(
                        "⊘ skipped 8 non-cohort pods (not devtest-…, or an @ staff entry): box1, box2, box3, box4, box5, box6 \
                         (+2 more) — name one to include it, or pass --include-non-cohort",
                    ),
                ),
            ];
            for (names, want) in cases {
                assert_eq!(non_cohort_line("devtest", names).as_deref(), *want, "{names:?}");
            }
        }

        #[test]
        fn cohort_pods_on_main_get_one_line_naming_the_fix() {
            let on = |pods: &[(&str, &str)]| pods.iter().map(|(n, b)| (n.to_string(), b.to_string())).collect::<Vec<_>>();
            let cases: &[OnMainCase] = &[
                (&[], true, None),
                // The morning reset: the plain command.
                (
                    &[("devtest-bloom", "main"), ("devtest-apple", "main"), ("devtest-extra", "main")],
                    true,
                    Some("⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`"),
                ),
                // Some of the fleet, or a named run: the pods themselves, sorted.
                (
                    &[("devtest-bloom", "main"), ("devtest-apple", "master")],
                    false,
                    Some("⚠ 2 cohort pods on main or master: not backed up until `arena pods init-branches devtest-apple devtest-bloom`"),
                ),
                (
                    &[("devtest-apple", "HEAD")],
                    false,
                    Some("⚠ 1 cohort pod on a detached HEAD: not backed up until `arena pods init-branches devtest-apple`"),
                ),
            ];
            for (pods, fleet_wide, want) in cases {
                assert_eq!(on_protected_line(&on(pods), *fleet_wide).as_deref(), *want, "{pods:?}");
            }
            // In `pods backup`: a staff box on main (it was named) has its own line and is not
            // counted with the cohort's; each pod is said exactly once.
            let cfg = account_cfg("");
            let naming = Naming::from_config(&cfg);
            let found = on(&[("devtest-apple", "main"), ("devtest-zebra", "main"), ("quiet_blue_fox", "master")]);
            assert_eq!(
                on_protected_lines(&naming, &found, false),
                [
                    "⊘ devtest-zebra (skipped — on protected branch main)",
                    "⊘ quiet_blue_fox (skipped — on protected branch master)",
                    "⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`",
                ]
            );
            assert!(on_protected_lines(&naming, &[], true).is_empty());
        }

        #[test]
        fn set_branch_says_what_it_restored_and_reminds_on_main() {
            let cases: &[(&[&str], &str, &str)] = &[
                (&[], "main", ""),
                (
                    &["Grokking"],
                    "main",
                    " — restored 1 nested repo that main doesn't track (Grokking): on disk and staged; \
                     the first backup on the next day branch commits them",
                ),
                (
                    &["Grokking", "chapter4/exercises/shutdown_avoidance"],
                    "master",
                    " — restored 2 nested repos that master doesn't track (Grokking, chapter4/exercises/shutdown_avoidance): \
                     on disk and staged; the first backup on the next day branch commits them",
                ),
                (
                    &["Grokking"],
                    "feature-x",
                    " — restored 1 nested repo that feature-x doesn't track (Grokking): on disk and staged; the next backup commits them",
                ),
            ];
            for (restored, branch, want) in cases {
                assert_eq!(restored_note(restored, branch), *want, "{restored:?} {branch}");
            }
            let cfg = account_cfg("");
            let naming = Naming::from_config(&cfg);
            let landed = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let reminder = |branch: &str, pods: &[&str], fleet_wide: bool| set_branch_reminder(&naming, branch, &landed(pods), fleet_wide);
            assert_eq!(
                reminder("main", &["devtest-apple", "devtest-bloom"], true).as_deref(),
                Some("⚠ 2 cohort pods on main: not backed up until `arena pods init-branches`")
            );
            // A staff box that landed on main isn't counted (init-branches isn't its fix).
            assert_eq!(
                reminder("master", &["staff-gpu", "devtest-bloom"], false).as_deref(),
                Some("⚠ 1 cohort pod on master: not backed up until `arena pods init-branches devtest-bloom`")
            );
            assert_eq!(reminder("main", &["staff-gpu"], false), None);
            assert_eq!(reminder("main", &[], true), None);
            assert_eq!(reminder("autocommit-devtest-w1d2-apple", &["devtest-apple"], true), None);
        }

        #[tokio::test(start_paused = true)]
        async fn backup_by_default_reaches_only_this_cohorts_pods_and_says_who_it_left_out() {
            let cfg = account_cfg("");
            take_said();
            // No targets, `all`, `--all`: the cohort's three (the off-list extra included) and one
            // line for the rest — not a failure (the run succeeds).
            for argv in [&["backup", "--no-pull"][..], &["backup", "all", "--no-pull"], &["backup", "--all", "--no-pull"]] {
                let fake = Arc::new(FakeRemote::new());
                handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert_eq!(ports(&fake), COHORT_PORTS, "{argv:?}");
                assert_eq!(take_said(), [LEFT_OUT], "{argv:?}");
            }
            // A filter narrows the cohort's pods; it doesn't widen the default.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["backup", "--exclude", "bloom", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), [22001, 22003]);
            assert_eq!(take_said(), [LEFT_OUT]);
            // Named — by name, @name or id, or a range: exactly those, staff boxes included, no line.
            let cases: &[(&[&str], &[u16])] = &[
                (&["backup", "staff-gpu", "@devtest-zebra", "quiet_blue_fox", "--no-pull"], &[22004, 22005, 22006]),
                (&["backup", "id-devtest2-apple", "apple", "--no-pull"], &[22001, 22007]),
                (&["backup", "apple..staff-gpu", "--no-pull"], &[22001, 22002, 22005]),
            ];
            for (argv, want) in cases {
                let fake = Arc::new(FakeRemote::new());
                handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert_eq!(ports(&fake), *want, "{argv:?}");
                assert_eq!(take_said(), Vec::<String>::new(), "{argv:?}");
            }
            // The non-cohort failures that used to fail every run are gone from it: here they
            // would all fail, and the default run still succeeds.
            let fake = Arc::new(FakeRemote::new());
            for port in [22004, 22005, 22006, 22007] {
                fake.script(&host(port), [FakeReply::exit(255, "Permission denied (publickey).")]);
            }
            handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), COHORT_PORTS);
            take_said();
        }

        /// Review (fb2 R8): each of the four commands passes `--include-non-cohort` through. Without
        /// it the ⊘ line is said (the rest left out); with it nothing is left out, so nothing is said —
        /// and `backup` reaches every pod. (`pull` and the switches as dry runs: no rsync, no switch.)
        #[tokio::test(start_paused = true)]
        async fn include_non_cohort_reaches_every_pod_through_each_command() {
            let cfg = account_cfg("");
            take_said();
            let runs: &[&[&str]] = &[
                &["backup", "--no-pull"],
                &["pull", "--label", "w1d2", "--dry-run"],
                &["init-branches", "--week", "1", "--day", "2", "--dry-run"],
                &["set-branch", "main", "--all", "--dry-run"],
            ];
            for argv in runs {
                let fake = Arc::new(FakeRemote::new());
                handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert!(take_said().iter().any(|l| l == LEFT_OUT), "{argv:?}: the default leaves them out");
                let with = [*argv, &["--include-non-cohort"]].concat();
                let fake = Arc::new(FakeRemote::new());
                handle_pods(pods(&with), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{with:?}: {e}"));
                assert!(!take_said().iter().any(|l| l.starts_with("⊘ skipped")), "{with:?}: nothing left out");
                if argv[0] == "backup" {
                    assert_eq!(ports(&fake), [22001, 22002, 22003, 22004, 22005, 22006, 22007], "{with:?}");
                }
            }
        }

        /// Review ai-review claude #5: `--include-non-cohort` takes every pod on the account — fine
        /// for a pull, but `set-branch --all --hard` with it reset the staff boxes and the other
        /// cohort's pods too, with nothing in the way. Now that is refused before a pod is listed or
        /// touched (a dry run too, and with --init-branches), saying why; pods named outright (the
        /// flag then changes nothing), a gentle switch, and `--hard` on this cohort go on.
        #[tokio::test(start_paused = true)]
        async fn hard_with_include_non_cohort_is_refused_unless_the_pods_are_named() {
            use super::super::hard_non_cohort_refusal;
            const REFUSED: &str = "set-branch --hard with --include-non-cohort refused: that takes every pod on the account, so \
                --hard would reset staff boxes and other cohorts' pods too, discarding their uncommitted work — name the pods to \
                reset (`arena pods set-branch <branch> <pod>... --hard`), or leave --include-non-cohort out to reset this cohort's";
            let cfg = account_cfg("");
            take_said();
            for argv in [
                &["set-branch", "main", "--all", "--hard", "--include-non-cohort"][..],
                &["set-branch", "main", "all", "--hard", "--include-non-cohort", "--dry-run"],
                &["set-branch", "main", "--all", "--hard", "--include-non-cohort", "--init-branches"],
                &["set-branch", "main", "--all", "--exclude", "staff-gpu", "--hard", "--include-non-cohort"],
            ] {
                let fake = Arc::new(FakeRemote::new());
                let err = handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_err();
                assert_eq!(err.to_string(), REFUSED, "{argv:?}");
                assert!(fake.calls().is_empty() && take_said().is_empty(), "{argv:?}: nothing touched or said");
            }
            // Named pods (a staff box, another cohort's): exactly those, the flag or not.
            for argv in [
                &["set-branch", "main", "@staff-gpu", "devtest2-apple", "--hard", "--include-non-cohort", "--dry-run"][..],
                // Gentle, fleet-wide: every pod, as before; --hard on this cohort's: as before.
                &["set-branch", "main", "--all", "--include-non-cohort", "--dry-run"],
                &["set-branch", "main", "--all", "--hard", "--dry-run"],
            ] {
                let fake = Arc::new(FakeRemote::new());
                handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            }
            take_said();
            let on = CohortScope { include_non_cohort: true };
            assert_eq!(hard_non_cohort_refusal(true, on, false).as_deref(), Some(REFUSED));
            for (hard, scope, named) in [(true, on, true), (false, on, false), (true, CohortScope::default(), false)] {
                assert_eq!(hard_non_cohort_refusal(hard, scope, named), None, "{hard} {scope:?} {named}");
            }
        }

        /// `pods ask --all` reads this cohort's pods (the others named in one line), every pod with
        /// `--include-non-cohort`; named pods exactly. Dry runs: nothing is sent.
        #[tokio::test(start_paused = true)]
        async fn ask_all_reads_the_cohort_and_include_non_cohort_every_pod() {
            let cfg = account_cfg("");
            take_said();
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["ask", "--all", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
            assert_eq!(take_said(), [LEFT_OUT]);
            assert_eq!(ports(&fake), COHORT_PORTS);
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["ask", "--all", "--include-non-cohort", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
            assert!(take_said().is_empty());
            assert_eq!(ports(&fake), [22001, 22002, 22003, 22004, 22005, 22006, 22007]);
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["ask", "bloom", "@devtest-zebra", "is it busy?", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
            assert_eq!(ports(&fake), [22002, 22004]);
        }

        /// `ASK_AI=off` refuses — whatever `--ai` says — and so does an unscoped ask or two
        /// questions, all before any pod is read.
        #[tokio::test(start_paused = true)]
        async fn ask_refuses_off_unscoped_or_two_questions_before_reading_a_pod() {
            let fake = Arc::new(FakeRemote::new());
            let cases: [(&str, &[&str], &str); 5] = [
                ("ASK_AI=off\n", &["ask", "apple"], "ASK_AI=off"),
                ("ASK_AI=off\n", &["ask", "apple", "--ai", "claude"], "ASK_AI=off"),
                ("ASK_AI=gpt\n", &["ask", "apple"], "ASK_AI must be claude, codex or off"),
                ("", &["ask"], "name the pods to ask about"),
                ("", &["ask", "apple", "is it up?", "and busy?"], "one question at a time"),
            ];
            for (extra, argv, want) in cases {
                let e = handle_pods(pods(argv), &account(), fake.clone(), &account_cfg(extra), false).await.unwrap_err();
                assert!(e.to_string().contains(want), "{argv:?}: {e}");
            }
            assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        }

        #[tokio::test(start_paused = true)]
        async fn backup_says_once_which_cohort_pods_sit_on_main_and_still_exits_zero() {
            let cfg = account_cfg("");
            take_said();
            // The morning reset: every cohort pod on main — one line, the plain fix, exit 0.
            let fake = Arc::new(FakeRemote::new());
            for port in COHORT_PORTS {
                fake.script(&host(port), [FakeReply::stdout("SKIP main\n")]);
            }
            handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(take_said(), [LEFT_OUT, "⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`"]);

            // Some on main: the line names them.
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::stdout("PUSHED autocommit-devtest-w1d2-apple\n")]);
            fake.script(&host(22002), [FakeReply::stdout("SKIP main\n")]);
            fake.script(&host(22003), [FakeReply::stdout("SKIP master\n")]);
            handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(
                take_said(),
                [
                    LEFT_OUT,
                    "⚠ 2 cohort pods on main or master: not backed up until `arena pods init-branches devtest-bloom devtest-extra`"
                ]
            );

            // A staff box named next to a cohort pod, both on main: each said once, in its own line.
            let fake = Arc::new(FakeRemote::new());
            for port in [22001, 22004] {
                fake.script(&host(port), [FakeReply::stdout("SKIP main\n")]);
            }
            handle_pods(pods(&["backup", "apple", "@devtest-zebra", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(
                take_said(),
                [
                    "⊘ devtest-zebra (skipped — on protected branch main)",
                    "⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`"
                ]
            );

            // A real failure beside them still fails the run — for that pod only.
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::stdout("SKIP main\n")]);
            fake.script(&host(22002), [FakeReply::exit(1, "error: failed to push some refs")]);
            fake.script(&host(22003), [FakeReply::stdout("SKIP main\n")]);
            let err = handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) failed to back up");
            assert_eq!(take_said(), [LEFT_OUT, "⚠ 2 cohort pods on main: not backed up until `arena pods init-branches`"]);
        }

        #[tokio::test(start_paused = true)]
        async fn set_branch_to_main_says_what_it_restored_and_reminds_until_init_branches() {
            let cfg = account_cfg("");
            take_said();
            // `--all --hard`: the cohort's pods only; the pod with adopted clones says what was put
            // back; the run ends with the reminder. (Each pod's branch is backed up before its
            // switch: the first reply is its backup's.)
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [FakeReply::ok(), FakeReply::stdout("RESTORED Grokking\nRESTORED chapter4/x\n")]);
            handle_pods(pods(&["set-branch", "main", "--all", "--hard"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), COHORT_PORTS);
            let said = take_said();
            assert_eq!((said.len(), said[0].as_str(), said[1].as_str()), (6, LEFT_OUT, NO_LOCKS), "{said:?}");
            let mut per_pod = unnumbered(&said[2..5]);
            per_pod.sort();
            assert_eq!(
                per_pod,
                [
                    "✓ devtest-apple (on main) — restored 2 nested repos that main doesn't track (Grokking, chapter4/x): \
                     on disk and staged; the first backup on the next day branch commits them",
                    "✓ devtest-bloom (on main)",
                    "✓ devtest-extra (on main)",
                ]
            );
            assert_eq!(said[5], "⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`");

            // Named, a staff box among them: the reminder names the cohort pod only.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["set-branch", "main", "bloom", "staff-gpu"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), [22002, 22005]);
            let said = take_said();
            assert_eq!(said.last().map(String::as_str), Some("⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-bloom`"));
            assert_eq!(said.len(), 4, "{said:?}");

            // A failed switch: the reminder still names the pods that did land on main.
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22002), [FakeReply::ok(), FakeReply::exit(1, "error: Your local changes would be overwritten")]);
            let err = handle_pods(pods(&["set-branch", "main", "apple", "bloom"]), &account(), fake.clone(), &cfg, true).await.unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
            assert_eq!(take_said().last().map(String::as_str), Some("⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`"));

            // Any other branch: no reminder.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["set-branch", "feature-x", "apple"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(take_said(), [NO_LOCKS, "[1/1] ✓ devtest-apple (on feature-x)"]);
        }

        #[tokio::test(start_paused = true)]
        async fn set_branch_init_branches_inits_the_pods_that_switched() {
            let cfg = account_cfg("ARENA_START_DATE=2026-10-05\n");
            let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
            let bcfg = BackupConfig::from_config(&cfg, week, day);
            let hard = checkout_command(REPO, "main", Some(KEY), true);
            take_said();
            // Each cohort pod: the hard reset, then its day branch — each after its branch is backed
            // up — nothing on the staff boxes.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), COHORT_PORTS);
            for (name, port) in [("devtest-apple", 22001), ("devtest-bloom", 22002), ("devtest-extra", 22003)] {
                let init = init_branch_command(&bcfg, name);
                let backup = (backup_command(REPO, Some(KEY), &format!("arena backup {name}"), None), Some(BACKUP_TIMEOUT));
                assert_eq!(
                    execs(&fake, port),
                    [backup.clone(), (hard.clone(), Some(BRANCH_TIMEOUT)), backup, (init, Some(BRANCH_TIMEOUT))],
                    "{name}"
                );
            }
            take_said();
            // A failed switch: that pod gets no day branch — it isn't on main — the others do, and the
            // run fails naming it (review pr4: one failed pod used to keep every pod on main).
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22002), [FakeReply::ok(), FakeReply::exit(1, "fatal: unable to access")]);
            let err = handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &cfg, true)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
            assert_eq!(execs(&fake, 22002).len(), 2, "bloom: its backup and the switch, no init: {:?}", fake.calls());
            for (name, port) in [("devtest-apple", 22001), ("devtest-extra", 22003)] {
                let init = (init_branch_command(&bcfg, name), Some(BRANCH_TIMEOUT));
                assert_eq!(execs(&fake, port).last(), Some(&init), "{name}: init'd");
            }
            // No start date to name the day: refused before any pod is switched.
            let fake = Arc::new(FakeRemote::new());
            let err = handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &account_cfg(""), true)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("ARENA_START_DATE"), "{err}");
            assert!(fake.calls().is_empty(), "{:?}", fake.calls());
            // A dry run previews both and reaches nobody.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["set-branch", "main", "--all", "--init-branches", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert!(fake.calls().is_empty());
            take_said();
        }

        #[tokio::test(start_paused = true)]
        async fn init_branches_by_default_leaves_out_non_cohort_pods_too() {
            let cfg = account_cfg("");
            take_said();
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["init-branches", "--week", "1", "--day", "2"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), COHORT_PORTS);
            // The one line, then each cohort pod's own.
            let said = take_said();
            assert_eq!((said.len(), said[0].as_str(), said[1].as_str()), (5, LEFT_OUT, NO_LOCKS), "{said:?}");
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["init-branches", "staff-gpu", "--week", "1", "--day", "2"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(ports(&fake), [22005]);
            assert_eq!(take_said(), [NO_LOCKS, "[1/1] ✓ staff-gpu -> autocommit-devtest-w1d2-staff-gpu"], "named: no ⊘ line");
        }

        #[tokio::test(start_paused = true)]
        async fn pull_by_default_leaves_out_non_cohort_pods_too() {
            let cfg = account_cfg("");
            take_said();
            // A dry run (it reaches no pod either way) says the same one line; named, none.
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&["pull", "--label", "w1d2", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert_eq!(take_said(), [LEFT_OUT]);
            handle_pods(pods(&["pull", "staff-gpu", "--label", "w1d2", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
            assert!(take_said().is_empty());
            assert!(fake.calls().is_empty());
        }
    }
}

/// PLAN 1.D + restart safety, end to end against fakes: `rename` brings `~/.name`, the
/// OpenRouter key mapping and the proxy along; `terminate --revoke-key` revokes keys and
/// drops CSV rows without ever blocking a terminate; `restart`/`stop` say what a container
/// reset destroys, refuse it without `--wipe-ok` when nothing would survive, and a restart
/// comes back set up and proxied. No network: `FakeRemote` for SSH, a fake provider and key
/// API, temp dirs for the keys CSV and the (write-only) proxy file.
#[cfg(test)]
mod lifecycle_tests {
    use super::{
        disk_fate, handle_pods, handle_rename, handle_restart, handle_terminate, migrate_cutover_on, migrate_finish_on, FinishOpts,
        machine_name_note, plan_csv_move, rename_followup_steps, rename_report, NameMissed, replace_on, rewrite_name_files, stop_lines,
        take_would_refuse, wipe_refusal, Cli, Cmd, CsvMove, CutoverOpts, DiskFate, DiskOp, Interrupt, KeyBook, PlaceFlags,
        PlannedRename, PodCmd, RenameRequest, ReplaceJob, ReplaceOpts, RestartOpts, Selected, SettleWait, SpecOverrides,
        TerminateOpts, LOCKED_REPLACEMENT_NOTE,
    };
    use super::{cutover_revert, handle_reimage, handle_start, migrate_copy_on, revert_followup, stopped_old_refusal, MigrateCmd, MigrateCopyJob};
    use super::{afterwards_line, handle_copy_keys, no_setup_line, reprovision_line, restart_fate, staging_dir, stopped_restart_refusal};
    use arena_core::provider::ContainerReset;
    use arena_core::volume::{probe_command, RepoSite};
    use arena_core::openrouter::{CreatedKey, KeyApi, KeyInfo};
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
    use arena_core::setup::name_file_command;
    use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// `pods terminate`'s switches without `--unlock` (most tests), positionally as before.
    fn topts(revoke_key: bool, skip_proxy: bool, dry_run: bool, yes: bool) -> TerminateOpts {
        TerminateOpts { unlock: false, revoke_key, skip_proxy, dry_run, yes }
    }

    /// A fleet that records every mutation and applies renames/terminates to its listing.
    struct Fleet {
        pods: Mutex<Vec<Pod>>,
        calls: Mutex<Vec<String>>,
        /// Persistent volume (GB) per pod id, as `pod_spec` reports it (absent: none).
        volumes: Vec<(&'static str, u32)>,
        /// Pod ids whose spec can't be read (an API error): volume unknown.
        no_spec: Vec<&'static str>,
        /// Pod ids whose provider rename fails (stopping a batch part-way).
        fail_rename: Vec<&'static str>,
        /// Pod ids the API holds locked — what it refuses stop/restart/terminate on with the
        /// recorded 400. Starts as the listing's locked pods; a test adds one the listing
        /// doesn't show (locked since). `set_locked` keeps both in step.
        server_locked: Mutex<Vec<String>>,
        /// Pod ids whose rename the API refuses as locked (unverified live — see `lock`).
        rename_locked: Vec<&'static str>,
        /// Pod ids whose unlock fails.
        fail_unlock: Vec<&'static str>,
        /// What `create_pod` hands out, in order (and adds to the listing); none left = the
        /// test didn't expect a create.
        to_create: Mutex<Vec<Pod>>,
        /// `list_pods` calls so far, and the calls (1-based) that list every pod WITHOUT its
        /// endpoint — RunPod's passing "running, no endpoint" glitch.
        lists: Mutex<usize>,
        blank_lists: Vec<usize>,
        /// Host maintenance windows `enrich` reports, by pod id.
        maintenance: Vec<(&'static str, arena_core::pod::Maintenance)>,
        /// What a GPU pod's restart is known to do: `Yes` as RunPod v2 (the default),
        /// `Unknown` as v1 — whose restart was seen keeping the disk (live 2026-10-09).
        restart_resets: ContainerReset,
    }

    impl Fleet {
        fn new(pods: Vec<Pod>) -> Self {
            let server_locked = Mutex::new(pods.iter().filter(|p| p.locked == Some(true)).map(|p| p.id.clone()).collect());
            Self {
                pods: Mutex::new(pods),
                calls: Mutex::default(),
                volumes: vec![],
                no_spec: vec![],
                fail_rename: vec![],
                server_locked,
                rename_locked: vec![],
                fail_unlock: vec![],
                to_create: Mutex::default(),
                lists: Mutex::default(),
                blank_lists: vec![],
                maintenance: vec![],
                restart_resets: ContainerReset::Yes,
            }
        }
        /// A container reset as the providers do it: the pod comes back on a NEW endpoint
        /// (port +1000), so a setup that reaches the new one ran after the reset.
        fn reset(&self, id: &str) {
            for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.ssh_port = p.ssh_port.map(|port| port + 1000);
            }
        }
        /// Refuse `ctx` on `id` as the API does a locked pod's.
        fn refuse_locked(&self, id: &str, ctx: &str) -> Result<()> {
            if self.server_locked.lock().unwrap().iter().any(|l| l == id) {
                return Err(recorded_locked(ctx));
            }
            Ok(())
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            let n = {
                let mut lists = self.lists.lock().unwrap();
                *lists += 1;
                *lists
            };
            let mut pods = self.pods.lock().unwrap().clone();
            if self.blank_lists.contains(&n) {
                for p in &mut pods {
                    (p.ssh_ip, p.ssh_port) = (None, None);
                }
            }
            Ok(pods)
        }
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            for p in pods.iter_mut() {
                if let Some((_, m)) = self.maintenance.iter().find(|(id, _)| *id == p.id) {
                    p.maintenance = Some(m.clone());
                }
            }
            Ok(())
        }
        async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
            let mut queue = self.to_create.lock().unwrap();
            assert!(!queue.is_empty(), "not exercised by this test: create {}", spec.name);
            let mut pod = queue.remove(0);
            pod.name = spec.name.clone();
            self.record(format!("create {} {}", spec.name, spec.gpu_type));
            self.pods.lock().unwrap().push(pod.clone());
            Ok(pod)
        }
        async fn stop_pod(&self, id: &str) -> Result<()> {
            self.refuse_locked(id, "stop pod")?;
            self.record(format!("stop {id}"));
            Ok(())
        }
        /// Like a real container reset, the restarted pod comes back on a NEW endpoint (port
        /// +1000): setup reaching the new one proves it ran after the restart, not before.
        async fn restart_pod(&self, id: &str) -> Result<()> {
            self.refuse_locked(id, "restart pod")?;
            self.record(format!("restart {id}"));
            self.reset(id);
            Ok(())
        }
        /// A stopped pod comes back running, as a fresh container on a new endpoint.
        async fn start_pod(&self, id: &str) -> Result<()> {
            self.record(format!("start {id}"));
            for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.status = "RUNNING".into();
            }
            self.reset(id);
            Ok(())
        }
        async fn reimage_pod(&self, id: &str, image: &str, _env: &[(String, String)]) -> Result<()> {
            self.record(format!("reimage {id} {image}"));
            self.reset(id);
            Ok(())
        }
        async fn terminate_pod(&self, id: &str) -> Result<()> {
            self.refuse_locked(id, "terminate pod")?;
            self.record(format!("terminate {id}"));
            self.pods.lock().unwrap().retain(|p| p.id != id);
            Ok(())
        }
        fn lock_support(&self, pod: &Pod) -> Result<()> {
            match pod.provider.as_str() {
                "runpod" => Ok(()),
                other => Err(Error::NotImplemented(format!("not supported on {other} — only RunPod pods can be locked (on RunPod REST v2, the default)"))),
            }
        }
        async fn set_locked(&self, id: &str, locked: bool) -> Result<()> {
            if !locked && self.fail_unlock.contains(&id) {
                return Err(Error::provider("unlock pod HTTP 500 Internal Server Error: boom"));
            }
            self.record(format!("lock {id} {locked}"));
            let mut server = self.server_locked.lock().unwrap();
            server.retain(|l| l != id);
            if locked {
                server.push(id.to_string());
            }
            for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.locked = Some(locked);
            }
            Ok(())
        }
        async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
            if self.fail_rename.contains(&id) {
                return Err(Error::provider("rename refused"));
            }
            if self.rename_locked.contains(&id) {
                return Err(recorded_locked("rename pod"));
            }
            self.record(format!("rename {id} {new_name}"));
            for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.name = new_name.to_string();
            }
            Ok(())
        }
        /// Every backend here renames; a `lambda` pod stands for one that can't.
        fn rename_support(&self, pod: &Pod) -> Result<()> {
            match pod.provider.as_str() {
                "lambda" => Err(Error::NotImplemented("rename not supported on provider `lambda`".into())),
                _ => Ok(()),
            }
        }
        async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
            if self.no_spec.contains(&id) {
                return Err(Error::provider("get pod: HTTP 500"));
            }
            let mut spec = PodSpec::from_config(&Config::parse(""));
            spec.volume_gb = self.volumes.iter().find(|(i, _)| *i == id).map_or(0, |(_, v)| *v);
            Ok(spec)
        }
        fn restart_wipes_container_disk(&self, pod: &Pod) -> bool {
            pod.provider != "hetzner" // as the fleet provider routes it
        }
        fn restart_resets_container(&self, pod: &Pod) -> ContainerReset {
            if pod.provider == "hetzner" {
                ContainerReset::No
            } else {
                self.restart_resets
            }
        }
    }

    fn pod(id: &str, name: &str, provider: &str, endpoint: Option<(&str, u16)>) -> Pod {
        Pod {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            ssh_ip: endpoint.map(|(ip, _)| ip.to_string()),
            ssh_port: endpoint.map(|(_, port)| port),
            ..Default::default()
        }
    }

    /// alpha + bravo (GPU pods, reachable), charlie (no endpoint yet), delta (a Hetzner VM).
    fn fleet() -> Fleet {
        Fleet::new(vec![
            pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("r-bravo", "devtest-bravo", "runpod", Some(("10.0.0.1", 22002))),
            pod("r-charlie", "devtest-charlie", "runpod", None),
            pod("h-delta", "devtest-delta", "hetzner", Some(("10.0.0.9", 22))),
        ])
    }

    /// A temp dir (per process + test), removed when dropped.
    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmpdir(tag: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!("arena-lifecycle-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    /// Setup + a write-only proxy in `dir` (never reloads nginx, never SSHes anywhere). The
    /// repo is in its default place, /root/r — on the container disk.
    fn cfg(dir: &Path) -> Config {
        Config::parse(&cfg_text(dir))
    }

    fn cfg_text(dir: &Path) -> String {
        format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo charlie delta echo @solo-gpu)\n\
             ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
             SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\nVSCODE_PREINSTALL=0\nREPO_ON_VOLUME=0\n",
            dir.join("proxy.conf").display()
        )
    }

    fn proxy_file(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("proxy.conf")).unwrap_or_default()
    }

    /// The cohort keys file plus the `openrouter_api_keys.csv` symlink to it, as `keys gen`
    /// leaves them. Returns the symlink (what the commands are pointed at).
    fn keys_csv(dir: &Path, rows: &str) -> PathBuf {
        std::fs::write(dir.join("devtest_openrouter_keys.csv"), format!("# OpenRouter API keys — arena iteration: devtest\n{rows}"))
            .unwrap();
        let link = dir.join("openrouter_api_keys.csv");
        std::os::unix::fs::symlink("devtest_openrouter_keys.csv", &link).unwrap();
        link
    }

    fn cohort_csv(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("devtest_openrouter_keys.csv")).unwrap()
    }

    /// The provisioning API: keys by name, recording creates/deletes/renames; deleting a
    /// key whose name is in `broken` fails. Shared with `selection_tests` (`keys` commands).
    #[derive(Default)]
    pub(crate) struct FakeKeys {
        pub(crate) keys: Mutex<Vec<KeyInfo>>,
        pub(crate) calls: Mutex<Vec<String>>,
        pub(crate) broken: Vec<&'static str>,
    }

    impl FakeKeys {
        pub(crate) fn with(names: &[(&str, &str)]) -> Self {
            let keys = names
                .iter()
                .map(|(hash, name)| KeyInfo {
                    hash: hash.to_string(),
                    name: Some(name.to_string()),
                    limit: Some(5.0),
                    ..Default::default()
                })
                .collect();
            Self { keys: Mutex::new(keys), ..Default::default() }
        }
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        pub(crate) fn names(&self) -> Vec<String> {
            let mut n: Vec<String> = self.keys.lock().unwrap().iter().filter_map(|k| k.name.clone()).collect();
            n.sort();
            n
        }
    }

    #[async_trait]
    impl KeyApi for FakeKeys {
        async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
            Ok(self.keys.lock().unwrap().clone())
        }
        async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey> {
            self.calls.lock().unwrap().push(format!("create {name}"));
            let hash = format!("h-new-{name}");
            let info = KeyInfo { hash: hash.clone(), name: Some(name.into()), limit, ..Default::default() };
            self.keys.lock().unwrap().push(info);
            Ok(CreatedKey { secret: format!("sk-new-{name}"), hash, name: name.into() })
        }
        async fn delete_key(&self, hash: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("delete {hash}"));
            let mut keys = self.keys.lock().unwrap();
            if keys.iter().any(|k| k.hash == hash && self.broken.contains(&k.name.as_deref().unwrap_or(""))) {
                return Err(Error::provider("openrouter delete key: HTTP 500"));
            }
            keys.retain(|k| k.hash != hash);
            Ok(())
        }
        async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("rename {hash} {new_name}"));
            for k in self.keys.lock().unwrap().iter_mut().filter(|k| k.hash == hash) {
                k.name = Some(new_name.to_string());
            }
            Ok(())
        }
    }

    fn no_api(csv: &Path) -> KeyBook<'static> {
        KeyBook { csv: csv.to_path_buf(), api: None }
    }

    fn one(old: &str, new: &str) -> RenameRequest {
        RenameRequest::One { old: old.into(), new: new.into() }
    }

    fn name_write(host: &str, short: &str) -> RemoteCall {
        RemoteCall::Exec { host: host.into(), cmd: name_file_command(short), timeout: Some(PROBE_TIMEOUT) }
    }

    #[tokio::test]
    async fn rename_rewrites_name_file_moves_the_key_and_syncs_the_proxy() {
        let dir = tmpdir("rename-one");
        let (cfg, f, remote) = (cfg(&dir.0), fleet(), Arc::new(FakeRemote::new()));
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-or-a\ndevtest-bravo,sk-or-b\n");
        let keys = FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-b", "devtest-bravo"), ("h-x", "other-cohort-x")]);
        let book = KeyBook::new(&csv, Some(&keys));
        handle_rename(&f, remote.clone(), &cfg, &book, one("alpha", "echo"), false, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
        // ~/.name: rewritten on alpha's host only, with setup's exact line and the SHORT name.
        assert_eq!(remote.calls(), [name_write("10.0.0.1:22001", "echo")]);
        // The CSV row moved in the cohort file the symlink points at (the link kept); the
        // key itself and the other rows are untouched.
        let text = cohort_csv(&dir.0);
        assert!(text.contains("devtest-echo,sk-or-a") && text.contains("devtest-bravo,sk-or-b"), "{text}");
        assert!(!text.contains("devtest-alpha") && text.starts_with("# OpenRouter API keys"), "{text}");
        assert!(std::fs::symlink_metadata(&csv).unwrap().file_type().is_symlink());
        // OpenRouter: the same key, renamed in place (no delete, no re-mint).
        assert_eq!(keys.calls(), ["rename h-a devtest-echo"]);
        assert_eq!(keys.names(), ["devtest-bravo", "devtest-echo", "other-cohort-x"]);
        // The proxy followed: the new name has a forward, the old one is gone.
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-echo") && !px.contains("name=devtest-alpha"), "{px}");
    }

    #[tokio::test]
    async fn an_unreachable_pod_is_a_warning_not_a_failure_and_absolute_names_work() {
        let dir = tmpdir("rename-unreachable");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-charlie,sk-or-c\n");
        // charlie has no endpoint: renamed anyway (to an absolute `@solo-gpu` entry), no SSH
        // attempted; no provisioning key → OpenRouter skipped quietly, the CSV still moves.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("charlie", "solo-gpu"), true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename r-charlie solo-gpu"]);
        assert!(remote.calls().is_empty());
        assert!(cohort_csv(&dir.0).contains("solo-gpu,sk-or-c"));
        // A reachable pod whose ssh fails: still renamed, still Ok — reported for a re-setup.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        remote.script("10.0.0.1:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
        handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "@solo-gpu"), true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename r-bravo solo-gpu"]);
        // An absolute name is its own short name.
        assert_eq!(remote.calls(), [name_write("10.0.0.1:22002", "solo-gpu")]);
        // What rename hands its summary for both: exactly these pods, with why.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        remote.script("10.0.0.1:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
        let pods = f.pods.lock().unwrap().clone();
        let plan = [
            PlannedRename { id: "r-charlie".into(), old: "devtest-charlie".into(), new: "solo-gpu".into() },
            PlannedRename { id: "r-bravo".into(), old: "devtest-bravo".into(), new: "devtest-echo".into() },
            PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-delta".into() },
        ];
        let done: Vec<&PlannedRename> = plan.iter().collect();
        let mut missed = rewrite_name_files(remote.clone(), &cfg, "devtest", &done, &pods).await;
        missed.name_file.sort();
        assert_eq!(
            missed.name_file,
            [
                ("devtest-echo".to_string(), "exit 255: ssh: connect to host 10.0.0.1 port 22002: Connection refused".to_string()),
                ("solo-gpu".to_string(), "no SSH endpoint".to_string()),
            ]
        );
        assert!(missed.hostname.is_empty(), "RunPod pods aren't named by their hostname: {missed:?}");
        // What the operator is told about them.
        let note = machine_name_note(&done, &pods);
        let report = rename_report(note.as_deref(), &missed, &["devtest-echo already has its own OpenRouter key".into()]);
        assert!(report[0].starts_with("note: each renamed pod's MACHINE_NAME env var keeps the old name") && report[0].contains("reimage"), "{report:?}");
        assert!(report[1].contains("re-run `arena pods setup devtest-echo solo-gpu`") && report[1].contains("no SSH endpoint"), "{report:?}");
        assert_eq!(report[2], "⚠ keys: devtest-echo already has its own OpenRouter key");
        assert!(rename_report(None, &NameMissed::default(), &[]).is_empty(), "nothing to say");
    }

    /// Prod 2026-10-30, `pods rename arena9-rainbow keepsake` on a Hetzner VM: its prompt, login
    /// banner and `~/.name` fallback come from the hostname, so the follow-up sets that too —
    /// after `~/.name`, on the VM alone, with setup's own command — and a miss is reported like
    /// `~/.name`'s, with the command that fixes it; never fatal. A RunPod pod's hostname is its
    /// container id: left alone.
    #[tokio::test]
    async fn a_hetzner_rename_sets_the_hostname_too() {
        use arena_core::setup::hostname_command;
        let dir = tmpdir("rename-hostname");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "");
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("delta", "echo"), true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename h-delta devtest-echo"]);
        let hostname = RemoteCall::Exec {
            host: "10.0.0.9:22".into(),
            cmd: hostname_command("devtest-echo").unwrap(),
            timeout: Some(PROBE_TIMEOUT),
        };
        assert_eq!(remote.calls(), [name_write("10.0.0.9:22", "echo"), hostname]);
        // The hostname can't be set: renamed anyway, reported with what to run.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        remote.script("10.0.0.9:22", [FakeReply::ok(), FakeReply::exit(1, "Could not set static hostname: Access denied")]);
        let pods = f.pods.lock().unwrap().clone();
        let plan = [
            PlannedRename { id: "h-delta".into(), old: "devtest-delta".into(), new: "devtest-echo".into() },
            PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-bravo".into() },
        ];
        let done: Vec<&PlannedRename> = plan.iter().collect();
        let missed = rewrite_name_files(remote.clone(), &cfg, "devtest", &done, &pods).await;
        assert!(missed.name_file.is_empty(), "{missed:?}");
        assert_eq!(missed.hostname, [("devtest-echo".to_string(), "exit 1: Could not set static hostname: Access denied".to_string())]);
        let report = rename_report(None, &missed, &[]);
        assert_eq!(
            report,
            ["⚠ the hostname is still the old name on devtest-echo (devtest-echo: exit 1: Could not set static hostname: Access \
              denied) — re-run `arena pods setup devtest-echo` once it answers"]
        );
        // No endpoint at all: both are said.
        let mut gone = pods.clone();
        gone.iter_mut().filter(|p| p.id == "h-delta").for_each(|p| p.ssh_ip = None);
        let missed = rewrite_name_files(Arc::new(FakeRemote::new()), &cfg, "devtest", &done[..1], &gone).await;
        let why = ("devtest-echo".to_string(), "no SSH endpoint".to_string());
        assert_eq!((missed.name_file, missed.hostname), (vec![why.clone()], vec![why]));
        // The dry run and prompt say it, for the Hetzner VM only.
        let steps = rename_followup_steps("devtest", &plan, &pods, &no_api(&csv), true).join("\n");
        assert!(steps.contains("and set the hostname (a Hetzner VM is named by it; kept across a reboot): devtest-echo\n"), "{steps}");
    }

    /// The MACHINE_NAME note is about a RunPod or Vast container's env var: said for those, and
    /// only those, by name in a mixed batch — never for a Hetzner (or Shadeform) VM, which has
    /// none (prod 2026-10-30 printed it for a Hetzner rename). A pod not listed gets it: extra
    /// only.
    #[test]
    fn the_machine_name_note_is_for_pods_with_that_env_var() {
        let pods = [
            pod("r1", "devtest-alpha", "runpod", None),
            pod("v1", "devtest-bravo", "vast", None),
            pod("h1", "devtest-charlie", "hetzner", None),
            pod("s1", "devtest-delta", "shadeform", None),
        ];
        let plan: Vec<PlannedRename> =
            [("r1", "devtest-echo"), ("v1", "devtest-foxtrot"), ("h1", "devtest-golf"), ("s1", "devtest-hotel"), ("x1", "devtest-india")]
                .iter()
                .map(|(id, new)| PlannedRename { id: id.to_string(), old: String::new(), new: new.to_string() })
                .collect();
        let note = |ids: &[&str]| {
            let renamed: Vec<&PlannedRename> = plan.iter().filter(|r| ids.contains(&r.id.as_str())).collect();
            machine_name_note(&renamed, &pods)
        };
        assert_eq!(note(&["h1"]), None);
        assert_eq!(note(&["h1", "s1"]), None);
        let all = note(&["r1", "v1"]).unwrap();
        assert!(all.starts_with("note: each renamed pod's MACHINE_NAME env var keeps the old name — env only changes with a reimage"), "{all}");
        let mixed = note(&["r1", "h1", "x1"]).unwrap();
        assert!(mixed.starts_with("note: the MACHINE_NAME env var of devtest-echo, devtest-india keeps the old name"), "{mixed}");
    }

    /// A pod whose backend can't rename ([`Provider::rename_support`]) is refused before
    /// anything is asked, renamed or reached — the whole batch, as `plan_renames`' problems are.
    #[tokio::test]
    async fn a_pod_its_backend_cannot_rename_is_refused_up_front() {
        let dir = tmpdir("rename-unsupported");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "");
        let f = Fleet::new(vec![
            pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("l-bravo", "devtest-bravo", "lambda", Some(("10.0.0.2", 22))),
        ]);
        let remote = Arc::new(FakeRemote::new());
        let e = handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "echo"), true, false, true).await.unwrap_err();
        assert_eq!(e.to_string(), "refusing to rename anything:\n  devtest-bravo: rename not supported on provider `lambda`");
        // The dry run says so too.
        let e = handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "echo"), true, true, true).await.unwrap_err();
        assert!(e.to_string().contains("rename not supported on provider `lambda`"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty());
        // Another pod in the same fleet is renamed as ever.
        handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("alpha", "echo"), true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
    }

    #[tokio::test]
    async fn from_prefix_moves_every_pod_its_name_file_and_its_key() {
        let dir = tmpdir("rename-prefix");
        let cfg = cfg(&dir.0);
        let f = Fleet::new(vec![
            pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
            pod("c", "arena8-charlie", "runpod", Some(("10.0.0.1", 22003))),
        ]);
        let remote = Arc::new(FakeRemote::new());
        let csv = keys_csv(&dir.0, "arena8-alpha,sk-a\narena8-bravo,sk-b\narena8-charlie,sk-c\nsolo-gpu,sk-s\n");
        // charlie's new name already has a (stale) key on OpenRouter: its mapping is left
        // alone on BOTH sides, so the CSV and OpenRouter never disagree.
        let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo"), ("h-c", "arena8-charlie"), ("h-old", "devtest-charlie")]);
        let book = KeyBook::new(&csv, Some(&keys));
        handle_rename(&f, remote.clone(), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), false, false, true)
            .await
            .unwrap();
        assert_eq!(f.calls(), ["rename a devtest-alpha", "rename b devtest-bravo", "rename c devtest-charlie"]);
        let mut writes = remote.calls();
        writes.sort_by(|x, y| x.host().cmp(y.host()));
        assert_eq!(
            writes,
            [name_write("10.0.0.1:22001", "alpha"), name_write("10.0.0.1:22002", "bravo"), name_write("10.0.0.1:22003", "charlie")]
        );
        let rows = arena_core::apikeys::parse_csv(&cohort_csv(&dir.0));
        let hosts: Vec<&str> = rows.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hosts, ["devtest-alpha", "devtest-bravo", "arena8-charlie", "solo-gpu"]);
        assert_eq!(keys.calls(), ["rename h-a devtest-alpha", "rename h-b devtest-bravo"]);
        let px = proxy_file(&dir.0);
        assert!(["devtest-alpha", "devtest-bravo", "devtest-charlie"].iter().all(|n| px.contains(&format!("name={n}"))), "{px}");
    }

    #[tokio::test]
    async fn after_a_prefix_change_the_rows_move_into_the_current_cohorts_keys_file() {
        // The link still points at the OLD cohort's file (keys were minted as arena8). The
        // renamed rows must land in devtest's file, with the link pointing at it — else the
        // next `keys gen` (which repoints the link and skips keys that exist by name) would
        // leave these machines with no deliverable key.
        let dir = tmpdir("rename-cross");
        let cfg = cfg(&dir.0);
        let old_file = dir.0.join("arena8_openrouter_keys.csv");
        std::fs::write(&old_file, "# OpenRouter API keys — arena iteration: arena8\narena8-alpha,sk-a\narena8-bravo,sk-b\nsolo-gpu,sk-s\narena8-gone,sk-g\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let link = dir.0.join("openrouter_api_keys.csv");
        std::os::unix::fs::symlink("arena8_openrouter_keys.csv", &link).unwrap();
        let f = Fleet::new(vec![
            pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
            pod("s", "solo-gpu", "vast", None),
        ]);
        let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo")]);
        let book = KeyBook::new(&link, Some(&keys));
        // The dry run says where the rows go.
        let pods = f.pods.lock().unwrap().clone();
        let plan = vec![
            PlannedRename { id: "a".into(), old: "arena8-alpha".into(), new: "devtest-alpha".into() },
            PlannedRename { id: "b".into(), old: "arena8-bravo".into(), new: "devtest-bravo".into() },
        ];
        let steps = rename_followup_steps("devtest", &plan, &pods, &book, true).join("\n");
        assert!(steps.contains("move rows out of the previous cohort's file into") && steps.contains("devtest_openrouter_keys.csv"), "{steps}");
        assert!(steps.contains("arena8-alpha → devtest-alpha, arena8-bravo → devtest-bravo (carrying along: solo-gpu)"), "{steps}");
        handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), true, false, true)
            .await
            .unwrap();
        // The current cohort's file: the moved rows + the other live pod's row, seeded header.
        let new_text = std::fs::read_to_string(dir.0.join("devtest_openrouter_keys.csv")).unwrap();
        assert!(new_text.starts_with("# OpenRouter API keys — arena iteration: devtest\n"), "{new_text}");
        let rows = arena_core::apikeys::parse_csv(&new_text);
        assert_eq!(rows, [("devtest-alpha".into(), "sk-a".into()), ("devtest-bravo".into(), "sk-b".into()), ("solo-gpu".to_string(), "sk-s".to_string())]);
        // ...which copy-keys now reads through the link; a new file keeps the old one's mode.
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("devtest_openrouter_keys.csv"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.0.join("devtest_openrouter_keys.csv")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The old file loses only the moved rows (the gone pod's record stays with its cohort).
        let old_text = std::fs::read_to_string(&old_file).unwrap();
        let old_rows: Vec<String> = arena_core::apikeys::parse_csv(&old_text).into_iter().map(|(h, _)| h).collect();
        assert_eq!(old_rows, ["solo-gpu", "arena8-gone"]);
        assert_eq!(keys.calls(), ["rename h-a devtest-alpha", "rename h-b devtest-bravo"]);
    }

    #[tokio::test]
    async fn rename_moves_a_key_named_the_keys_gen_way() {
        // vast-777 is off the prefix: its key is devtest-vast-777 (as `keys gen` named it).
        let dir = tmpdir("rename-keyname");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "vast-777,sk-v\n");
        let keys = FakeKeys::with(&[("h-v", "devtest-vast-777")]);
        let f = Fleet::new(vec![pod("v-777", "vast-777", "vast", None)]);
        handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &KeyBook::new(&csv, Some(&keys)), one("vast-777", "echo"), true, false, true)
            .await
            .unwrap();
        assert_eq!(keys.calls(), ["rename h-v devtest-echo"]);
        assert!(cohort_csv(&dir.0).contains("devtest-echo,sk-v"));
    }

    #[test]
    fn csv_moves_in_place_or_into_the_current_cohort_file() {
        let src = "# h\na,sk-a\nb,sk-b\nx,sk-x\ngone,sk-g\n";
        let live = ["A".to_string(), "B".into(), "x".into()];
        // In place (the link already points at this cohort's file): rows renamed there.
        let mv = plan_csv_move(src, None, &[("a", "A"), ("b", "x")], &live);
        assert_eq!(mv.dst.as_deref(), Some("# h\nA,sk-a\nb,sk-b\nx,sk-x\ngone,sk-g\n"));
        assert_eq!((mv.moved, mv.taken, mv.src), (vec!["a → A".to_string()], vec!["x".to_string()], None));
        // Cross-cohort: moved into dst (taken out of src), other live rows carried (copied).
        let mv = plan_csv_move(src, Some("# new\n"), &[("a", "A"), ("b", "B"), ("nope", "N")], &live);
        assert_eq!(
            mv,
            CsvMove {
                dst: Some("# new\nA,sk-a\nB,sk-b\nx,sk-x\n".into()),
                src: Some("# h\nx,sk-x\ngone,sk-g\n".into()),
                moved: vec!["a → A".into(), "b → B".into()],
                taken: vec![],
                carried: vec!["x".into()],
            }
        );
        // A new name with a row already (either file): left alone; dst rows never overwritten.
        let mv = plan_csv_move(src, Some("# new\nA,sk-other\nx,sk-x2\n"), &[("a", "A"), ("b", "B")], &live);
        assert_eq!(mv.taken, ["A"]);
        assert_eq!(mv.dst.as_deref(), Some("# new\nA,sk-other\nx,sk-x2\nB,sk-b\n"));
        assert!(mv.carried.is_empty());
        // Nothing of ours in the old file: nothing written anywhere (the link stays put).
        assert_eq!(plan_csv_move(src, Some("# new\n"), &[("q", "Q")], &live), CsvMove::default());
        assert_eq!(plan_csv_move(src, None, &[("q", "Q")], &live), CsvMove::default());
    }

    #[tokio::test]
    async fn a_batch_that_stops_part_way_still_moves_what_it_renamed_but_not_the_proxy() {
        let dir = tmpdir("rename-partial");
        let cfg = cfg(&dir.0);
        let mut f = Fleet::new(vec![
            pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
        ]);
        f.fail_rename = vec!["b"];
        let remote = Arc::new(FakeRemote::new());
        let csv = keys_csv(&dir.0, "arena8-alpha,sk-a\narena8-bravo,sk-b\n");
        let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo")]);
        let book = KeyBook::new(&csv, Some(&keys));
        let e = handle_rename(&f, remote.clone(), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), false, false, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("arena8-bravo → devtest-bravo failed") && e.contains("1 of 2 rename(s) done"), "{e}");
        // alpha (renamed) got its ~/.name and key; bravo (not renamed) kept its own.
        assert_eq!(remote.calls(), [name_write("10.0.0.1:22001", "alpha")]);
        assert!(cohort_csv(&dir.0).contains("devtest-alpha,sk-a\narena8-bravo,sk-b"));
        assert_eq!(keys.calls(), ["rename h-a devtest-alpha"]);
        // No proxy write on a partial prefix batch (it would drop the arena8-* forwards).
        assert_eq!(proxy_file(&dir.0), "");
    }

    #[test]
    fn rename_dry_run_lists_every_follow_up_step() {
        let dir = tmpdir("rename-steps");
        let f = fleet();
        let pods = f.pods.lock().unwrap().clone();
        let plan = vec![
            PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-echo".into() },
            PlannedRename { id: "r-charlie".into(), old: "devtest-charlie".into(), new: "solo-gpu".into() },
        ];
        let missing = dir.0.join("openrouter_api_keys.csv");
        let steps = rename_followup_steps("devtest", &plan, &pods, &no_api(&missing), false).join("\n");
        assert!(steps.contains("1. rewrite ~/.name") && steps.contains("devtest-echo ← 'echo'") && steps.contains("solo-gpu ← 'solo-gpu'"), "{steps}");
        assert!(steps.contains("no SSH endpoint right now") && steps.contains(": solo-gpu"), "{steps}");
        assert!(steps.contains("2. OpenRouter key mapping: no ") && steps.contains("no OPENROUTER_PROVISIONING_KEY"), "{steps}");
        assert!(steps.contains("3. redeploy the proxy"), "{steps}");
        // With a CSV and the API: what moves, what's left alone, and the key rename.
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-charlie,sk-c\nsolo-gpu,sk-s\n");
        let keys = FakeKeys::default();
        let steps = rename_followup_steps("devtest", &plan, &pods, &KeyBook::new(&csv, Some(&keys)), true).join("\n");
        assert!(steps.contains("move rows in") && steps.contains("devtest-alpha → devtest-echo"), "{steps}");
        assert!(steps.contains("already have their own row (left alone): solo-gpu"), "{steps}");
        assert!(steps.contains("rename each machine's key on OpenRouter") && steps.contains("proxy NOT synced"), "{steps}");
        assert!(!steps.contains("sk-"), "never prints a key: {steps}");
        assert!(keys.calls().is_empty(), "a preview makes no API call");
    }

    #[tokio::test]
    async fn terminate_revoke_key_revokes_and_drops_the_row_with_or_without_a_key() {
        let dir = tmpdir("terminate-revoke");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-bravo,sk-b\n");
        let keys = FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-a2", "devtest-alpha"), ("h-b", "devtest-bravo")]);
        let book = KeyBook::new(&csv, Some(&keys));
        let f = fleet();
        handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-alpha"]);
        // Every key named after the machine is revoked; its row dropped, the others kept.
        assert_eq!(keys.calls(), ["delete h-a", "delete h-a2"]);
        assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
        // A machine with no key and no row: terminated, nothing to revoke — not a failure.
        handle_terminate(&f, &cfg, &book, Some("charlie"), topts(true, true, false, true)).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-alpha", "terminate r-charlie"]);
        assert_eq!(keys.calls().len(), 2);
        // A dry run touches nothing, on either side.
        handle_terminate(&f, &cfg, &book, Some("bravo"), topts(true, true, true, true)).await.unwrap();
        assert_eq!((f.calls().len(), keys.calls().len()), (2, 2));
        // Without --revoke-key the key and row stay.
        handle_terminate(&f, &cfg, &book, Some("bravo"), topts(false, true, false, true)).await.unwrap();
        assert!(cohort_csv(&dir.0).contains("devtest-bravo,sk-b") && keys.names() == ["devtest-bravo"]);
    }

    #[tokio::test]
    async fn a_failed_revoke_never_blocks_terminate_but_fails_the_exit() {
        let dir = tmpdir("terminate-all");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-bravo,sk-b\n");
        let keys = FakeKeys { broken: vec!["devtest-bravo"], ..FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-b", "devtest-bravo")]) };
        let book = KeyBook::new(&csv, Some(&keys));
        let f = fleet();
        super::sync_proxy(&cfg, &f, "test setup").await; // forwards for the fleet, to see them go
        assert!(proxy_file(&dir.0).contains("name=devtest-alpha"));
        let e = handle_terminate(&f, &cfg, &book, None, topts(true, false, false, true)).await.unwrap_err().to_string();
        // Every pod went, bravo's revoke failed: named, non-zero, and its row is kept as the
        // record that the key still works.
        assert_eq!(f.calls().len(), 4, "{:?}", f.calls());
        assert!(e.contains("revoking the OpenRouter key failed for devtest-bravo") && e.contains("ARE terminated"), "{e}");
        assert!(e.contains("arena keys revoke devtest-bravo"), "{e}");
        let text = cohort_csv(&dir.0);
        assert!(!text.contains("devtest-alpha") && text.contains("devtest-bravo,sk-b"), "{text}");
        assert_eq!(keys.names(), ["devtest-bravo"]);
        // The proxy was still synced after the terminates (the revoke failure came after).
        let px = proxy_file(&dir.0);
        assert!(!px.contains("name=devtest-alpha") && !px.contains("name=devtest-bravo"), "{px}");
    }

    #[tokio::test]
    async fn revoke_key_without_a_provisioning_key_is_refused_before_anything() {
        let dir = tmpdir("terminate-nokey");
        let f = fleet();
        let e = handle_terminate(&f, &cfg(&dir.0), &no_api(&dir.0.join("k.csv")), Some("alpha"), topts(true, true, false, true))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("--revoke-key needs OPENROUTER_PROVISIONING_KEY"), "{e}");
        assert!(f.calls().is_empty());
        // Through the command line too (no OPENROUTER_PROVISIONING_KEY in this config).
        let r = handle_pods(pods_cmd(&["terminate", "alpha", "--revoke-key"]), &f, Arc::new(FakeRemote::new()), &cfg(&dir.0), true).await;
        assert!(r.unwrap_err().to_string().contains("OPENROUTER_PROVISIONING_KEY"));
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn a_name_another_pod_still_holds_keeps_its_key() {
        // A double create left two pods named devtest-alpha. Removing the extra one by id
        // (as the ambiguity error advises) must not revoke the key the survivor uses.
        let dir = tmpdir("terminate-twin");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\n");
        let keys = FakeKeys::with(&[("h-a", "devtest-alpha")]);
        let book = KeyBook::new(&csv, Some(&keys));
        let twins = || {
            let f = fleet();
            f.pods.lock().unwrap().push(pod("r-alpha-dup", "devtest-alpha", "runpod", Some(("10.0.0.1", 22009))));
            f
        };
        let f = twins();
        handle_terminate(&f, &cfg, &book, Some("r-alpha-dup"), topts(true, true, true, true)).await.unwrap(); // dry run
        assert!(f.calls().is_empty() && keys.calls().is_empty());
        handle_terminate(&f, &cfg, &book, Some("r-alpha-dup"), topts(true, true, false, true)).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-alpha-dup"]);
        assert!(keys.calls().is_empty(), "the survivor's key is kept: {:?}", keys.calls());
        assert!(cohort_csv(&dir.0).contains("devtest-alpha,sk-a"), "and so is its row");
        // --all with both twins going: the shared key is revoked ONCE (not a second delete
        // of a hash already gone, which the real API would fail).
        let f = twins();
        handle_terminate(&f, &cfg, &book, None, topts(true, true, false, true)).await.unwrap();
        assert_eq!(keys.calls(), ["delete h-a"]);
        assert!(!cohort_csv(&dir.0).contains("devtest-alpha"));
    }

    #[tokio::test]
    async fn revoke_finds_the_key_by_the_name_keys_gen_gave_it() {
        // Vast's fallback name `vast-777` is off the prefix: `keys gen` named its key
        // devtest-vast-777 (key_name), with the CSV row under the pod name.
        let dir = tmpdir("terminate-keyname");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "vast-777,sk-v\ndevtest-bravo,sk-b\n");
        let keys = FakeKeys::with(&[("h-v", "devtest-vast-777"), ("h-b", "devtest-bravo")]);
        let book = KeyBook::new(&csv, Some(&keys));
        let f = Fleet::new(vec![pod("v-777", "vast-777", "vast", None)]);
        handle_terminate(&f, &cfg, &book, Some("vast-777"), topts(true, true, false, true)).await.unwrap();
        assert_eq!(keys.calls(), ["delete h-v"]);
        assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
    }

    #[tokio::test]
    async fn a_csv_row_whose_key_is_already_gone_is_dropped_quietly() {
        // `keys revoke` leaves the CSV as-is; terminating the pod later finds no key under
        // its name (the full listing) and drops the dead row — not a failure.
        let dir = tmpdir("terminate-row-only");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-dead\ndevtest-bravo,sk-b\n");
        let keys = FakeKeys::with(&[("h-b", "devtest-bravo")]);
        let book = KeyBook::new(&csv, Some(&keys));
        let f = fleet();
        handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await.unwrap();
        assert!(keys.calls().is_empty());
        assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
    }

    #[tokio::test]
    async fn a_csv_that_cant_be_rewritten_is_not_reported_as_a_failed_revoke() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("terminate-csv-ro");
        let cfg = cfg(&dir.0);
        let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\n");
        let keys = FakeKeys::with(&[("h-a", "devtest-alpha")]);
        let book = KeyBook::new(&csv, Some(&keys));
        // The keys file and its directory read-only: the revoke works, the row can't go.
        let file = dir.0.join("devtest_openrouter_keys.csv");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555)).unwrap();
        let f = fleet();
        let r = handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await;
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = r.unwrap_err().to_string();
        assert_eq!(keys.calls(), ["delete h-a"]);
        assert!(e.contains("devtest-alpha ARE revoked (or were already gone), but their rows couldn't be removed from"), "{e}");
        assert!(e.contains("delete those rows by hand"), "{e}");
        assert!(!e.contains("retry with `arena keys revoke"), "keys revoke can't fix a local file: {e}");
        assert_eq!(f.calls(), ["terminate r-alpha"]);
    }

    const FAST: SettleWait = SettleWait { stable_secs: 0, timeout_secs: 5, poll: Duration::ZERO };

    fn restart_opts(wipe_ok: bool) -> RestartOpts {
        RestartOpts { wipe_ok, yes: true, ..Default::default() }
    }

    #[tokio::test]
    async fn restart_of_a_volumeless_gpu_pod_is_refused_without_wipe_ok_even_with_yes() {
        let dir = tmpdir("restart-refused");
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        let keys_dir = dir.0.display().to_string();
        let e = handle_restart(&f, remote.clone(), &cfg(&dir.0), "alpha", restart_opts(false), FAST, &keys_dir)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to restart devtest-alpha without --wipe-ok") && e.contains("--yes alone isn't enough"), "{e}");
        // An unreadable spec (volume unknown) counts as no volume.
        let mut g = fleet();
        g.volumes = vec![("r-alpha", 50)];
        g.no_spec = vec!["r-alpha"];
        assert!(handle_restart(&g, remote.clone(), &cfg(&dir.0), "alpha", restart_opts(false), FAST, &keys_dir).await.is_err());
        assert!(f.calls().is_empty() && g.calls().is_empty() && remote.calls().is_empty(), "nothing restarted");
        // Through the command line: the same refusal (the gate comes before any wait).
        let r = handle_pods(pods_cmd(&["restart", "alpha"]), &f, remote.clone(), &cfg(&dir.0), true).await;
        assert!(r.unwrap_err().to_string().contains("without --wipe-ok"));
        handle_pods(pods_cmd(&["restart", "alpha", "--dry-run"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap();
        assert!(f.calls().is_empty() && remote.calls().is_empty());
    }

    #[tokio::test]
    async fn restart_with_wipe_ok_comes_back_set_up_and_proxied() {
        let dir = tmpdir("restart-ok");
        let cfg = cfg(&dir.0);
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        let keys_dir = dir.0.display().to_string();
        handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-alpha"]);
        // Setup ran again on alpha (and only alpha) — on the endpoint it came back on (the
        // fake moves it on restart), so setup ran AFTER the restart, not before: deploy key,
        // then the config command, which writes ~/.name again.
        let calls = remote.calls();
        assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "{calls:?}");
        assert!(matches!(&calls[0], RemoteCall::Copy { remote, .. } if remote == "/root/.ssh/id_ed25519"), "{calls:?}");
        assert!(matches!(&calls[1], RemoteCall::Exec { cmd, .. } if cmd.contains(&name_file_command("alpha"))), "{calls:?}");
        // …and the proxy was synced afterwards, to the new endpoint.
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
    }

    #[tokio::test]
    async fn restart_needs_no_wipe_ok_only_when_the_work_survives() {
        let dir = tmpdir("restart-volume");
        let cfg = cfg(&dir.0);
        let keys_dir = dir.0.display().to_string();
        // A pod WITH a volume but the repo in its default place (/root/r): the restart still
        // wipes the participants' work — refused without --wipe-ok, even with --yes. The pod
        // was asked where its repo is (read-only, bounded); it said nothing usable.
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        let e = handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to restart devtest-bravo without --wipe-ok") && e.contains("the repo at /root/r"), "{e}");
        assert!(f.calls().is_empty());
        assert_eq!(
            remote.calls(),
            [RemoteCall::Exec { host: "10.0.0.1:22002".into(), cmd: probe_command("/root/r"), timeout: Some(PROBE_TIMEOUT) }],
            "only the probe"
        );
        // The repo on the volume (BACKUP_REPO_PATH under /workspace): it survives, so no
        // --wipe-ok — still re-set-up afterwards.
        let on_volume = Config::parse(&format!("{}BACKUP_REPO_PATH=/workspace/r\n", cfg_text(&dir.0)));
        handle_restart(&f, remote.clone(), &on_volume, "bravo", restart_opts(false), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-bravo"]);
        assert!(!remote.calls_to("10.0.0.1:23002").is_empty(), "setup ran");
        // ...but not on a volume-less pod, wherever the repo is.
        let (g, remote) = (fleet(), Arc::new(FakeRemote::new()));
        assert!(handle_restart(&g, remote.clone(), &on_volume, "bravo", restart_opts(false), FAST, &keys_dir).await.is_err());
        assert!(g.calls().is_empty());
        // A Hetzner VM keeps its disk: not refused, nothing re-provisioned.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        handle_restart(&f, remote.clone(), &cfg, "delta", restart_opts(false), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart h-delta"]);
        assert!(remote.calls().is_empty());
        // --no-setup: restarted (with --wipe-ok), not provisioned.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        let opts = RestartOpts { no_setup: true, ..restart_opts(true) };
        handle_restart(&f, remote.clone(), &cfg, "alpha", opts, FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-alpha"]);
        assert!(remote.calls().is_empty());
    }

    /// The gate follows where the repo REALLY is: setup links `/root/r` onto the volume, which
    /// the configured path can't show — so a pod with a volume is asked. Linked onto a mounted
    /// volume: no --wipe-ok needed. The volume not mounted, a plain directory, or no answer at
    /// all: refused as before.
    #[tokio::test]
    async fn restart_asks_the_pod_where_its_repo_really_is() {
        let dir = tmpdir("restart-real-path");
        let cfg = cfg(&dir.0);
        let keys_dir = dir.0.display().to_string();
        let said = |real: &str, mount: &str| {
            FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={real}\narena-repo-mount={mount}\n"))
        };
        // Linked onto the mounted volume: restarted without --wipe-ok, then set up again.
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [said("/workspace/r", "yes")]);
        handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-bravo"]);
        assert!(matches!(&remote.calls_to("10.0.0.1:22002")[..], [RemoteCall::Exec { cmd, .. }] if *cmd == probe_command("/root/r")));
        assert!(!remote.calls_to("10.0.0.1:23002").is_empty(), "setup ran on the restarted pod");
        // Refused: (the pod's answer) — nothing restarted.
        for answer in [
            said("/workspace/r", "no"),                     // the volume isn't mounted there
            said("/root/r", "yes"),                          // still a plain directory
            FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection refused"),
            FakeReply::exit(255, "Permission denied (publickey)"),
            FakeReply::stdout("garbage\n"),
        ] {
            let mut f = fleet();
            f.volumes = vec![("r-bravo", 50)];
            let remote = Arc::new(FakeRemote::new());
            remote.script("10.0.0.1:22002", [answer.clone()]);
            let r = handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir).await;
            let e = r.unwrap_err().to_string();
            assert!(e.contains("refusing to restart devtest-bravo without --wipe-ok"), "{answer:?}: {e}");
            assert!(f.calls().is_empty(), "{answer:?}");
        }
        // The dry run says what the pod said.
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [said("/workspace/r", "yes")]);
        let opts = RestartOpts { dry_run: true, ..restart_opts(false) };
        handle_restart(&f, remote.clone(), &cfg, "bravo", opts, FAST, &keys_dir).await.unwrap();
        assert!(f.calls().is_empty());
        // A pod without a volume is never asked (nothing on it survives anyway).
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        assert!(handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(false), FAST, &keys_dir).await.is_err());
        assert!(remote.calls().is_empty());
    }

    /// A wedged pod doesn't hold the gate: the probe is bounded, and no answer is "refuse".
    #[tokio::test(start_paused = true)]
    async fn a_wedged_pod_is_refused_after_the_probe_budget() {
        let dir = tmpdir("restart-wedged");
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let e = handle_restart(&f, remote.clone(), &cfg(&dir.0), "bravo", restart_opts(false), FAST, &dir.0.display().to_string())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("without --wipe-ok"), "{e}");
        assert!(start.elapsed() <= PROBE_TIMEOUT + Duration::from_secs(1), "{:?}", start.elapsed());
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn a_restart_whose_setup_fails_says_not_to_restart_again() {
        let dir = tmpdir("restart-setup-fails");
        let cfg = cfg(&dir.0);
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        // (on the endpoint the restart moved it to)
        remote.script("10.0.0.1:23001", [FakeReply::ok(), FakeReply::exit(1, "fatal: not a git repository")]);
        let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("devtest-alpha restarted, but it is NOT set up again"), "{e}");
        assert!(e.contains("arena pods setup devtest-alpha") && e.contains("not another restart"), "{e}");
        assert_eq!(f.calls(), ["restart r-alpha"]);
        assert!(proxy_file(&dir.0).contains("name=devtest-alpha"), "the proxy is synced anyway");
        // Setup that can't run at all (no repo config) is caught BEFORE the wipe.
        let bare = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha)\n");
        let f = fleet();
        let e = handle_restart(&f, remote.clone(), &bare, "alpha", restart_opts(true), FAST, "/nonexistent").await.unwrap_err();
        assert!(format!("{e:#}").contains("pass --no-setup"), "{e:#}");
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn disk_fate_follows_the_backend_and_the_volume() {
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        f.no_spec = vec!["r-charlie"];
        let pods = f.pods.lock().unwrap().clone();
        let fate = |i: usize| disk_fate(&f, &pods[i]);
        assert_eq!(fate(0).await, DiskFate::Wiped { volume_gb: Some(0) });
        assert_eq!(fate(1).await, DiskFate::Wiped { volume_gb: Some(50) });
        assert_eq!(fate(2).await, DiskFate::Wiped { volume_gb: None });
        assert_eq!(fate(3).await, DiskFate::Kept);
    }

    /// Live R (2026-10-09): a RunPod v1 restart kept the container disk (marker files and
    /// `/.dockerenv` survived), while the output said "the restart reset them to the image".
    /// v1 stays gated (cheap safety; v1 retires 2026-11-16) and is set up again in case — but
    /// nothing says a reset happened. v2's restart, which does reset, still says so.
    #[tokio::test]
    async fn a_restart_not_known_to_reset_is_gated_and_set_up_again_but_never_said_to_reset() {
        let dir = tmpdir("restart-v1");
        let cfg = cfg(&dir.0);
        let keys_dir = dir.0.display().to_string();
        let mut f = fleet();
        f.restart_resets = ContainerReset::Unknown;
        f.volumes = vec![("r-bravo", 50)];
        let pods = f.pods.lock().unwrap().clone();
        // The fate: gated like a wipe, with or without a volume; a VM is still kept.
        assert_eq!(restart_fate(&f, &pods[0]).await, DiskFate::MayWipe { volume_gb: Some(0) });
        assert_eq!(restart_fate(&f, &pods[1]).await, DiskFate::MayWipe { volume_gb: Some(50) });
        assert_eq!(restart_fate(&f, &pods[3]).await, DiskFate::Kept);
        // The gate holds (--yes alone isn't enough), worded as a possibility.
        let remote = Arc::new(FakeRemote::new());
        let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(false), FAST, &keys_dir).await.unwrap_err().to_string();
        assert!(e.contains("refusing to restart devtest-alpha without --wipe-ok: this may reset the container disk"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty());
        // With --wipe-ok: restarted, and set up again on the endpoint it came back on.
        handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-alpha"]);
        assert!(!remote.calls_to("10.0.0.1:23001").is_empty(), "set up again, in case");
        // What it says, before and after — never that the reset happened.
        let may = DiskFate::MayWipe { volume_gb: Some(0) };
        let text = may.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
        assert!(text.starts_with("⚠ may WIPE the container disk") && text.contains("NO persistent volume"), "{text}");
        assert_eq!(may.reset(), ContainerReset::Unknown);
        let line = reprovision_line(1, "restart", ContainerReset::Unknown);
        assert_eq!(line, "Re-provisioning 1 pod(s) (in case the restart reset them to the image — it may not have)…");
        assert!(afterwards_line(ContainerReset::Unknown, false, false).contains("(no setup: if it was reset, it comes back as the bare image)"));
        assert!(afterwards_line(ContainerReset::Unknown, false, true).contains("if it was reset"));
        assert_eq!(afterwards_line(ContainerReset::Unknown, true, false), afterwards_line(ContainerReset::Yes, true, false));
        assert!(no_setup_line(&pods[..1], ContainerReset::Unknown).starts_with("(--no-setup: devtest-alpha may be back as the bare image"));
        // A known reset (v2, reimage, start) still says it plainly.
        assert_eq!(reprovision_line(2, "restart", ContainerReset::Yes), "Re-provisioning 2 pod(s) (the restart reset them to the image)…");
        assert!(no_setup_line(&pods[..1], ContainerReset::Yes).starts_with("(--no-setup: devtest-alpha back as the bare image"));
        let v2 = DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
        assert!(v2.starts_with("⚠ WIPES the container disk (reset to the image)"), "{v2}");
        let r = wipe_refusal(DiskOp::Restart, false, &["devtest-a"], "/root/r");
        assert!(r.contains("this resets the container disk to the image"), "{r}");
        // A restart whose setup fails: don't restart again — it "may" wipe, not "would again".
        let mut g = fleet();
        g.restart_resets = ContainerReset::Unknown;
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:23001", [FakeReply::ok(), FakeReply::exit(1, "fatal: not a git repository")]);
        let e = handle_restart(&g, remote, &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap_err().to_string();
        assert!(e.contains("not another restart — that may wipe it)") && !e.contains("wipe it again"), "{e}");
    }

    /// Live R: `restart` on a stopped Hetzner VM said `pods start` brings it back "as a fresh
    /// image and is set up again" — a VM keeps its disk and isn't set up again.
    #[tokio::test]
    async fn restart_on_a_stopped_pod_says_what_a_start_brings_back() {
        let dir = tmpdir("restart-stopped-vm");
        let cfg = cfg(&dir.0);
        let f = fleet();
        f.pods.lock().unwrap()[3].status = "OFF".into(); // delta, the Hetzner VM
        let remote = Arc::new(FakeRemote::new());
        let e = handle_restart(&f, remote.clone(), &cfg, "delta", restart_opts(false), FAST, &dir.0.display().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("devtest-delta is stopped (OFF)") && e.contains("`arena pods start devtest-delta`"), "{e}");
        assert!(e.contains("its disk was kept") && !e.contains("fresh image"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty());
        // A RunPod pod's start does bring it back as a fresh image.
        let gpu = pod("r-x", "devtest-x", "runpod", None);
        assert!(stopped_restart_refusal(&gpu, true).contains("(it comes back as a fresh image and is set up again)"));
    }

    #[test]
    fn disk_fate_text_and_gate() {
        // The gate follows the WORK (the repo), not just "is there a volume".
        const ROOT: &str = "/root/ARENA_materials";
        const VOL: &str = "/workspace/ARENA_materials";
        let v50 = DiskFate::Wiped { volume_gb: Some(50) };
        let cfg_only = RepoSite::configured;
        // What the pod said: its repo's real path, and whether /workspace is mounted.
        let pod_said = |real: &str, mounted: bool| RepoSite { configured: ROOT.into(), on_pod: Some((real.into(), mounted)) };
        let linked = pod_said(VOL, true);
        let cases: &[(DiskFate, RepoSite, bool, &[&str])] = &[
            (DiskFate::Kept, cfg_only(ROOT), false, &["disk kept"]),
            // a volume, but the repo in the default place: lost — gated
            (v50, cfg_only(ROOT), true, &["WIPES the container disk", "the ARENA repo at /root/ARENA_materials (participants' work) is NOT on the volume", "only the 50 GB volume at /workspace survives"]),
            // the repo on the volume: survives — not gated
            (v50, cfg_only(VOL), false, &["everything outside /workspace is lost", "the repo at /workspace/ARENA_materials is on the 50 GB volume at /workspace and survives"]),
            // setup linked it there, the pod says so: survives — not gated, and the prompt says where
            (v50, linked.clone(), false, &["the repo at /root/ARENA_materials → /workspace/ARENA_materials on the pod is on the 50 GB volume"]),
            // linked, but the volume isn't mounted on the pod / still a plain directory: gated
            (v50, pod_said(VOL, false), true, &["isn't mounted there", "is NOT on the volume"]),
            (v50, pod_said(ROOT, true), true, &["the ARENA repo at /root/ARENA_materials (participants' work) is NOT on the volume"]),
            // no volume at all: whatever the pod says, nothing survives
            (DiskFate::Wiped { volume_gb: Some(0) }, linked.clone(), true, &["NO persistent volume"]),
            (DiskFate::Wiped { volume_gb: Some(0) }, cfg_only(VOL), true, &["WIPES the container disk", "NO persistent volume", "~/.name", "repo at /workspace/ARENA_materials"]),
            (DiskFate::Wiped { volume_gb: None }, cfg_only(ROOT), true, &["couldn't read whether it has a volume", "assume nothing survives"]),
            (DiskFate::Wiped { volume_gb: None }, cfg_only(VOL), true, &["assume nothing survives"]),
        ];
        for (fate, site, gated, says) in cases {
            assert_eq!(fate.needs_wipe_ok(site), *gated, "{fate:?} {site:?}");
            let text = fate.describe(DiskOp::Restart, site);
            for s in *says {
                assert!(text.contains(s), "{fate:?} {site:?}: {text}");
            }
        }
        assert!(DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Stop, &cfg_only(ROOT)).contains("starts again as a fresh image"));
        assert!(DiskFate::Kept.describe(DiskOp::Stop, &cfg_only(ROOT)).contains("powered off"));
        // A stopped pod comes back only when started — the prompt says to re-run setup then.
        assert!(v50.describe(DiskOp::Stop, &linked).contains("`arena pods start` brings it back and re-links the repo"));
        assert!(!v50.describe(DiskOp::Restart, &linked).contains("re-links"));
        let r = wipe_refusal(DiskOp::Restart, false, &["devtest-a", "devtest-b"], ROOT);
        assert!(r.starts_with("refusing to restart devtest-a, devtest-b without --wipe-ok"), "{r}");
        assert!(r.contains("the repo at /root/ARENA_materials") && r.contains("outside /workspace"), "{r}");
        assert!(r.contains("`arena pods backup devtest-a devtest-b`") && r.contains("--yes alone isn't enough"), "{r}");
        assert!(wipe_refusal(DiskOp::Stop, false, &["x"], ROOT).contains("terminate instead"));
    }

    #[test]
    fn only_a_repo_at_or_under_the_mount_is_on_the_volume() {
        for (repo, on) in [
            ("/workspace", true),
            ("/workspace/ARENA", true),
            ("/workspace/ARENA/", true),
            ("/root/ARENA_materials", false),
            ("/workspacex/ARENA", false),
            ("workspace/ARENA", false),
            ("~/ARENA", false),
            ("/workspace/../root/ARENA", false),
            ("", false),
        ] {
            assert_eq!(RepoSite::configured(repo).on_volume(), on, "{repo}");
        }
    }

    #[tokio::test]
    async fn stop_follows_the_same_work_survives_rule() {
        let dir = tmpdir("stop-volume");
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        // bravo has a volume, but the repo (/root/r) isn't on it: refused without --wipe-ok.
        let e = handle_pods(pods_cmd(&["stop", "bravo"]), &f, Arc::new(FakeRemote::new()), &cfg(&dir.0), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to stop devtest-bravo without --wipe-ok") && e.contains("the repo at /root/r"), "{e}");
        assert!(f.calls().is_empty());
        // With the repo on the volume it goes ahead (and a Hetzner VM always does).
        let on_volume = Config::parse(&format!("{}BACKUP_REPO_PATH=/workspace/r\n", cfg_text(&dir.0)));
        handle_pods(pods_cmd(&["stop", "bravo", "delta"]), &f, Arc::new(FakeRemote::new()), &on_volume, true).await.unwrap();
        assert_eq!(f.calls(), ["stop r-bravo", "stop h-delta"]);
        // …and so it does when setup linked it onto the volume — the pod says so. Only the pod
        // with a volume is asked; alpha (no volume) is refused whatever it would say.
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [FakeReply::stdout("arena-repo-real=/workspace/r\narena-repo-mount=yes\n")]);
        handle_pods(pods_cmd(&["stop", "bravo", "delta"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap();
        assert_eq!(f.calls(), ["stop r-bravo", "stop h-delta"]);
        assert_eq!(remote.calls().len(), 1, "one probe, to bravo");
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [FakeReply::stdout("arena-repo-real=/workspace/r\narena-repo-mount=yes\n")]);
        let e = handle_pods(pods_cmd(&["stop", "alpha", "bravo"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap_err();
        assert!(e.to_string().contains("refusing to stop devtest-alpha without --wipe-ok"), "{e}");
        assert!(remote.calls_to("10.0.0.1:22001").is_empty());
        // The prompt / dry-run line per pod names it and what happens to its disk.
        let pods = f.pods.lock().unwrap().clone();
        let sites = [RepoSite::configured("/root/r"), RepoSite::configured("/root/r")];
        let lines = stop_lines(&[&pods[1], &pods[3]], &[DiskFate::Wiped { volume_gb: Some(50) }, DiskFate::Kept], &sites);
        assert!(lines[0].starts_with("  devtest-bravo (id=r-bravo, runpod): ⚠ discards the container disk"), "{lines:?}");
        assert!(lines[0].contains("the ARENA repo at /root/r (participants' work) is NOT on the volume"), "{lines:?}");
        // (a VM is shut down gracefully now — powered off only if it hangs: live R)
        assert_eq!(lines[1], "  devtest-delta (id=h-delta, hetzner): disk kept — shut down (powered off only if it hangs), files stay");
    }

    fn pods_cmd(argv: &[&str]) -> PodCmd {
        match Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).map(|c| c.cmd) {
            Ok(Cmd::Pods(p)) => p,
            other => panic!("{argv:?}: {:?}", other.err()),
        }
    }

    /// `cfg` plus a readable public key (reimage re-seeds PUBLIC_KEY and refuses without
    /// one), an image and a GPU type (a replacement spec needs one: the fakes report none).
    fn keyed_cfg(dir: &Path, extra: &str) -> Config {
        std::fs::write(dir.join("k"), "private").unwrap();
        std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest devtest-test").unwrap();
        Config::parse(&format!(
            "{}SHARED_SSH_KEY_PATH={}\nIMAGE=img:9\nGPU_TYPE=\"NVIDIA RTX A4000\"\n{extra}",
            cfg_text(dir),
            dir.join("k").display()
        ))
    }

    fn named(pods: &[Pod]) -> Selected {
        Selected { pods: pods.to_vec(), named: true }
    }

    /// Live findings #2 + #8: `reimage` wiped a volume-less pod with only `-y` (stop and
    /// restart beside it refused), and left it unprovisioned (no ~/.name, the public git
    /// remote, no deploy key). Now the same `--wipe-ok` gate — refused up front, the dry run
    /// says so, a repo on the volume passes — and afterwards it settles, runs setup again ON
    /// THE NEW CONTAINER and syncs the proxy to it.
    #[tokio::test]
    async fn reimage_is_gated_like_restart_and_sets_the_pod_up_again() {
        let dir = tmpdir("reimage");
        let cfg = keyed_cfg(&dir.0, "");
        let keys_dir = dir.0.display().to_string();
        // Refused without --wipe-ok, even with --yes (handle_pods passes it): nothing reimaged.
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        let e = handle_pods(pods_cmd(&["reimage", "alpha", "bravo"]), &f, remote.clone(), &cfg, true).await.unwrap_err().to_string();
        assert!(e.contains("refusing to reimage devtest-alpha, devtest-bravo without --wipe-ok"), "{e}");
        assert!(e.contains("--yes alone isn't enough") && e.contains("`arena pods backup devtest-alpha devtest-bravo`"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty());
        // The dry run says it would be refused (and changes nothing).
        take_would_refuse();
        let dry = RestartOpts { dry_run: true, yes: true, ..Default::default() };
        let pods = f.pods.lock().unwrap().clone();
        handle_reimage(&f, remote.clone(), &cfg, &named(&pods[..1]), None, dry, FAST, &keys_dir).await.unwrap();
        let notes = take_would_refuse();
        assert!(notes.len() == 1 && notes[0].contains("refusing to reimage devtest-alpha without --wipe-ok"), "{notes:?}");
        assert!(f.calls().is_empty());
        // --wipe-ok: reimaged, set up again on the endpoint it came back on, proxied there.
        let opts = RestartOpts { wipe_ok: true, yes: true, ..Default::default() };
        handle_reimage(&f, remote.clone(), &cfg, &named(&pods[..1]), None, opts, FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["reimage r-alpha img:9"]);
        let calls = remote.calls();
        assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "{calls:?}");
        assert!(calls.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains(&name_file_command("alpha")))), "{calls:?}");
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
        // The repo on a volume survives a reimage: no --wipe-ok needed.
        let mut g = fleet();
        g.volumes = vec![("r-bravo", 50)];
        let on_volume = keyed_cfg(&dir.0, "BACKUP_REPO_PATH=/workspace/r\n");
        let bravo = g.pods.lock().unwrap()[1].clone();
        let opts = RestartOpts { no_setup: true, skip_proxy: true, yes: true, ..Default::default() };
        handle_reimage(&g, Arc::new(FakeRemote::new()), &on_volume, &named(&[bravo]), None, opts, FAST, &keys_dir).await.unwrap();
        assert_eq!(g.calls(), ["reimage r-bravo img:9"]);
    }

    /// Live finding #15: a stopped pod had no way back — `restart` answered a raw 409 (v2)
    /// or "restarted" it without starting it (v1), and `run` said "no SSH endpoint yet". Now
    /// `restart` points at `pods start`, which starts it, sets it up again (it's a fresh
    /// image) and syncs the proxy; the SSH commands say it's stopped.
    #[tokio::test]
    async fn a_stopped_pod_is_started_set_up_and_proxied() {
        let dir = tmpdir("start");
        let cfg = cfg(&dir.0);
        let keys_dir = dir.0.display().to_string();
        let f = fleet();
        f.pods.lock().unwrap()[0].status = "EXITED".into(); // alpha (its last endpoint still listed)
        let remote = Arc::new(FakeRemote::new());
        let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap_err().to_string();
        assert!(e.contains("devtest-alpha is stopped (EXITED)") && e.contains("`arena pods start devtest-alpha`"), "{e}");
        let e = handle_pods(pods_cmd(&["run", "-t", "alpha", "hostname"]), &f, remote.clone(), &cfg, true).await.unwrap_err().to_string();
        assert!(e.contains("stopped (devtest-alpha)") && e.contains("arena pods start devtest-alpha"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty(), "nothing restarted, nothing reached");
        // Start: the stopped one only (bravo is running: skipped).
        let pods = f.pods.lock().unwrap().clone();
        let opts = RestartOpts { yes: true, ..Default::default() };
        handle_start(&f, remote.clone(), &cfg, &named(&pods[..2]), opts, FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["start r-alpha"]);
        let calls = remote.calls();
        assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "set up on the new container: {calls:?}");
        assert!(proxy_file(&dir.0).contains("23001"));
        // Through the command line: needs targets; `--dry-run` starts nothing.
        assert!(handle_pods(pods_cmd(&["start"]), &f, remote.clone(), &cfg, true).await.is_err());
        let g = fleet();
        g.pods.lock().unwrap()[1].status = "EXITED".into();
        handle_pods(pods_cmd(&["start", "bravo", "--dry-run"]), &g, remote.clone(), &cfg, true).await.unwrap();
        assert!(g.calls().is_empty());
    }

    /// Live R: `copy-keys` on a stopped pod printed the right skip line, then failed with
    /// "none of the selected pods … has an SSH endpoint yet"; every other SSH command says
    /// the pods are stopped, and how to start them.
    #[tokio::test]
    async fn copy_keys_on_a_stopped_pod_says_it_is_stopped() {
        let dir = tmpdir("copy-keys-stopped");
        let cfg = cfg(&dir.0);
        std::fs::write(dir.0.join("openai_api_keys.csv"), "host,key\ndevtest-alpha,sk-a\ndevtest-charlie,sk-c\n").unwrap();
        let keys_dir = dir.0.display().to_string();
        let f = fleet();
        f.pods.lock().unwrap()[0].status = "EXITED".into(); // alpha
        let pods = f.pods.lock().unwrap().clone();
        let remote = Arc::new(FakeRemote::new());
        let e = handle_copy_keys(remote.clone(), &cfg, &keys_dir, None, None, &named(&pods[..1]), false, true)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "the selected pod(s) are stopped (devtest-alpha) — `arena pods start devtest-alpha` first");
        // With a pod that has no endpoint yet among them (charlie), that is what's said.
        let both = named(&[pods[0].clone(), pods[2].clone()]);
        let e = handle_copy_keys(remote.clone(), &cfg, &keys_dir, None, None, &both, false, true).await.unwrap_err().to_string();
        assert!(e.starts_with("none of the selected pods (devtest-alpha, devtest-charlie) has an SSH endpoint yet"), "{e}");
        assert!(remote.calls().is_empty(), "nothing reached");
    }

    /// Live (wave 6): after a restart the endpoint flapped — the proxy sync right after setup
    /// caught a moment the listing showed the pod with no endpoint, and its forward stayed
    /// on the dead one. The sync now waits for a settled endpoint (and re-syncs once).
    #[tokio::test]
    async fn the_post_restart_proxy_sync_waits_for_a_settled_endpoint() {
        let dir = tmpdir("restart-flap");
        let cfg = cfg(&dir.0);
        let mut f = fleet();
        // List calls: 1 the restart's lookup, 2-3 the settle wait, 4 setup's listing — so 5 is
        // the first listing after setup: the one the sync used to take as it came.
        f.blank_lists = vec![5];
        let remote = Arc::new(FakeRemote::new());
        handle_restart(&f, remote, &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string()).await.unwrap();
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
    }

    /// Live: RunPod listed a running pod without an endpoint for ~20 s, `pull` skipped it and
    /// exited 1 — a backup cron tick silently missed that pod. `run` / `pull` / `backup` ask
    /// once more first.
    #[tokio::test(start_paused = true)]
    async fn run_and_backup_ask_again_for_a_running_pod_listed_without_an_endpoint() {
        let dir = tmpdir("endpoint-retry");
        let cfg = cfg(&dir.0);
        for argv in [&["run", "-t", "alpha", "hostname"][..], &["backup", "alpha", "--no-pull"]] {
            let mut f = fleet();
            f.blank_lists = vec![1]; // the selection's listing
            let remote = Arc::new(FakeRemote::new());
            handle_pods(pods_cmd(argv), &f, remote.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(remote.calls().len(), 1, "{argv:?}: {:?}", remote.calls());
            assert_eq!(remote.calls()[0].host(), "10.0.0.1:22001", "{argv:?}");
        }
    }

    /// Live finding #6: `migrate finish` terminated `<name>-old` while the proxy still routed
    /// the participant's port to it (after a `--skip-proxy` cutover). Now it syncs first and
    /// refuses unless the name's stable port routes to the live pod.
    #[tokio::test]
    async fn finish_keeps_old_until_the_proxy_routes_the_name_to_the_live_pod() {
        let dir = tmpdir("finish-proxy");
        let cfg = cfg(&dir.0);
        let pods = |live_endpoint: Option<(&str, u16)>| {
            vec![
                pod("r-bravo-new", "devtest-bravo", "runpod", live_endpoint),
                pod("r-bravo", "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002))),
            ]
        };
        // The live pod has no endpoint yet: the sync can't route bravo to it.
        let f = Fleet::new(pods(None));
        let listed = f.pods.lock().unwrap().clone();
        let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
        assert!(e.contains("NOT terminating devtest-bravo-old") && e.contains("arena proxy apply"), "{e}");
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        // Routed: it goes.
        let f = Fleet::new(pods(Some(("10.0.0.3", 22009))));
        let listed = f.pods.lock().unwrap().clone();
        migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-bravo"]);
        assert!(proxy_file(&dir.0).contains("10.0.0.3"));
    }

    /// Without a proxy, `migrate finish` syncs nothing before its terminate — so `SSH_CONFIG_FILE`
    /// is rewritten after it, from the listing without `-old`: its block goes.
    #[tokio::test]
    async fn finish_without_a_proxy_drops_old_from_the_ssh_config_file() {
        let dir = tmpdir("finish-sshconfig");
        let path = dir.0.join("arena.conf");
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo)\nSSH_CONFIG_FILE={}\n",
            path.display()
        ));
        let f = Fleet::new(vec![
            pod("r-bravo-new", "devtest-bravo", "runpod", Some(("10.0.0.3", 22009))),
            pod("r-bravo", "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002))),
        ]);
        let listed = f.pods.lock().unwrap().clone();
        std::fs::write(&path, arena_core::sshconfig::managed_text(&arena_core::sshconfig::render_manual("devtest", "root", "~/.ssh/shared_infra_key", &listed))).unwrap();
        migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { yes: true, ..Default::default() }).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-bravo"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Host devtest-bravo\n    HostName 10.0.0.3\n") && !text.contains("devtest-bravo-old"), "{text}");
    }

    /// The cutover's live findings, on a STOPPED original: #16 — the final sync is refused
    /// saying the source is stopped and offering `--no-final-sync`, which then cuts over to
    /// the copy already on `-new`; #26 — `~/.name` on the promoted pod in setup's own format;
    /// #5 — its per-host API keys re-distributed under the canonical name; #29 — a write-only
    /// proxy honoured up front: no 45 s wait "for nginx", no probe through it, no revert.
    #[tokio::test(start_paused = true)]
    async fn cutover_onto_the_copy_of_a_stopped_original() {
        let dir = tmpdir("cutover");
        let cfg = cfg(&dir.0);
        std::fs::write(dir.0.join("openai_api_keys.csv"), "devtest-alpha,sk-openai-alpha\n").unwrap();
        let keys_dir = dir.0.display().to_string();
        let mut src = pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001)));
        src.status = "EXITED".into();
        let f = Fleet::new(vec![src, pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001)))]);
        let new_host = "10.0.0.2:22001";
        let remote = Arc::new(FakeRemote::new());
        remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
        remote.on(new_host, "arena-last-sync", FakeReply::stdout("arena-last-sync=1791457323\n"));
        let pods = f.pods.lock().unwrap().clone();
        let opts = |no_final_sync| CutoverOpts { skip_proxy: false, no_final_sync, dry_run: false, yes: true };
        let e = migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), &keys_dir).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("the source devtest-alpha is stopped (EXITED)") && msg.contains("--no-final-sync"), "{msg}");
        assert!(f.calls().is_empty() && remote.calls().is_empty(), "nothing swapped, nothing reached");

        let start = tokio::time::Instant::now();
        migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(true), &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["rename r-alpha devtest-alpha-old", "rename r-alpha-new devtest-alpha"]);
        assert!(start.elapsed() < Duration::from_secs(45), "no wait for an nginx arena doesn't reload: {:?}", start.elapsed());
        assert!(remote.calls_to("10.0.0.1:22001").is_empty(), "the stopped original isn't contacted");
        assert!(remote.calls().iter().all(|c| c.host() == new_host), "nothing probed through the proxy: {:?}", remote.calls());
        let on_new: Vec<String> = remote
            .calls_to(new_host)
            .into_iter()
            .filter_map(|c| match c {
                RemoteCall::Exec { cmd, .. } => Some(cmd),
                RemoteCall::Copy { .. } => None,
            })
            .collect();
        assert!(on_new.iter().any(|c| c.ends_with(&name_file_command("alpha"))), "~/.name in setup's format: {on_new:?}");
        assert!(on_new.iter().any(|c| c.contains("OPENAI_API_KEY") && c.contains("sk-openai-alpha")), "keys under the canonical name: {on_new:?}");
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-alpha ") || px.contains("name=devtest-alpha\n") || px.contains("10.0.0.2"), "{px}");
        assert!(px.contains("10.0.0.2") && !px.contains("10.0.0.1"), "routed to the new pod: {px}");
    }

    /// `pods replace` end to end on fakes: the new pod's host has a maintenance window (live
    /// finding #28) — said before the swap, and the original is KEPT, not terminated; the
    /// promoted pod gets `~/.name` and its API keys under the canonical name (#26, #5), the
    /// parked one its `-old` name; the copy is a replica's (deletions mirrored, moved aside).
    #[tokio::test(start_paused = true)]
    async fn replace_onto_a_host_with_maintenance_keeps_the_original() {
        let dir = tmpdir("replace-maint");
        let cfg = keyed_cfg(&dir.0, "");
        std::fs::write(dir.0.join("openai_api_keys.csv"), "devtest-alpha,sk-openai-alpha\n").unwrap();
        let keys_dir = dir.0.display().to_string();
        let mut f = fleet();
        *f.to_create.lock().unwrap() = vec![pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)))];
        f.maintenance = vec![(
            "r-new",
            arena_core::pod::Maintenance { start: Some("2026-10-12T04:00:00Z".into()), end: Some("2026-10-12T12:00:00Z".into()), note: None },
        )];
        let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
        let remote = Arc::new(FakeRemote::new());
        remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
        remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout("r-alpha"));
        remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
        remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let opts = ReplaceOpts { keep_old: false, skip_proxy: false, dry_run: false, yes: true };
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: None };
        replace_on(remote.clone(), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap();
        assert_eq!(
            f.calls(),
            ["create devtest-alpha-new NVIDIA RTX A4000", "rename r-alpha devtest-alpha-old", "rename r-new devtest-alpha"],
            "swapped, but the original NOT terminated"
        );
        let execs = |host: &str| -> Vec<String> {
            remote.calls_to(host).into_iter().filter_map(|c| if let RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None }).collect()
        };
        let (on_old, on_new) = (execs(old_host), execs(new_host));
        let wrote = |cmds: &[String], short: &str| cmds.iter().any(|c| c.ends_with(&name_file_command(short)) && c.contains("RUNPOD_POD_ID"));
        assert!(wrote(&on_new, "alpha") && wrote(&on_old, "alpha-old"), "each on the pod itself: {on_new:?} {on_old:?}");
        assert!(on_new.iter().any(|c| c.contains("OPENAI_API_KEY") && c.contains("sk-openai-alpha")), "{on_new:?}");
        // The copy ran on the original, as a replica: deletions mirrored into the kept folder,
        // the remote rsync started only on the new pod itself.
        assert!(
            on_old.iter().any(|c| c.contains("\nrsync ")
                && c.contains("'--delete'")
                && c.contains("--backup-dir=arena-sync-kept/")
                && c.contains("--filter=P arena-sync-kept/")
                && c.contains("--rsync-path=")
                && c.contains("r-new")),
            "{on_old:?}"
        );
        // …the new pod's baseline was started before it (a pod made for this copy), then sorted
        // after; and the sync that landed is on record on the original.
        let baseline = on_new.iter().position(|c| c.contains("arena-replica-ready") && c.contains("touch \"$s\"")).expect("baseline");
        let settle = on_new.iter().position(|c| c.contains("arena-replica-kept") && c.contains("del=1")).expect("settle");
        assert!(baseline < settle);
        assert!(on_old.iter().any(|c| c.contains("sent-to") && c.contains("'r-new'") && c.contains("rm -f")), "{on_old:?}");
        assert!(proxy_file(&dir.0).contains("10.0.0.2"), "the proxy routes alpha to the new pod");
    }

    /// Review finding (untested branch): a `replace` that terminated the original removes the
    /// leftover local staging copy of its home (a participant's whole home, on the shared box).
    #[tokio::test(start_paused = true)]
    async fn a_finished_replace_removes_the_originals_leftover_staging_copy() {
        let dir = tmpdir("replace-stage");
        let cfg = keyed_cfg(&dir.0, "");
        let keys_dir = dir.0.display().to_string();
        let src_id: &'static str = Box::leak(format!("r-replace-{}", std::process::id()).into_boxed_str());
        let f = Fleet::new(vec![pod(src_id, "devtest-alpha", "runpod", Some(("10.0.0.1", 22001)))]);
        *f.to_create.lock().unwrap() = vec![pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)))];
        let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
        let remote = Arc::new(FakeRemote::new());
        remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
        remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout(src_id));
        remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
        remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
        // Where `replace` looks for it (the system temp dir), removed by the guard even if an
        // assertion fails first — a failed or mutated run leaked these (live R, S11).
        let stage = TmpDir(staging_dir(&std::env::temp_dir(), src_id));
        std::fs::create_dir_all(stage.0.join("ARENA_materials")).unwrap();
        std::fs::write(stage.0.join("ARENA_materials/work.py"), "an earlier failed copy's stage").unwrap();
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let opts = ReplaceOpts { keep_old: false, skip_proxy: false, dry_run: false, yes: true };
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: None };
        replace_on(remote, &f, &cfg, &f, src_id.into(), "devtest-alpha".into(), job).await.unwrap();
        assert_eq!(
            f.calls(),
            [
                "create devtest-alpha-new NVIDIA RTX A4000".to_string(),
                format!("rename {src_id} devtest-alpha-old"),
                "rename r-new devtest-alpha".to_string(),
                format!("terminate {src_id}")
            ]
        );
        assert!(!stage.0.exists(), "removed once the original is gone");
    }

    /// A balance that remembers prices into `dir` for the RunPod account (a fake: no call).
    fn priced_balance(dir: &Path) -> crate::money::BalanceCtx {
        use arena_core::balance::{Account, FakeBalanceApi};
        let path = dir.join("state").join("devtest").join(arena_core::snapshot::BALANCE_FILE);
        crate::money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().with(Account::Runpod)), path: Some(Ok(path)) }
    }

    /// The price the balance state remembers for pod `id`, if any.
    fn remembered(bal: &crate::money::BalanceCtx, id: &str) -> Option<f64> {
        let (state, _) = arena_core::balance::load(bal.state_path().unwrap());
        state.accounts.get(&arena_core::balance::Account::Runpod).and_then(|a| a.prices.get(id)).map(|p| p.0)
    }

    /// `pods replace` remembers the new pod's price the moment it's made, as `create`/`up`
    /// do — by id, so the promoted pod (renamed to the canonical name) keeps it, and a later
    /// listing with no price (RunPod v1's) still prices it.
    #[tokio::test(start_paused = true)]
    async fn replace_remembers_the_new_pods_price_for_the_balance() {
        let dir = tmpdir("replace-price");
        let cfg = keyed_cfg(&dir.0, "");
        let keys_dir = dir.0.display().to_string();
        let f = fleet();
        let mut new = pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)));
        new.cost_per_hr = Some(0.29);
        *f.to_create.lock().unwrap() = vec![new];
        let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
        let remote = Arc::new(FakeRemote::new());
        remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
        remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout("r-alpha"));
        remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
        remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        // --keep-old: the original isn't terminated (no staging clean-up of a shared id).
        let opts = ReplaceOpts { keep_old: true, skip_proxy: false, dry_run: false, yes: true };
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let bal = priced_balance(&dir.0);
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: Some(&bal) };
        replace_on(remote, &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap();
        assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000", "rename r-alpha devtest-alpha-old", "rename r-new devtest-alpha"]);
        assert_eq!(remembered(&bal, "r-new"), Some(0.29));
    }

    /// `migrate copy` remembers `-new`'s price the moment it's created — before the wait for
    /// it to come up, so a copy that then fails (here: it never gets an endpoint) still leaves
    /// the billing pod priced. An unpriced create remembers nothing.
    #[tokio::test(start_paused = true)]
    async fn migrate_copy_remembers_the_new_pods_price_even_when_the_copy_fails() {
        let dir = tmpdir("migrate-price");
        let cfg = keyed_cfg(&dir.0, "");
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let bal = priced_balance(&dir.0);
        for (id, price) in [("r-new", Some(0.33)), ("r-bare", None)] {
            let f = fleet();
            let mut new = pod(id, "", "runpod", None); // never gets an endpoint
            new.cost_per_hr = price;
            *f.to_create.lock().unwrap() = vec![new];
            let pods = f.pods.lock().unwrap().clone();
            let job = MigrateCopyJob {
                setup_timeouts: timeouts,
                ov: &ov,
                place: &place,
                dry_run: false,
                yes: true,
                interrupt: &interrupt,
                keys_dir: "/nonexistent",
                balance: Some(&bal),
            };
            let e = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err();
            assert!(format!("{e:#}").contains("devtest-alpha-new never came up"), "{e:#}");
            assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000"]);
            assert_eq!(remembered(&bal, id), price, "{id}");
        }
    }

    /// Review finding: `replace` / `migrate copy` created (and billed) the new pod, set it up,
    /// and only then found the original stopped — leaving `-new` running and advising a
    /// `migrate copy` that fails the same way. Refused while planning now, and the dry runs
    /// say so; nothing is created.
    #[tokio::test]
    async fn a_stopped_original_is_refused_before_anything_is_created() {
        let dir = tmpdir("stopped-src");
        let cfg = keyed_cfg(&dir.0, "");
        let f = fleet(); // `create_pod` panics: reaching a create fails the test
        f.pods.lock().unwrap()[0].status = "EXITED".into();
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let replace = |dry_run| {
            let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run, yes: true };
            ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None }
        };
        let e = replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), replace(false))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("devtest-alpha is stopped (EXITED)") && e.contains("`arena pods start devtest-alpha`") && e.contains("nothing was created"), "{e}");
        take_would_refuse();
        replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), replace(true)).await.unwrap();
        assert!(take_would_refuse().iter().any(|r| r.contains("devtest-alpha is stopped")));
        // migrate copy: the same, whether -new would be created or re-synced.
        let pods = f.pods.lock().unwrap().clone();
        let copy = |dry_run| MigrateCopyJob { setup_timeouts: timeouts, ov: &ov, place: &place, dry_run, yes: true, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
        let e = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, copy(false)).await.unwrap_err().to_string();
        assert!(e.contains("devtest-alpha is stopped (EXITED)"), "{e}");
        migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, copy(true)).await.unwrap();
        assert!(take_would_refuse().iter().any(|r| r.contains("devtest-alpha is stopped")));
        let mut with_new = pods.clone();
        with_new.push(pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))));
        let remote = Arc::new(FakeRemote::new());
        let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &with_new, copy(false)).await.unwrap_err().to_string();
        assert!(e.contains("devtest-alpha is stopped (EXITED)"), "{e}");
        assert!(f.calls().is_empty() && remote.calls().is_empty(), "{:?} {:?}", f.calls(), remote.calls());
    }

    /// Review finding: after a revert the summary said `arena pods pull <new>` while pull's
    /// positional argument was the backup LABEL — that pulled EVERY pod's home into a folder
    /// named after the new pod, onto the disk the live cohort shares. Positionals are pods now
    /// (`--label` takes the label; `-t` is the hidden old spelling), and the hint names the pod
    /// that way: it resolves to that pod alone, with no label read from it.
    #[test]
    fn the_revert_followup_backs_up_only_the_new_pod() {
        let line = revert_followup("devtest-alpha", "devtest-alpha-new");
        assert!(line.contains("`arena pods pull devtest-alpha-new`"), "{line}");
        let PodCmd::Pull { label, sel, target_flag, .. } = pods_cmd(&["pull", "devtest-alpha-new"]) else { panic!("not a pull") };
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
        let naming = arena_core::selector::Naming::from_config(&cfg);
        let args = crate::pull_label_and_targets(label, &sel.targets, &target_flag, &naming, &[]).unwrap();
        assert_eq!(args, crate::pods::pull::PullArgs { label: None, targets: vec!["devtest-alpha-new".to_string()], note: None });
    }

    /// Review finding: a revert wrote `~/.name` through the listed endpoint of a STOPPED
    /// original (after a `--no-final-sync` cutover) — whatever pod answers there would be
    /// labelled as this machine. A stopped pod isn't written, and every write runs only on
    /// the pod it names (RUNPOD_POD_ID, in the same session).
    #[tokio::test(start_paused = true)]
    async fn a_revert_never_writes_through_a_stopped_originals_stale_endpoint() {
        let dir = tmpdir("revert-stopped");
        let cfg = cfg(&dir.0);
        let mut old = pod("r-alpha", "devtest-alpha-old", "runpod", Some(("10.0.0.1", 22001)));
        old.status = "EXITED".into();
        let f = Fleet::new(vec![old, pod("r-alpha-new", "devtest-alpha", "runpod", Some(("10.0.0.2", 22001)))]);
        let remote = FakeRemote::new();
        cutover_revert(&remote, &cfg, &f, &f, "devtest-alpha", "r-alpha-new", "r-alpha", false).await;
        assert_eq!(f.calls(), ["rename r-alpha-new devtest-alpha-new", "rename r-alpha devtest-alpha"]);
        assert!(remote.calls_to("10.0.0.1:22001").is_empty(), "{:?}", remote.calls());
        let on_new = remote.calls_to("10.0.0.2:22001");
        assert!(
            matches!(&on_new[..], [RemoteCall::Exec { cmd, .. }] if cmd.ends_with(&name_file_command("alpha-new")) && cmd.contains("= 'r-alpha-new' ]")),
            "{on_new:?}"
        );
    }

    /// Review finding: `come_back` waited for each pod to settle, then `settled_sync` waited
    /// for ALL of them again, and once more before its re-sync — a pod that never got an
    /// endpoint back cost 3 × 600 s before `start`/`restart`/`reimage` reported it. One wait.
    #[tokio::test(start_paused = true)]
    async fn a_pod_that_never_comes_back_costs_one_settle_wait() {
        let dir = tmpdir("start-dead");
        let cfg = cfg(&dir.0);
        let mut f = fleet();
        f.pods.lock().unwrap()[0].status = "EXITED".into();
        f.blank_lists = (1..10_000).collect(); // never an endpoint again
        let pods = f.pods.lock().unwrap().clone();
        let settle = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };
        let start = tokio::time::Instant::now();
        let opts = RestartOpts { yes: true, ..Default::default() };
        let e = handle_start(&f, Arc::new(FakeRemote::new()), &cfg, &named(&pods[..1]), opts, settle, &dir.0.display().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("NOT set up again") && e.contains("never stabilized within 600s"), "{e}");
        let took = start.elapsed();
        assert!(took >= Duration::from_secs(600) && took < Duration::from_secs(700), "one wait, not three: {took:?}");
    }

    /// Lane P × lane R: `up` calls a pod that stays up behind RunPod's SSH proxy only (no public
    /// IP) a host failure after its 5-min grace. `replace`'s new pod used to wait its whole
    /// 900 s window for an endpoint such a host never gives, then say only "timed out". It
    /// now gives up at the grace, naming the host's problem, with `-new` left in place and
    /// nothing renamed or copied.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_behind_the_ssh_proxy_only_is_given_up_on_after_ups_grace() {
        let dir = tmpdir("replace-proxy-only");
        let cfg = keyed_cfg(&dir.0, "");
        let f = fleet();
        let mut new = pod("r-new", "", "runpod", None);
        new.proxy_only = true;
        *f.to_create.lock().unwrap() = vec![new];
        let remote = Arc::new(FakeRemote::new());
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run: false, yes: true };
        let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
        let start = tokio::time::Instant::now();
        let e = replace_on(remote.clone(), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap_err();
        let e = format!("{e:#}");
        let took = start.elapsed();
        assert!(e.contains("devtest-alpha-new never came up") && e.contains(arena_core::fleet::PROXY_ONLY) && e.contains("no public IP"), "{e}");
        assert!(took >= Duration::from_secs(300) && took < Duration::from_secs(400), "up's grace, not the 900 s window: {took:?}");
        assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000"], "nothing renamed, nothing terminated");
        assert!(remote.calls().is_empty(), "nothing copied: {:?}", remote.calls());
    }

    /// The same for the settle of `start`/`restart`/`reimage`: a pod that comes back behind the
    /// SSH proxy only is named as such after the grace, not after the whole settle window as a
    /// bare "never stabilized". A listing that lacks the pod meanwhile doesn't restart the clock.
    #[tokio::test(start_paused = true)]
    async fn a_pod_started_behind_the_ssh_proxy_only_is_named_after_ups_grace() {
        let dir = tmpdir("start-proxy-only");
        let cfg = cfg(&dir.0);
        let f = fleet();
        {
            let mut pods = f.pods.lock().unwrap();
            pods[0].status = "EXITED".into();
            (pods[0].ssh_ip, pods[0].ssh_port, pods[0].proxy_only) = (None, None, true);
        }
        let pods = f.pods.lock().unwrap().clone();
        let settle = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };
        let gap = async {
            // A listing gap two minutes in (the aggregate list drops a failed backend's pods).
            tokio::time::sleep(Duration::from_secs(120)).await;
            let gone = f.pods.lock().unwrap().remove(0);
            tokio::time::sleep(Duration::from_secs(40)).await;
            f.pods.lock().unwrap().insert(0, gone);
        };
        let start = tokio::time::Instant::now();
        let opts = RestartOpts { yes: true, ..Default::default() };
        let (sel, keys_dir) = (named(&pods[..1]), dir.0.display().to_string());
        let (res, ()) = tokio::join!(handle_start(&f, Arc::new(FakeRemote::new()), &cfg, &sel, opts, settle, &keys_dir), gap);
        let e = res.unwrap_err().to_string();
        assert!(e.contains("NOT set up again") && e.contains(arena_core::fleet::PROXY_ONLY) && e.contains("no public IP"), "{e}");
        let took = start.elapsed();
        assert!(took >= Duration::from_secs(300) && took < Duration::from_secs(400), "the grace, through the gap: {took:?}");
    }

    /// The endpoint-flap fix's second half (review: no test held it): when the listing the
    /// first post-restart sync takes shows the pod without an endpoint, the sync doesn't
    /// route it — it waits for that pod again and syncs once more.
    #[tokio::test]
    async fn a_post_restart_sync_that_missed_the_pod_waits_and_syncs_again() {
        let dir = tmpdir("restart-resync");
        let cfg = cfg(&dir.0);
        let mut f = fleet();
        // List calls: 1 the restart's lookup, 2-3 the settle wait, 4 setup's listing, 5-6 the
        // sync's settle wait — so 7 is the listing the first sync routes from.
        f.blank_lists = vec![7];
        handle_restart(&f, Arc::new(FakeRemote::new()), &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string()).await.unwrap();
        assert!(*f.lists.lock().unwrap() >= 10, "a second wait and sync: {}", f.lists.lock().unwrap());
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-alpha") && px.contains("23001"), "{px}");
    }

    /// Review finding (untested branch): `pods start` of a powered-off VM — its disk kept, so
    /// no setup — still syncs the proxy to it.
    #[tokio::test]
    async fn starting_a_powered_off_vm_syncs_the_proxy_without_setup() {
        let dir = tmpdir("start-vm");
        let cfg = cfg(&dir.0);
        let f = fleet();
        f.pods.lock().unwrap()[3].status = "off".into(); // delta: a Hetzner VM
        let pods = f.pods.lock().unwrap().clone();
        let remote = Arc::new(FakeRemote::new());
        let opts = RestartOpts { yes: true, ..Default::default() };
        handle_start(&f, remote.clone(), &cfg, &named(&pods[3..4]), opts, FAST, &dir.0.display().to_string()).await.unwrap();
        assert_eq!(f.calls(), ["start h-delta"]);
        assert!(remote.calls().is_empty(), "its disk was kept: no setup");
        let px = proxy_file(&dir.0);
        assert!(px.contains("name=devtest-delta") && px.contains("1022"), "{px}");
    }

    /// Review finding (untested branch): the cutover reverts when the proxy sync went through
    /// but didn't route the name to the new pod (listed without an endpoint at that moment) —
    /// nobody could reach it.
    #[tokio::test(start_paused = true)]
    async fn a_cutover_whose_sync_doesnt_route_the_new_pod_reverts() {
        let dir = tmpdir("cutover-unrouted");
        let cfg = cfg(&dir.0);
        let owner = Fleet::new(vec![
            pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
            pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))),
        ]);
        // What the proxy sync lists right after the swap: the promoted pod, without an endpoint.
        let seen = Fleet::new(vec![
            pod("r-alpha", "devtest-alpha-old", "runpod", Some(("10.0.0.1", 22001))),
            pod("r-alpha-new", "devtest-alpha", "runpod", None),
        ]);
        let remote = Arc::new(FakeRemote::new());
        remote.on("10.0.0.2:22001", "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
        let pods = owner.pods.lock().unwrap().clone();
        let opts = CutoverOpts { skip_proxy: false, no_final_sync: true, dry_run: false, yes: true };
        let e = migrate_cutover_on(remote, &seen, &cfg, &owner, "devtest-alpha", &pods, opts, &dir.0.display().to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("cutover reverted") && e.contains("doesn't route devtest-alpha to the new pod"), "{e}");
        assert_eq!(
            owner.calls(),
            ["rename r-alpha devtest-alpha-old", "rename r-alpha-new devtest-alpha", "rename r-alpha-new devtest-alpha-new", "rename r-alpha devtest-alpha"]
        );
    }

    /// Review finding: after a `--no-final-sync` cutover, `migrate finish` terminated a STOPPED
    /// `-old` — and with it its volume, holding what changed on it after the last sync. It's
    /// kept unless `--discard-stopped`, saying what it may hold and since when; with the flag
    /// it goes, and its leftover local staging copy with it (that removal was untested too).
    #[tokio::test]
    async fn finish_keeps_a_stopped_old_unless_told_to_discard_it() {
        let dir = tmpdir("finish-stopped");
        let cfg = cfg(&dir.0);
        let old_id: &'static str = Box::leak(format!("r-finish-{}", std::process::id()).into_boxed_str());
        // (removed by the guard even if an assertion fails first — see the replace test)
        let stage = TmpDir(staging_dir(&std::env::temp_dir(), old_id));
        let mut old = pod(old_id, "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002)));
        old.status = "EXITED".into();
        let mut f = Fleet::new(vec![pod("r-bravo-new", "devtest-bravo", "runpod", Some(("10.0.0.3", 22009))), old]);
        f.volumes = vec![(old_id, 20)];
        let remote = FakeRemote::new();
        remote.on("10.0.0.3:22009", "arena-last-sync", FakeReply::stdout("arena-last-sync=1791457323\n"));
        let listed = f.pods.lock().unwrap().clone();
        let e = migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { yes: true, ..Default::default() })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("NOT terminating devtest-bravo-old: it is stopped (EXITED)") && e.contains("its 20 GB volume"), "{e}");
        assert!(e.contains("2026-10-08 11:02 UTC") && e.contains("--discard-stopped") && e.contains("`arena pods pull devtest-bravo-old`"), "{e}");
        assert!(f.calls().is_empty());
        take_would_refuse();
        migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: true, yes: true, ..Default::default() }).await.unwrap();
        assert!(take_would_refuse().iter().any(|r| r.contains("it is stopped")));
        // --discard-stopped: it goes, and the leftover staging copy of its home with it.
        std::fs::create_dir_all(stage.0.join("ARENA_materials")).unwrap();
        std::fs::write(stage.0.join("ARENA_materials/work.py"), "a participant's home").unwrap();
        let opts = FinishOpts { discard_stopped: true, yes: true, ..Default::default() };
        migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, opts).await.unwrap();
        assert_eq!(f.calls(), [format!("terminate {old_id}")]);
        assert!(!stage.0.exists(), "the leftover staging copy is removed once its source is gone");
        // The refusal's words for the volume it may (not) have.
        assert!(stopped_old_refusal("o", "EXITED", "c", Some(0), "t").contains("--discard-stopped is safe"));
        assert!(stopped_old_refusal("o", "EXITED", "c", None, "t").contains("couldn't read whether it has a volume"));
    }

    #[test]
    fn the_new_flags_parse() {
        assert!(matches!(
            pods_cmd(&["migrate", "finish", "alpha", "--discard-stopped"]),
            PodCmd::Migrate { cmd: MigrateCmd::Finish { discard_stopped: true, .. } }
        ));
        assert!(matches!(pods_cmd(&["migrate", "finish", "alpha"]), PodCmd::Migrate { cmd: MigrateCmd::Finish { discard_stopped: false, .. } }));
        assert!(matches!(
            pods_cmd(&["restart", "alpha", "--wipe-ok", "--no-setup", "--skip-proxy"]),
            PodCmd::Restart { wipe_ok: true, no_setup: true, skip_proxy: true, dry_run: false, .. }
        ));
        assert!(matches!(pods_cmd(&["restart", "alpha"]), PodCmd::Restart { wipe_ok: false, no_setup: false, .. }));
        assert!(matches!(pods_cmd(&["stop", "alpha", "--wipe-ok"]), PodCmd::Stop { wipe_ok: true, .. }));
        assert!(matches!(pods_cmd(&["terminate", "alpha", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: false, .. }));
        assert!(matches!(pods_cmd(&["terminate", "--all", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: true, .. }));
        assert!(matches!(pods_cmd(&["terminate", "--all", "--unlock"]), PodCmd::Terminate { unlock: true, all: true, .. }));
        assert!(matches!(pods_cmd(&["terminate", "alpha"]), PodCmd::Terminate { unlock: false, .. }));
        assert!(matches!(pods_cmd(&["lock", "alpha", "--dry-run"]), PodCmd::Lock { dry_run: true, .. }));
        assert!(matches!(pods_cmd(&["unlock", "--all"]), PodCmd::Unlock { dry_run: false, .. }));
    }

    /// The recorded refusal of a locked pod (`locked_stop_refused.json`, live 2026-10-08),
    /// replayed through the v2 backend's own error classification — exactly what the real
    /// backend returns for it (`Locked`).
    fn recorded_locked(ctx: &str) -> Error {
        let e = arena_core::provider::runpod_v2::recorded::locked_refusal(ctx);
        assert!(e.is_locked() && e.to_string().contains("HTTP 400") && e.to_string().contains("Pod is locked"), "{e:?}");
        e
    }

    /// The fleet with alpha locked (as the v2 listing reports it).
    fn locked_fleet() -> Fleet {
        let f = fleet();
        let mut pods = f.pods.lock().unwrap().clone();
        pods[0].locked = Some(true);
        for p in pods.iter_mut().skip(1).filter(|p| p.provider == "runpod") {
            p.locked = Some(false);
        }
        Fleet::new(pods)
    }

    /// Every lifecycle command refuses a pod the listing reports locked — up front, the
    /// whole command, before any call (a batch never half-runs) — with the one sentence that
    /// says how to unlock it. A dry run says it would be refused, and does nothing.
    #[tokio::test]
    async fn every_mutating_command_refuses_a_locked_pod_up_front() {
        let dir = tmpdir("locked-refusals");
        let cfg = cfg(&dir.0);
        let hint = "devtest-alpha is locked — `arena pods unlock devtest-alpha` first";
        // (argv, the refusal)
        let cases: &[(&[&str], String)] = &[
            (&["stop", "alpha", "--wipe-ok"], format!("refusing to stop: {hint}")),
            (&["stop", "--all", "--wipe-ok"], format!("refusing to stop: {hint}")),
            (&["restart", "alpha", "--wipe-ok"], format!("refusing to restart: {hint}")),
            (&["reimage", "alpha", "bravo"], format!("refusing to reimage: {hint}")),
            (&["terminate", "alpha"], format!("refusing to terminate: {hint} — or pass --unlock to lift the lock as part of the terminate")),
            (&["terminate", "--all"], format!("refusing to terminate: {hint} — or pass --unlock")),
        ];
        for (argv, want) in cases {
            let f = locked_fleet();
            let e = handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap_err().to_string();
            assert!(e.contains(want.as_str()), "{argv:?}: `{want}` in {e}");
            assert!(f.calls().is_empty(), "{argv:?}: nothing touched: {:?}", f.calls());
            // The dry run says so and changes nothing. (Reimage's needs an image and keys
            // from config first; its refusal comes before those, as checked above.)
            if argv[0] == "reimage" {
                continue;
            }
            let mut dry = argv.to_vec();
            dry.push("--dry-run");
            take_would_refuse();
            handle_pods(pods_cmd(&dry), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap();
            assert!(f.calls().is_empty(), "{dry:?}");
            let notes = take_would_refuse();
            assert!(
                notes.len() == 1 && notes[0].starts_with("(without --dry-run: ") && notes[0].contains(want.as_str()),
                "{dry:?}: the dry run says it would be refused: {notes:?}"
            );
        }
        // Unlocked pods next to it go ahead as before.
        let f = locked_fleet();
        handle_pods(pods_cmd(&["stop", "bravo", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap();
        assert_eq!(f.calls(), ["stop r-bravo"]);
    }

    /// `replace` (with and without `--keep-old`), `migrate cutover` of a locked original and
    /// `migrate finish` of a locked `-old`: refused up front with the unlock sentence (plus,
    /// for the two that retire the pod for a new one, that the new one isn't locked by
    /// itself) — nothing created, renamed or terminated. The dry run prints the plan and
    /// says it would be refused, touching nothing either.
    #[tokio::test]
    async fn replace_and_migrate_refuse_a_locked_pod() {
        let dir = tmpdir("locked-replace");
        // A GPU type for the replacement spec (the fake's pod_spec reports none).
        let cfg = Config::parse(&format!("{}GPU_TYPE=\"NVIDIA RTX A4000\"\n", cfg_text(&dir.0)));
        let hint = |name: &str| format!("{name} is locked — `arena pods unlock {name}` first");
        /// `pods replace <src>` on `f`, `-y`, no proxy.
        async fn replace(f: &Fleet, cfg: &Config, src: &str, keep_old: bool, dry_run: bool) -> anyhow::Result<()> {
            let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None).unwrap();
            let opts = ReplaceOpts { keep_old, skip_proxy: true, dry_run, yes: true };
            let id = f.pods.lock().unwrap().iter().find(|p| p.name == src).unwrap().id.clone();
            let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
            let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
            replace_on(Arc::new(FakeRemote::new()), f, cfg, f, id, src.to_string(), job).await
        }
        let want = format!("refusing to replace: {}{LOCKED_REPLACEMENT_NOTE}", hint("devtest-alpha"));
        for keep_old in [false, true] {
            // Fleet::create_pod panics: reaching a create fails the test.
            let f = locked_fleet();
            let e = replace(&f, &cfg, "devtest-alpha", keep_old, false).await.unwrap_err().to_string();
            assert_eq!(e, want, "--keep-old={keep_old}");
            assert!(f.calls().is_empty(), "--keep-old={keep_old}: {:?}", f.calls());
            take_would_refuse();
            replace(&f, &cfg, "devtest-alpha", keep_old, true).await.unwrap();
            assert!(f.calls().is_empty(), "--keep-old={keep_old} (dry run): {:?}", f.calls());
            assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")], "--keep-old={keep_old}");
        }
        // An unlocked source gets past the lock check (to the plan; dry run: nothing else).
        let f = locked_fleet();
        replace(&f, &cfg, "devtest-bravo", false, true).await.unwrap();
        assert!(take_would_refuse().is_empty() && f.calls().is_empty());

        // migrate cutover: the original (alpha) locked, its -new built.
        let f = locked_fleet();
        f.pods.lock().unwrap().push(pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))));
        let pods = f.pods.lock().unwrap().clone();
        let want = format!("refusing to cut over: {}{LOCKED_REPLACEMENT_NOTE}", hint("devtest-alpha"));
        let opts = |dry_run| CutoverOpts { skip_proxy: true, no_final_sync: false, dry_run, yes: true };
        let e = migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), "/nonexistent")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, want);
        migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-alpha", &pods, opts(true), "/nonexistent").await.unwrap();
        assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")]);
        assert!(f.calls().is_empty(), "{:?}", f.calls());

        // migrate finish: the parked -old locked (the live pod isn't).
        let mut pods = fleet().pods.lock().unwrap().clone();
        let mut old = pod("r-bravo-old", "devtest-bravo-old", "runpod", None);
        old.locked = Some(true);
        pods.push(old);
        let f = Fleet::new(pods.clone());
        let want = format!("refusing to terminate: {}", hint("devtest-bravo-old"));
        let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &pods, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
        assert_eq!(e, want);
        migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &pods, FinishOpts { dry_run: true, yes: true, ..Default::default() }).await.unwrap();
        assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")]);
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        // Locked since it was listed: the API's refusal reads as the same sentence.
        let mut unlisted = pods.clone();
        unlisted.last_mut().unwrap().locked = None;
        let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &unlisted, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
        assert_eq!(e, format!("terminating devtest-bravo-old: {}", hint("devtest-bravo-old")));
        assert!(f.calls().is_empty());
        // Unlocked: it goes.
        f.server_locked.lock().unwrap().clear();
        migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &unlisted, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-bravo-old"]);
    }

    /// disk-nits (review #42): a `CONTROL_MIN_FREE_GB` the via-local copy can't read refuses
    /// replace, migrate copy and a cutover's final sync before anything is created or asked —
    /// found only at the copy, it left the new pod built, set up and billing. Its dry runs say
    /// they would be refused; a cutover without a final sync copies nothing, so it doesn't care.
    #[tokio::test]
    async fn replace_and_migrate_refuse_a_bad_floor_before_creating_anything() {
        let dir = tmpdir("bad-floor");
        let cfg = Config::parse(&format!("{}GPU_TYPE=\"NVIDIA RTX A4000\"\nCONTROL_MIN_FREE_GB=2.5\n", cfg_text(&dir.0)));
        let timeouts = || arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let want = |what: &str| {
            format!(
                "CONTROL_MIN_FREE_GB must be a whole number of GB, 0..=100000 (got `2.5`) — not starting the {what}: its copy via \
                 local staging needs it (fix it, or unset it for the default)"
            )
        };
        for dry_run in [false, true] {
            // Fleet::create_pod panics: reaching a create fails the test.
            let f = locked_fleet();
            let id = f.pods.lock().unwrap().iter().find(|p| p.name == "devtest-bravo").unwrap().id.clone();
            let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run, yes: true };
            let job = ReplaceJob { setup_timeouts: timeouts(), ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
            take_would_refuse();
            let r = replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, id, "devtest-bravo".into(), job).await;
            if dry_run {
                r.unwrap();
                assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("replace"))]);
            } else {
                assert_eq!(r.unwrap_err().to_string(), want("replace"));
            }
            assert!(f.calls().is_empty(), "dry run {dry_run}: {:?}", f.calls());

            let pods = f.pods.lock().unwrap().clone();
            let job = MigrateCopyJob { setup_timeouts: timeouts(), ov: &ov, place: &place, dry_run, yes: true, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
            let r = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-bravo", &pods, job).await;
            if dry_run {
                r.unwrap();
                assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("migrate"))]);
            } else {
                assert_eq!(r.unwrap_err().to_string(), want("migrate"));
            }
            assert!(f.calls().is_empty(), "dry run {dry_run}: {:?}", f.calls());
        }
        // A cutover's final sync: refused alike; without one, nothing to copy — not refused.
        let f = locked_fleet();
        f.pods.lock().unwrap().push(pod("r-bravo-new", "devtest-bravo-new", "runpod", Some(("10.0.0.2", 22001))));
        let pods = f.pods.lock().unwrap().clone();
        let cutover = |no_final_sync, dry_run| CutoverOpts { skip_proxy: true, no_final_sync, dry_run, yes: true };
        let e = migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(false, false), "/nonexistent")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, want("cut over"));
        migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(false, true), "/nonexistent").await.unwrap();
        assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("cut over"))]);
        migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(true, true), "/nonexistent").await.unwrap();
        assert!(take_would_refuse().is_empty(), "no final sync: no copy, no floor needed");
        assert!(f.calls().is_empty(), "{:?}", f.calls());
    }

    /// A pod locked since it was listed (or on a backend whose listing doesn't say): the
    /// API's recorded 400 reads as the same sentence — for restart, terminate and rename.
    #[tokio::test]
    async fn the_apis_locked_refusal_reads_as_the_unlock_hint() {
        let dir = tmpdir("locked-api");
        let cfg = cfg(&dir.0);
        let hint = "devtest-bravo is locked — `arena pods unlock devtest-bravo` first";
        let f = fleet();
        f.server_locked.lock().unwrap().push("r-bravo".into());
        let e = handle_pods(pods_cmd(&["terminate", "bravo", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(hint), "{e}");
        let restart = handle_pods(
            pods_cmd(&["restart", "bravo", "--wipe-ok", "--no-setup", "--skip-proxy"]),
            &f,
            Arc::new(FakeRemote::new()),
            &cfg,
            true,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(restart.contains(hint), "{restart}");
        let e = handle_pods(pods_cmd(&["stop", "bravo", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("1 pod(s) failed to stop"), "{e}");
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        // Rename: allowed on a locked pod by the spec's wording (assumed — see `lock`), so
        // it isn't refused up front; if the API refuses it after all, it reads the same.
        let f = locked_fleet();
        let book = no_api(&dir.0.join("none.csv"));
        handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, one("alpha", "echo"), true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
        let mut f = locked_fleet();
        f.rename_locked = vec!["r-bravo"];
        let e = handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, one("bravo", "echo"), true, false, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(hint), "{e}");
    }

    /// `terminate --unlock` (the end-of-program `--all --unlock`): each locked pod is
    /// unlocked right before its own terminate, the others just terminated; one whose unlock
    /// fails is NOT terminated. Never without the flag (see the refusals above).
    #[tokio::test]
    async fn terminate_unlock_lifts_each_lock_right_before_its_terminate() {
        let dir = tmpdir("terminate-unlock");
        let cfg = cfg(&dir.0);
        let f = locked_fleet();
        handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap();
        assert_eq!(
            f.calls(),
            ["lock r-alpha false", "terminate r-alpha", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"]
        );
        // A failed unlock: that pod stays (locked), and the command says so.
        let mut f = locked_fleet();
        f.fail_unlock = vec!["r-alpha"];
        let e = handle_pods(pods_cmd(&["terminate", "alpha", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("devtest-alpha: unlocking failed, so it was NOT terminated"), "{e}");
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        // --unlock on a pod that isn't locked: just the terminate.
        let f = locked_fleet();
        handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap();
        assert_eq!(f.calls(), ["terminate r-bravo"]);
    }

    /// The fleet with alpha locked, plus two locked boxes that aren't the cohort's on the same
    /// account: `solo-gpu` (the `@solo-gpu` staff entry) and `james-gpu` (off the list).
    fn fleet_with_locked_staff() -> Fleet {
        let mut pods = locked_fleet().pods.lock().unwrap().clone();
        for (id, name) in [("r-solo", "solo-gpu"), ("r-james", "james-gpu")] {
            let mut p = pod(id, name, "runpod", Some(("10.0.0.1", 22009)));
            p.locked = Some(true);
            pods.push(p);
        }
        Fleet::new(pods)
    }

    /// `--all --unlock` (the end-of-program one-liner) lifts only the cohort's locks: a locked
    /// staff box and a locked pod off the list are KEPT — never unlocked, never terminated —
    /// the cohort goes, and the exit fails naming them. Said up front in the dry run. Named
    /// alone, a staff box is the operator's explicit choice: unlocked and terminated.
    #[tokio::test]
    async fn terminate_all_unlock_never_unlocks_a_box_that_isnt_the_cohorts() {
        let dir = tmpdir("terminate-unlock-staff");
        let cfg = cfg(&dir.0);
        let f = fleet_with_locked_staff();
        let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            f.calls(),
            ["lock r-alpha false", "terminate r-alpha", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"],
            "the staff box and the off-list pod: no unlock, no terminate"
        );
        assert!(
            e.contains("james-gpu, solo-gpu KEPT, not terminated: locked, and not this cohort's (`devtest-…`)")
                && e.contains("`arena pods terminate james-gpu --unlock`"),
            "{e}"
        );
        let left: Vec<String> = f.pods.lock().unwrap().iter().map(|p| p.name.clone()).collect();
        assert_eq!(left, ["solo-gpu", "james-gpu"]);
        assert_eq!(*f.server_locked.lock().unwrap(), ["r-solo", "r-james"], "still locked");

        // The dry run says so, and does nothing.
        let f = fleet_with_locked_staff();
        take_would_refuse();
        handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--dry-run"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap();
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        let notes = take_would_refuse();
        assert!(
            notes.len() == 1 && notes[0].contains("james-gpu, solo-gpu KEPT, not terminated") && notes[0].ends_with("the exit fails)"),
            "{notes:?}"
        );

        // Without --unlock the whole command is refused, and the refusal says --unlock
        // wouldn't take those two either.
        let e = handle_pods(pods_cmd(&["terminate", "--all"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to terminate: devtest-alpha, james-gpu, solo-gpu are locked"), "{e}");
        assert!(e.contains("(with --all it unlocks only the cohort's: james-gpu, solo-gpu would be KEPT, locked"), "{e}");
        assert!(f.calls().is_empty());

        // Only kept pods left: nothing is touched, and it fails.
        let f = fleet_with_locked_staff();
        f.pods.lock().unwrap().retain(|p| !p.name.starts_with("devtest-"));
        let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("KEPT, not terminated"), "{e}");
        assert!(f.calls().is_empty());

        // Named on its own: the operator's choice — unlocked, then terminated.
        for target in ["solo-gpu", "james-gpu"] {
            let f = fleet_with_locked_staff();
            handle_pods(pods_cmd(&["terminate", target, "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
                .await
                .unwrap();
            let id = if target == "solo-gpu" { "r-solo" } else { "r-james" };
            assert_eq!(f.calls(), [format!("lock {id} false"), format!("terminate {id}")]);
        }
    }

    /// A pod locked after the listing (it said unlocked): under `--unlock` the API's Locked
    /// refusal is the lock the operator asked to lift — unlocked, terminated on the one retry.
    /// Never for a box `--all --unlock` mustn't unlock, and never without the flag (the hint).
    #[tokio::test]
    async fn terminate_unlock_lifts_a_lock_taken_since_the_listing() {
        let dir = tmpdir("terminate-unlock-race");
        let cfg = cfg(&dir.0);
        let f = fleet();
        f.server_locked.lock().unwrap().push("r-bravo".into());
        handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap();
        assert_eq!(f.calls(), ["lock r-bravo false", "terminate r-bravo"]);

        // --all --unlock: the cohort pod is unlocked and goes; an off-list box locked since
        // the listing is NOT unlocked — it fails with the hint, the rest still go.
        let f = fleet();
        f.pods.lock().unwrap().push(pod("r-james", "james-gpu", "runpod", None));
        f.server_locked.lock().unwrap().extend(["r-bravo".to_string(), "r-james".to_string()]);
        let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("1 pod(s) failed to terminate"), "{e}");
        assert_eq!(
            f.calls(),
            ["terminate r-alpha", "lock r-bravo false", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"],
        );
        assert!(f.pods.lock().unwrap().iter().any(|p| p.name == "james-gpu"));

        // The retried unlock fails: not terminated, said so.
        let mut f = fleet();
        f.fail_unlock = vec!["r-bravo"];
        f.server_locked.lock().unwrap().push("r-bravo".into());
        let e = handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("devtest-bravo: locked since it was listed, and unlocking failed, so it was NOT terminated"), "{e}");
        assert!(f.calls().is_empty(), "{:?}", f.calls());
    }

    /// `pods lock`/`unlock`: the lockable pods are (un)locked, one already in that state is
    /// left alone, one on a backend that can't lock is reported and fails the exit. A dry
    /// run and an unscoped selection change nothing.
    #[tokio::test]
    async fn lock_and_unlock_act_on_what_can_be_locked_and_fail_on_the_rest() {
        let dir = tmpdir("lock-cmd");
        let cfg = cfg(&dir.0);
        let f = locked_fleet();
        let run = |argv: &'static [&'static str]| handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true);
        let e = run(&["lock", "alpha", "bravo", "delta"]).await.unwrap_err().to_string();
        assert!(e.contains("1 pod(s) can't be locked: devtest-delta"), "{e}");
        assert_eq!(f.calls(), ["lock r-bravo true"], "alpha already was; delta can't be");
        run(&["lock", "bravo"]).await.unwrap();
        assert_eq!(f.calls().len(), 1, "already locked: nothing sent");
        run(&["unlock", "bravo", "--dry-run"]).await.unwrap();
        assert_eq!(f.calls().len(), 1);
        run(&["unlock", "alpha", "bravo"]).await.unwrap();
        assert_eq!(f.calls(), ["lock r-bravo true", "lock r-alpha false", "lock r-bravo false"]);
        let e = run(&["lock", "delta"]).await.unwrap_err().to_string();
        assert!(e.contains("can't be locked: devtest-delta (leave other providers' pods out with --on runpod)"), "{e}");
        run(&["lock", "delta", "--dry-run"]).await.unwrap(); // says it would fail; changes nothing
        let e = run(&["lock"]).await.unwrap_err().to_string();
        assert!(e.contains("name the pods to lock"), "{e}");
        assert_eq!(f.calls().len(), 3);
        // The listing now says unlocked: a stop goes through again.
        run(&["stop", "alpha", "--wipe-ok"]).await.unwrap();
        assert_eq!(f.calls().last().unwrap(), "stop r-alpha");
    }
}
