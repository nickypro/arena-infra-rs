//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Mutating commands
//! **act by default but confirm first**: at a terminal they print what they'll do and
//! prompt `Proceed? [y/N]`; `--yes` skips the prompt; with no terminal they refuse
//! unless `--yes`. `--dry-run` previews without doing anything. This is deliberate —
//! the tool is developed against a live production account.

#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use anyhow::{Context, Result};

use arena_core::health::judge_deep_call;
#[cfg(test)]
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::sshconfig::Audience;
use arena_core::Config;
#[cfg(test)]
use arena_core::PodSpec;

#[cfg(all(test, unix))]
mod admin_key_tests;
mod archive;
mod backups;
mod claude;
mod disk;
mod fleet_files;
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

use cli::{ApiCmd, BalanceCmd, Cmd};
use config_cmd::{config_check, handle_config, hetzner_image_rows, ConfigCmd};
use cron::{handle_cron, CRONTAB};
use keys::{handle_keys, openrouter_client};
use plan::handle_plan;
use pods::handle_pods;
use pods::setup::unstage_hetzner_script;
use proxy::handle_proxy;
use snapshot::handle_snapshot;
use ssh_config::{handle_ssh_config, handle_ssh_config_admin, handle_ssh_config_install, handle_ssh_config_uninstall};
// Reached as `super::x` / `crate::x` by the modules that predate the split; new code imports
// from the module itself.
use cli::{Cli, Select, SelectOpts};
use config_cmd::note_invalid_text;
use cron::{arena_cron_lines, char_named, cron_unsafe_char, reap_cron_installed, resolve_week_day, CRON_PATH_CHARS};
use keys::{copy_keys_command, drop_key_row, fleet_ssh_for, key_holders, keys_targets, machine_key_names, rotate_key, write_openrouter_key, KeySources, COPY_KEYS_TIMEOUT, OPENROUTER_KEYS_LINK};
use pods::backup::{failure_why, BackupFailed, Unpushed, RSYNC};
use pods::create::{sizing_flags, spec_with_overrides, Made, SpecOverrides};
use pods::health::{health_cache_path, record_health};
use pods::list::{enrich_best_effort, ENRICH_TIMEOUT};
use pods::placement::{checked_gpu, live_gpu_catalog, plan_placement, v2_catalog, PRICE_TIMEOUT};
use pods::pull::{absolute_path, local_backup_dir, pull_roots, take_pod_locks, Pulled, POD_LOCK_WAIT};
use pods::selection::{say, Selected};
use pods::setup::{repair_pod_keys, stage_hetzner_script};
use pods::{each_pod, exec_each_pod, PodCall, BACKUP_TIMEOUT};
use proxy::{proxy_sync_now, sync_line, sync_proxy, sync_proxy_to_stderr, ProxySync};
use ssh_config::ssh_config_lines;
#[cfg(test)]
use cli::{PodCmd, ReapAtCreate};
#[cfg(test)]
use config_cmd::{admin_key_rows, bad_hosts_section, balance_section, config_report, hetzner_rows};
#[cfg(test)]
use cron::{env_assignment, reap_cron_line, CronKind, CRON_BEGIN, CRON_END};
#[cfg(test)]
use keys::{handle_copy_keys, keys_with, up_keys_stage, KeysCmd};
#[cfg(test)]
use pods::backup::{copy_unpushed, handle_backup, handle_full_backup};
#[cfg(test)]
use pods::create::image_flag;
#[cfg(test)]
use pods::list::list_with;
#[cfg(test)]
use pods::lock::lock_settles;
#[cfg(test)]
use pods::pull::{pull_locked, pull_pods, LockMode, PodLocks, PullRun};
#[cfg(test)]
use pods::run::handle_run;
#[cfg(test)]
use pods::selection::take_said;
#[cfg(test)]
use pods::setup::{handle_setup, HETZNER_SETUP};
#[cfg(test)]
use pods::{handle_pods_with, lifecycle_tests, remote_tests};
#[cfg(test)]
use proxy::proxy_deployable;
#[cfg(test)]
use ssh_config::{ssh_config_after, ssh_config_check_row_as};

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
    // `--config`, else ARENA_CONFIG, else ~/.config/arena/config.env, else the built-in default;
    // then the fleet's provider: `--provider`, else the config's PROVIDER, else runpod.
    let (config_path, config_source) = arena_core::config::find_config(cli.config.as_deref());
    let cfg = Config::load(&config_path).with_context(|| arena_core::config::load_context(&config_path, config_source))?;
    let (provider_name, provider_source) = arena_core::config::pick_provider(cli.provider.as_deref(), &cfg);
    let picked = config_cmd::Picked { config: config_path.clone(), config_source, provider: provider_name.clone(), provider_source };

    // `config check` must work even when a provider key is missing (that's what it's
    // for), so build the provider lazily — only for commands that actually talk to one.
    let provider = match cli.cmd {
        // The archive builds its own Hetzner client (its own token), and its cron path none.
        Cmd::Config(_) | Cmd::Cron(_) | Cmd::Tui | Cmd::Update { .. } | Cmd::Gpus { .. } | Cmd::Offers { .. } | Cmd::Api(_) | Cmd::Archive(_) | Cmd::Image(_) | Cmd::Backups { .. } => None,
        // Fleet-wide: every command spans all configured providers (create still targets
        // --provider). One configured backend behaves like that single provider.
        _ => Some(picked.fleet(&cfg)?),
    };
    // How every pod-SSH path reaches pods — one for the whole run: real ssh/scp here,
    // `FakeRemote` in tests. Each call carries its own budget (see the `*_TIMEOUT`s).
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::SshRemote);

    match cli.cmd {
        Cmd::Tui => launch_tui(&provider_name, &config_path),
        Cmd::Update { .. } => unreachable!("handled before the config is read"),
        Cmd::Plan(c) => handle_plan(c, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Config(ConfigCmd::Check) => config_check(&cfg, &provider_name, &[hetzner_image_rows(&cfg).await, disk::check_rows(&cfg)].concat()),
        Cmd::Config(c) => handle_config(c, &cfg, &picked),
        Cmd::Cron(c) => handle_cron(c, &cfg, &config_path, cli.yes, CRONTAB, std::env::var("HOME").ok().as_deref()).await,
        Cmd::Archive(c) => archive::handle_archive(c, &cfg, remote.as_ref(), cli.yes).await,
        Cmd::Backups { json } => backups::handle_backups(&cfg, &config_path, json, arena_core::backups::read_crontab()),
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
        Cmd::Claude(c) => claude::handle_claude(c, provider.unwrap().as_ref(), remote, &cfg, cli.yes).await,
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
        Cmd::Gpus { json } => offers::handle_gpus(&cfg, &provider_name, json).await,
        Cmd::Api(c) => handle_api(c, &cfg).await,
        Cmd::Offers { view, gpu, cloud, max_price, gpus, server_type, order, all, verbose, json } => {
            let flags = offers::Flags { gpu, cloud, max_price, gpus, server_type, order };
            let view = if all { Some("all") } else { view.as_deref() };
            offers::handle_offers(&cfg, provider_named.then_some(provider_name.as_str()), view, flags, json, verbose).await
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

/// Single-quote for safe inclusion in a remote `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {

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
}
