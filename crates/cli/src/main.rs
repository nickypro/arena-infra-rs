//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Mutating commands
//! **act by default but confirm first**: at a terminal they print what they'll do and
//! prompt `Proceed? [y/N]`; `--yes` skips the prompt; with no terminal they refuse
//! unless `--yes`. `--dry-run` previews without doing anything. This is deliberate —
//! the tool is developed against a live production account.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arena_core::provider::Provider;
use arena_core::{Config, PodSpec};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

#[derive(Parser)]
#[command(
    name = "arena",
    version,
    about = "Streamlined ARENA infra control plane",
    // Cisco-style shorthands: any unambiguous prefix works (`arena po l` == `pods
    // list`). Ambiguous prefixes (e.g. `p` for pods/proxy) error and ask you to
    // disambiguate. Applies at each level.
    infer_subcommands = true
)]
struct Cli {
    /// Path to config.env (defaults to the read-only prod copy).
    #[arg(long, default_value = DEFAULT_CONFIG, global = true)]
    config: PathBuf,

    /// Compute provider to target.
    #[arg(long, default_value = "runpod", global = true)]
    provider: String,

    /// Skip the interactive "Proceed? [y/N]" confirmation on mutating commands.
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Ask the operator to confirm a mutating action. `--yes` proceeds without asking. At
/// an interactive terminal, prompts y/N. With **no terminal** (cron/pipes) and no
/// `--yes`, it *refuses* — automation must opt in explicitly with `--yes`, so nothing
/// mutates non-interactively by accident.
fn confirm(assume_yes: bool, what: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("{what}\nRefusing to proceed without a terminal — pass --yes to confirm non-interactively.");
        return Ok(false);
    }
    eprint!("{what}\nProceed? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[derive(Subcommand)]
enum Cmd {
    /// Machine (pod) lifecycle.
    #[command(subcommand, infer_subcommands = true)]
    Pods(PodCmd),
    /// Launch the interactive dashboard (arena-tui), inheriting --provider/--config.
    Tui,
    /// Plan port-forwarding/proxy wiring (read-only; prints config to apply).
    #[command(subcommand, infer_subcommands = true)]
    Proxy(ProxyCmd),
    /// View/preview a scheduled provisioning plan (arena-plan.json). Read-only for now;
    /// the timed executor + arming come next.
    #[command(subcommand, infer_subcommands = true)]
    Plan(PlanCmd),
    /// Inspect the loaded config.
    #[command(subcommand, infer_subcommands = true)]
    Config(ConfigCmd),
    /// Manage a cron schedule for `arena pods backup` (edits your crontab, touching only
    /// arena-managed lines).
    #[command(subcommand, infer_subcommands = true)]
    Cron(CronCmd),
}

#[derive(Subcommand)]
enum CronCmd {
    /// Install/replace the arena backup cron job.
    Install {
        /// Cron schedule expression (default: hourly).
        #[arg(long, default_value = "0 * * * *")]
        schedule: String,
        /// Bake `ARENA_START_DATE=YYYY-MM-DD` into the cron line, so the scheduled
        /// backup computes the right wNdM label without it being in config.env
        /// (a crontab line doesn't inherit your shell environment).
        #[arg(long)]
        start_date: Option<String>,
    },
    /// Remove the arena-managed cron lines.
    Remove,
    /// Show the currently-installed arena cron lines.
    Show,
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Validate the plan file; print the detected local time/timezone, the night
    /// window, caps, and every day entry with its resolved date.
    Check {
        #[arg(long, default_value = "arena-plan.json")]
        file: PathBuf,
    },
    /// Preview what the plan would do for a date (default: today): the GPU/provider
    /// fallback order, fill-to-target against the current fleet, and (for a replace
    /// day) what it would tear down. Read-only — never mutates.
    Show {
        #[arg(long, default_value = "arena-plan.json")]
        file: PathBuf,
        /// Date to preview (YYYY-MM-DD). Defaults to today (local).
        #[arg(long)]
        date: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate that the keys needed by the selected provider, proxy, and backup are
    /// present. Read-only; never prints secret values. Exits non-zero if a required
    /// key is missing.
    Check,
    /// Set a key in config.env (e.g. an API key): replaces the line if present, else
    /// appends `KEY="value"`. Writes to the --config file; never echoes the value. With
    /// no KEY/VALUE it prompts interactively (pick a key, type the value).
    Set {
        /// Config key, e.g. RUNPOD_API_KEY. Omit to choose interactively.
        key: Option<String>,
        /// Value to store (will be quoted). Omit to be prompted.
        value: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProxyCmd {
    /// Compute the proxy plan for current pods and print the nginx `stream` config
    /// (stable public port -> each pod's current SSH endpoint) to apply on the proxy
    /// host. Read-only: never connects to the proxy.
    Plan {
        /// Also write the rendered nginx config to this local path for review.
        /// (Local only — this never copies anything to the proxy host.)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Deploy the rendered nginx config to the proxy host and reload nginx
    /// (`nginx -t && nginx -s reload`). Acts by default; --dry-run to preview.
    Apply {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum PodCmd {
    /// List current pods (read-only).
    List {
        /// Emit JSON instead of a table (for scripting).
        #[arg(long)]
        json: bool,
        /// Fill the GPU column by querying `nvidia-smi` over SSH (the provider's list
        /// API omits GPU type). Slower — one SSH per pod.
        #[arg(long)]
        probe: bool,
    },
    /// Create N pods on the next free machine names. Requires -n/--count. GPU
    /// type/count and cloud default to config but can be overridden here.
    /// Acts by default; --dry-run to preview.
    Create {
        /// Target TOTAL number of pods — tops up to this many (mutually exclusive with -a).
        #[arg(short = 'n', long)]
        count: Option<usize>,
        /// Number of pods to ADD (mutually exclusive with -n).
        #[arg(short = 'a', long)]
        add: Option<usize>,
        /// GPU type, e.g. `A4000`, `3090`, `A100`, `A100 SXM` (or a full provider name).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod cloud tier: COMMUNITY or SECURE (overrides config).
        #[arg(long)]
        cloud: Option<String>,
        /// Container disk size in GB (overrides config DISK_GB).
        #[arg(long)]
        disk: Option<u32>,
        /// Persistent volume size in GB (overrides config VOLUME_GB; default 0).
        #[arg(long)]
        volume: Option<u32>,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
    },
    /// Create N pods, then poll until they have SSH endpoints and print the proxy
    /// plan — the one-command spin-up. Acts by default; --dry-run to preview. Polling stops at the
    /// timeout; it never runs in the background or mutates the proxy.
    Up {
        /// Target TOTAL number of pods — tops up to this many (mutually exclusive with -a).
        #[arg(short = 'n', long)]
        count: Option<usize>,
        /// Number of pods to ADD (mutually exclusive with -n).
        #[arg(short = 'a', long)]
        add: Option<usize>,
        /// GPU type, e.g. `A4000`, `3090`, `A100`, `A100 SXM` (or a full provider name).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod cloud tier: COMMUNITY or SECURE (overrides config).
        #[arg(long)]
        cloud: Option<String>,
        /// Container disk size in GB (overrides config DISK_GB).
        #[arg(long)]
        disk: Option<u32>,
        /// Persistent volume size in GB (overrides config VOLUME_GB; default 0).
        #[arg(long)]
        volume: Option<u32>,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Don't poll after creating; just print ids (run `proxy plan` later).
        #[arg(long)]
        no_wait: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
        /// After endpoints are up, deploy the nginx config to the proxy and reload it.
        #[arg(long)]
        proxy: bool,
        /// After endpoints are up, provision the pods over SSH (like `arena setup`).
        #[arg(long)]
        setup: bool,
        /// Give up waiting for endpoints after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between readiness polls (one list call per poll, whole fleet).
        #[arg(long, default_value_t = 12)]
        interval: u64,
    },
    /// Stop a pod by name or id. Acts by default; --dry-run to preview.
    Stop {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Restart a pod in place by name or id. Acts by default; --dry-run to preview.
    Restart {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Terminate (delete) a pod by name or id, or the whole fleet with --all.
    /// Acts by default; --dry-run to preview.
    Terminate {
        /// Machine name (e.g. arena8-apple) or raw provider id. Omit with --all.
        target: Option<String>,
        /// Terminate every pod the provider reports (the whole fleet).
        #[arg(long)]
        all: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Commit + push each pod's ARENA working tree to its autocommit branch over SSH.
    /// Branch is autocommit-{prefix}-w{week}d{day}-{machine}. Acts by default; --dry-run to preview.
    Backup {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Commit message (default: the autocommit branch name per machine).
        #[arg(long)]
        message: Option<String>,
        /// Override the iteration week (default: computed from ARENA_START_DATE).
        #[arg(long)]
        week: Option<u32>,
        /// Override the day-within-week (default: computed from ARENA_START_DATE).
        #[arg(long)]
        day: Option<u32>,
    },
    /// Provision pods over SSH: copy the git deploy key, write ~/.name, point the
    /// repo at GitHub. Acts by default; --dry-run to preview.
    Setup {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Force-checkout the default branch and hard-reset (else stay on current).
        #[arg(long)]
        force: bool,
    },
    /// Gently switch a pod's ARENA checkout to a branch (fetch + checkout +
    /// fast-forward pull, no hard reset). One pod (name/id) or --all. Acts by default;
    /// --dry-run to preview.
    SetBranch {
        /// Branch to check out (e.g. main, or a feature branch).
        branch: String,
        /// Machine name or id. Omit with --all.
        target: Option<String>,
        /// Apply to every pod with an SSH endpoint.
        #[arg(long)]
        all: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}


/// Warn (on a GPU provider) when pods would be created with no persistent volume —
/// container disk is wiped on restart, so uncommitted work would be lost.
fn warn_no_volume(provider: &dyn Provider, spec: &PodSpec) {
    if spec.volume_gb == 0 && provider.name() != "hetzner" {
        eprintln!(
            "⚠ no persistent volume (VOLUME_GB=0): work is lost when a pod restarts. \
             Set VOLUME_GB to keep it."
        );
    }
}

/// Compute the next free machine names for `count` pods, warning if fewer are
/// available than requested. (Read-only: lists current pods to know what's taken.)
///
/// CRITICAL: this propagates a list failure instead of swallowing it. If we can't
/// confirm what already exists, we must NOT proceed — treating a failed list as "zero
/// pods" would create a duplicate of the entire fleet on the live account. Better to
/// abort with an error the operator can see.
/// How many pods to make: a TOTAL to reach (top up to N) or a number to ADD.
#[derive(Debug, Clone, Copy)]
enum Want {
    Total(usize),
    Add(usize),
}

/// Resolve the `-n`(total) / `-a`(add) flags into a `Want`. Exactly one is required.
fn resolve_want(count: Option<usize>, add: Option<usize>) -> Result<Want> {
    match (count, add) {
        (Some(n), None) => Ok(Want::Total(n)),
        (None, Some(a)) => Ok(Want::Add(a)),
        (Some(_), Some(_)) => anyhow::bail!("pass either -n/--count (total) or -a/--add, not both"),
        (None, None) => anyhow::bail!("pass -n/--count <total> or -a/--add <count>"),
    }
}

async fn plan_names(provider: &dyn Provider, cfg: &Config, want: Want) -> Result<Vec<String>> {
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context(
            "listing existing pods (refusing to allocate names — a failed list could \
             create duplicate pods)",
        )?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    // `-n` is a target total: create only enough to top up to it. `-a` adds outright.
    let to_create = match want {
        Want::Add(a) => a,
        Want::Total(n) => {
            let have = existing.iter().filter(|p| p.name.starts_with(&format!("{prefix}-"))).count();
            if n <= have {
                eprintln!("already have {have} pod(s) (target {n}) — nothing to create");
                return Ok(Vec::new());
            }
            n - have
        }
    };
    let names = arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, to_create);
    if names.len() < to_create {
        eprintln!(
            "warning: need {to_create} but only {} free machine name(s) available",
            names.len()
        );
    }
    Ok(names)
}

/// Seconds to wait between capacity retries when `--keep-trying` is set.
const CAPACITY_RETRY_SECS: u64 = 30;

/// Max capacity retries per machine under `--keep-trying` (~1h at 30s) — generous
/// enough to "wait around" for a free GPU, bounded enough to never spin forever.
const MAX_CAPACITY_ATTEMPTS: u32 = 120;

/// Create one pod per name, returning those that came up. Error handling is typed:
/// - `Capacity` → the pool is exhausted; stop gracefully (or, with `keep_trying`,
///   wait `CAPACITY_RETRY_SECS` and retry that name) — the pods already created are
///   kept, never rolled back.
/// - `Auth` → credentials are wrong; abort immediately (retrying is pointless).
/// - anything else → report what we made so far, then surface the error.
/// Command-line overrides for the create spec, so you don't have to edit config.env
/// to spin up a different GPU/count/cloud for one run.
#[derive(Debug, Clone, Default)]
struct SpecOverrides {
    gpu: Option<String>,
    gpus: Option<u32>,
    cloud: Option<String>,
    disk: Option<u32>,
    volume: Option<u32>,
}

/// The base spec from config, with any command-line overrides applied.
fn spec_with_overrides(cfg: &Config, ov: &SpecOverrides) -> PodSpec {
    let mut spec = PodSpec::from_config(cfg);
    if let Some(g) = &ov.gpu {
        spec.gpu_type = arena_core::gpu::resolve(g);
    }
    if let Some(n) = ov.gpus {
        spec.gpu_count = n;
    }
    if let Some(c) = &ov.cloud {
        spec.cloud_type = c.to_uppercase();
    }
    if let Some(d) = ov.disk {
        spec.disk_gb = d;
    }
    if let Some(v) = ov.volume {
        spec.volume_gb = v;
    }
    spec
}

async fn create_pods(
    provider: &dyn Provider,
    cfg: &Config,
    names: &[String],
    keep_trying: bool,
    ov: &SpecOverrides,
) -> Result<Vec<arena_core::Pod>> {
    use arena_core::ProviderErrorKind as K;

    let base = spec_with_overrides(cfg, ov);
    let policy = arena_core::retry::RetryPolicy::default();
    let mut created = Vec::new();
    'names: for name in names {
        let mut spec = base.clone();
        spec.name = name.clone();
        spec.env.push(("MACHINE_NAME".into(), name.clone()));
        // Bound the capacity wait so a *misclassified* permanent error (e.g. a config
        // problem whose message merely looks capacity-ish) can't spin forever.
        let mut cap_attempts = 0u32;
        loop {
            // Retry transient/throttle failures with backoff; capacity & auth fall
            // through immediately to the classification below.
            match arena_core::retry::retrying(&policy, || provider.create_pod(&spec)).await {
                Ok(pod) => {
                    println!("[created] {} id={}", pod.name, pod.id);
                    created.push(pod);
                    continue 'names;
                }
                Err(e) => match e.kind() {
                    Some(K::Capacity) if keep_trying && cap_attempts < MAX_CAPACITY_ATTEMPTS => {
                        cap_attempts += 1;
                        eprintln!(
                            "[waiting] no capacity for {name}; retry {cap_attempts}/{MAX_CAPACITY_ATTEMPTS} in {CAPACITY_RETRY_SECS}s (have {}/{})",
                            created.len(),
                            names.len()
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(CAPACITY_RETRY_SECS)).await;
                        // loop: retry the same name
                    }
                    Some(K::Capacity) if keep_trying => {
                        eprintln!(
                            "[stop] still no capacity for {name} after {MAX_CAPACITY_ATTEMPTS} attempts (created {}/{}).",
                            created.len(),
                            names.len()
                        );
                        break 'names;
                    }
                    Some(K::Capacity) => {
                        eprintln!(
                            "[stop] no more capacity (created {}/{}). Re-run with --keep-trying to wait.",
                            created.len(),
                            names.len()
                        );
                        break 'names;
                    }
                    Some(K::Auth) => {
                        anyhow::bail!("authentication failed creating {name}: {e}");
                    }
                    _ => {
                        eprintln!("created {}/{} before failure", created.len(), names.len());
                        return Err(anyhow::anyhow!("creating {name}: {e}"));
                    }
                },
            }
        }
    }
    println!("\nCreated {} of {} requested.", created.len(), names.len());
    Ok(created)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;

    // `config check` must work even when a provider key is missing (that's what it's
    // for), so build the provider lazily — only for commands that actually talk to one.
    let provider = match cli.cmd {
        Cmd::Config(_) | Cmd::Cron(_) | Cmd::Tui => None,
        _ => Some(arena_core::provider::build(&cli.provider, &cfg)?),
    };

    match cli.cmd {
        Cmd::Tui => launch_tui(&cli.provider, &cli.config),
        Cmd::Plan(c) => handle_plan(c, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Config(c) => handle_config(c, &cfg, &cli.provider, &cli.config),
        Cmd::Cron(c) => handle_cron(c, &cli.config).await,
        Cmd::Pods(p) => handle_pods(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
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

async fn handle_setup(provider: &dyn Provider, cfg: &Config, apply: bool, force: bool) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let scfg = arena_core::setup::SetupConfig::from_config(cfg)?;
    let pods = provider.list_pods().await.context("listing pods for setup")?;
    let mut targets = Vec::new();
    for pod in &pods {
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to set up)");
        return Ok(());
    }

    if !apply {
        println!(
            "Dry-run — would provision {} pod(s) (copy key {} -> {}, then):\n",
            targets.len(),
            scfg.key_local,
            scfg.key_remote
        );
        for (name, target) in &targets {
            println!("# {name}");
            println!("{}", target.display_scp(&scfg.key_local, &scfg.key_remote));
            println!("{}\n", target.display_command(&scfg.remote_command(name, force)));
        }
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    let mut ok = 0;
    let mut failed = 0;
    for (name, target) in &targets {
        // 1) copy the key, 2) run the provisioning script.
        let copied = ssh::scp(target, &scfg.key_local, &scfg.key_remote).await;
        let result = match copied {
            Ok(out) if out.success => ssh::run(target, &scfg.remote_command(name, force)).await,
            Ok(out) => Err(arena_core::Error::provider(format!(
                "scp key failed: {}",
                out.stderr.trim()
            ))),
            Err(e) => Err(e),
        };
        match result {
            Ok(out) if out.success => {
                println!("[set up]  {name}");
                ok += 1;
            }
            Ok(out) => {
                eprintln!("[FAILED]  {name} (exit {:?}): {}", out.code, out.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                eprintln!("[FAILED]  {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} provisioned, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to set up");
    }
    Ok(())
}

/// Resolve the iteration (week, day): explicit `--week/--day` win; otherwise compute
/// from `ARENA_START_DATE` (the start date is w0d1) and today's date.
fn resolve_week_day(cfg: &Config, week: Option<u32>, day: Option<u32>) -> Result<(u32, u32)> {
    if let (Some(w), Some(d)) = (week, day) {
        return Ok((w, d));
    }
    let start = cfg
        .get("ARENA_START_DATE")
        .and_then(arena_core::schedule::parse_ymd)
        .context(
            "computing week/day needs ARENA_START_DATE=YYYY-MM-DD in config (or pass \
             --week and --day)",
        )?;
    let start_days = arena_core::schedule::days_from_civil(start.0, start.1, start.2);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today = arena_core::schedule::days_from_unix(now_secs);
    let (w, d) = arena_core::schedule::week_day(start_days, today);
    // Allow overriding just one of the two.
    Ok((week.unwrap_or(w), day.unwrap_or(d)))
}

const CRON_BEGIN: &str = "# >>> arena-infra-rs >>>";
const CRON_END: &str = "# <<< arena-infra-rs <<<";

/// Return `existing` crontab text with any arena-managed block removed.
fn strip_arena_block(existing: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        match line.trim() {
            CRON_BEGIN => in_block = true,
            CRON_END => in_block = false,
            _ if !in_block => out.push(line),
            _ => {}
        }
    }
    let mut s = out.join("\n");
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

/// Build new crontab text = existing (minus old arena block) + the new arena block
/// (empty `lines` => just remove). Other entries are preserved untouched.
fn with_arena_block(existing: &str, lines: &[String]) -> String {
    let base = strip_arena_block(existing);
    if lines.is_empty() {
        return if base.is_empty() { String::new() } else { format!("{base}\n") };
    }
    let mut s = String::new();
    if !base.is_empty() {
        s.push_str(&base);
        s.push('\n');
    }
    s.push_str(CRON_BEGIN);
    s.push('\n');
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(CRON_END);
    s.push('\n');
    s
}

async fn handle_cron(cmd: CronCmd, config_path: &std::path::Path) -> Result<()> {
    use tokio::process::Command;

    // Read the current crontab (no crontab installed => empty, not an error).
    let read = Command::new("crontab").arg("-l").output().await.context("running `crontab -l`")?;
    let current = if read.status.success() {
        String::from_utf8_lossy(&read.stdout).into_owned()
    } else {
        String::new()
    };

    match cmd {
        CronCmd::Show => {
            let arena: Vec<&str> = current
                .lines()
                .skip_while(|l| l.trim() != CRON_BEGIN)
                .take_while(|l| l.trim() != CRON_END)
                .filter(|l| l.trim() != CRON_BEGIN)
                .collect();
            if arena.is_empty() {
                println!("(no arena-managed cron lines)");
            } else {
                for l in arena {
                    println!("{l}");
                }
            }
            return Ok(());
        }
        CronCmd::Remove => {
            let new = with_arena_block(&current, &[]);
            write_crontab(&new).await?;
            println!("Removed arena-managed cron lines.");
            return Ok(());
        }
        CronCmd::Install { schedule, start_date } => {
            let exe = std::env::current_exe().context("finding the arena executable path")?;
            let cfg_abs = std::fs::canonicalize(config_path)
                .unwrap_or_else(|_| config_path.to_path_buf());
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            // Optional inline env (cron runs the line via sh, so `VAR=val cmd` works).
            let env_prefix = match &start_date {
                Some(d) => {
                    arena_core::schedule::parse_ymd(d)
                        .context("--start-date must be YYYY-MM-DD")?;
                    format!("ARENA_START_DATE={d} ")
                }
                None => String::new(),
            };
            let line = format!(
                "{schedule} {env_prefix}{} --config {} pods backup --yes >> {}/arena-cron.log 2>&1",
                exe.display(),
                cfg_abs.display(),
                home
            );
            let new = with_arena_block(&current, &[line.clone()]);
            write_crontab(&new).await?;
            println!("Installed arena cron job:\n  {line}");
            println!("\n(remove with `arena cron remove`; view with `arena cron show`)");
            return Ok(());
        }
    }
}

/// Replace the crontab with `content` via `crontab -` (reads from stdin).
async fn write_crontab(content: &str) -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning `crontab -`")?;
    child
        .stdin
        .as_mut()
        .context("opening crontab stdin")?
        .write_all(content.as_bytes())
        .await
        .context("writing crontab")?;
    let status = child.wait().await.context("waiting for crontab")?;
    if !status.success() {
        anyhow::bail!("`crontab -` exited with {status}");
    }
    Ok(())
}

fn handle_config(
    cmd: ConfigCmd,
    cfg: &Config,
    provider_name: &str,
    config_path: &std::path::Path,
) -> Result<()> {
    match cmd {
        ConfigCmd::Check => config_check(cfg, provider_name),
        ConfigCmd::Set { key, value } => {
            let (key, value) = match (key, value) {
                (Some(k), Some(v)) => (k, v),
                _ => interactive_config_set()?,
            };
            let text = std::fs::read_to_string(config_path)
                .with_context(|| format!("reading {}", config_path.display()))?;
            let updated = arena_core::config::upsert_line(&text, &key, &value);
            std::fs::write(config_path, updated)
                .with_context(|| format!("writing {}", config_path.display()))?;
            // Never echo the value (it may be a secret).
            println!("set {key} in {}", config_path.display());
            Ok(())
        }
    }
}

/// Report one config key: prints a ✓/✗/· line (never the value if `secret`) and
/// records it in `missing` when it's required but absent/empty.
fn cfg_row(cfg: &Config, missing: &mut Vec<String>, key: &str, required: bool, secret: bool) {
    let ok = cfg.get(key).map(|v| !v.is_empty()).unwrap_or(false);
    let mark = if ok { "✓" } else if required { "✗" } else { "·" };
    let shown = if !ok {
        "(missing)".to_string()
    } else if secret {
        "(set)".to_string()
    } else {
        cfg.get(key).unwrap_or("").to_string()
    };
    println!("  {mark} {key:<28} {shown}");
    if required && !ok {
        missing.push(key.to_string());
    }
}

/// Interactively pick a config key and read its value (for `config set` with no args).
/// Requires a terminal.
fn interactive_config_set() -> Result<(String, String)> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("config set needs a key and value non-interactively: arena config set KEY VALUE");
    }
    let read = |prompt: &str| -> Result<String> {
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        Ok(s.trim().to_string())
    };

    const OPTS: &[&str] = &["RUNPOD_API_KEY", "VAST_API_KEY", "HETZNER_API_KEY", "ARENA_START_DATE"];
    eprintln!("Which key to set?");
    for (i, k) in OPTS.iter().enumerate() {
        eprintln!("  {}) {k}", i + 1);
    }
    eprintln!("  {}) other (type the key name)", OPTS.len() + 1);
    let choice = read("> ")?;
    let key = match choice.parse::<usize>() {
        Ok(n) if (1..=OPTS.len()).contains(&n) => OPTS[n - 1].to_string(),
        Ok(n) if n == OPTS.len() + 1 => read("key name: ")?,
        _ => choice, // a key name typed directly
    };
    if key.is_empty() {
        anyhow::bail!("no key chosen");
    }
    let value = read(&format!("value for {key}: "))?;
    if value.is_empty() {
        anyhow::bail!("empty value — nothing set");
    }
    Ok((key, value))
}

/// Print a config checklist for the selected provider + proxy + backup, never showing
/// secret values. Returns an error if a required key is missing.
fn config_check(cfg: &Config, provider_name: &str) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();

    // Show every provider's API key (set/empty), marking the selected one. Only the
    // selected provider's key is *required* (counts toward `missing`).
    println!("Providers (selected: {provider_name}):");
    for (name, key) in [
        ("runpod", "RUNPOD_API_KEY"),
        ("vast", "VAST_API_KEY"),
        ("hetzner", "HETZNER_API_KEY"),
    ] {
        let selected = name == provider_name;
        let set = cfg.get(key).map(|v| !v.is_empty()).unwrap_or(false);
        let mark = if set { "✓" } else if selected { "✗" } else { "·" };
        println!(
            "  {mark} {key:<24} {}{}",
            if set { "(set)" } else { "(empty)" },
            if selected { "   ← selected" } else { "" }
        );
        if selected && !set {
            missing.push(key.to_string());
        }
    }
    if provider_name == "hetzner" {
        cfg_row(cfg, &mut missing, "HETZNER_SERVER_TYPE", false, false);
        cfg_row(cfg, &mut missing, "HETZNER_IMAGE", false, false);
        cfg_row(cfg, &mut missing, "HETZNER_SSH_KEY", false, false);
    }

    println!("\nMachine naming:");
    cfg_row(cfg, &mut missing, "MACHINE_NAME_PREFIX", false, false);
    println!(
        "  {} MACHINE_NAME_LIST          {} names",
        if cfg.machine_names.is_empty() { "✗" } else { "✓" },
        cfg.machine_names.len()
    );
    if cfg.machine_names.is_empty() {
        missing.push("MACHINE_NAME_LIST".into());
    }

    if provider_name != "hetzner" {
        println!("\nCreate spec (generic key, else RUNPOD_* fallback):");
        let spec = PodSpec::from_config(cfg);
        let yn = |s: &str| if s.is_empty() { "✗ (missing)".into() } else { format!("✓ {s}") };
        println!("  GPU_TYPE / RUNPOD_GPU_TYPE   {}", yn(&spec.gpu_type));
        println!("  IMAGE / RUNPOD_DOCKER_IMAGE  {}", yn(&spec.image));
        println!("  resolved disk                {}GB", spec.disk_gb);
        if spec.volume_gb == 0 {
            println!("  ⚠ persistent volume          0GB — work is lost on pod restart");
        }
    }

    println!("\nSSH (backup + dashboard metrics):");
    cfg_row(cfg, &mut missing, "SSH_USER", false, false);
    cfg_row(cfg, &mut missing, "SHARED_SSH_KEY_PATH", false, false);

    println!("\nProxy (`proxy plan`):");
    cfg_row(cfg, &mut missing, "SSH_PROXY_HOST", false, false);
    cfg_row(cfg, &mut missing, "SSH_PROXY_STARTING_PORT", false, false);

    println!("\nBackup (`backup`):");
    cfg_row(cfg, &mut missing, "ARENA_REPO_NAME", false, false);
    cfg_row(cfg, &mut missing, "GIT_SSH_KEY_REMOTE", false, false);
    cfg_row(cfg, &mut missing, "ARENA_START_DATE", false, false); // for wNdM naming

    println!("\nDashboard (optional):");
    cfg_row(cfg, &mut missing, "PROGRESS_CMD", false, false);

    // Read-only readiness: are the local files this user needs actually there/readable?
    // Answers "is it set up yet?" without touching any API.
    println!("\nSetup readiness (local, read-only):");
    let readable = |p: &str| std::fs::File::open(p).is_ok();
    if let Some(k) = cfg.get("SHARED_SSH_KEY_PATH") {
        // Resolve the same way pods/proxy do (readable ~/.ssh fallback).
        let resolved = arena_core::ssh::SshTarget::for_host("x", "x", 22, Some(k))
            .key_path
            .unwrap_or_default();
        let ok = readable(&resolved);
        println!(
            "  {} shared SSH key             {}{}",
            if ok { "✓" } else { "✗" },
            resolved,
            if ok || resolved == k { String::new() } else { format!("  (configured: {k})") }
        );
        if !ok {
            println!("      └ not readable by this user — dashboard metrics / SSH will fail");
        }
    }
    if let Some(k) = cfg.get("GIT_SSH_KEY_LOCAL") {
        let resolved = arena_core::ssh::resolve_key_path(k);
        println!("  {} git deploy key (local)    {resolved}", if readable(&resolved) { "✓" } else { "✗" });
        let pubk = format!("{resolved}.pub");
        println!("  {} git deploy key .pub       {pubk}", if readable(&pubk) { "✓" } else { "✗" });
    }
    let plan_present = readable("arena-plan.json");
    println!(
        "  {} provisioning plan          {}",
        if plan_present { "✓" } else { "·" },
        if plan_present { "arena-plan.json" } else { "none (optional — `arena plan`)" }
    );
    println!(
        "  {} iteration start date       {}",
        if cfg.get("ARENA_START_DATE").is_some() { "✓" } else { "✗" },
        cfg.get("ARENA_START_DATE").unwrap_or("unset — backups can't label wNdM")
    );

    if missing.is_empty() {
        println!("\nOK — required keys for provider `{provider_name}` are present.");
        Ok(())
    } else {
        anyhow::bail!("missing required keys: {}", missing.join(", "))
    }
}

async fn handle_backup(
    provider: &dyn Provider,
    cfg: &Config,
    apply: bool,
    message: Option<String>,
    week: Option<u32>,
    day: Option<u32>,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let (week, day) = resolve_week_day(cfg, week, day)?;
    let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
    // Per-machine default commit message = that machine's autocommit branch name.
    let msg_for = |name: &str| message.clone().unwrap_or_else(|| bcfg.branch_for(name));
    println!("Iteration: w{week}d{day}\n");

    let pods = provider.list_pods().await.context("listing pods for backup")?;
    // Back up only pods that actually have an SSH endpoint; report the rest.
    let mut targets = Vec::new();
    for pod in &pods {
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to back up)");
        return Ok(());
    }

    if !apply {
        println!("Dry-run — would back up {} pod(s):\n", targets.len());
        for (name, target) in &targets {
            let cmd = arena_core::backup::backup_command(&bcfg, name, &msg_for(name));
            println!("# {name}  ->  branch {}", bcfg.branch_for(name));
            println!("{}\n", target.display_command(&cmd));
        }
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Apply: run sequentially so output stays readable and one failure doesn't
    // obscure the rest. Each pod's result is classified (backed up / no changes / error).
    let mut backed_up = 0;
    let mut no_changes = 0;
    let mut failed = 0;
    for (name, target) in &targets {
        let cmd = arena_core::backup::backup_command(&bcfg, name, &msg_for(name));
        match ssh::run(target, &cmd).await {
            Ok(out) if out.success && out.stdout.lines().any(|l| l.trim() == "NO_CHANGES") => {
                println!("[no changes] {name}");
                no_changes += 1;
            }
            Ok(out) if out.success => {
                println!("[backed up]  {name} -> {}", bcfg.branch_for(name));
                backed_up += 1;
            }
            Ok(out) => {
                eprintln!(
                    "[FAILED]     {name} (exit {:?}): {}",
                    out.code,
                    out.stderr.trim()
                );
                failed += 1;
            }
            Err(e) => {
                eprintln!("[FAILED]     {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {backed_up} backed up, {no_changes} unchanged, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to back up");
    }
    Ok(())
}

async fn handle_proxy(cmd: ProxyCmd, provider: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    match cmd {
        ProxyCmd::Plan { out } => {
            let pods = provider.list_pods().await.context("listing pods for proxy plan")?;
            emit_proxy_plan(&pods, cfg, out.as_deref())?;
        }
        ProxyCmd::Apply { dry_run } => {
            let pods = provider.list_pods().await.context("listing pods for proxy apply")?;
            if !dry_run {
                let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
                if !confirm(yes, &format!("Will deploy the nginx config to {} and reload nginx.", pxcfg.proxy_host))? {
                    println!("aborted.");
                    return Ok(());
                }
            }
            deploy_proxy(cfg, &pods, !dry_run).await?;
        }
    }
    Ok(())
}

/// Render the proxy config for `pods` and (with `apply`) deploy it to the proxy host
/// over SSH, then reload nginx. Dry-run prints the exact scp + reload it would run.
/// This is the one place the tool touches the proxy host.
async fn deploy_proxy(cfg: &Config, pods: &[arena_core::Pod], apply: bool) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let plan = arena_core::proxy::plan_forwards(&pxcfg, prefix, &cfg.machine_names, pods);
    let nginx = arena_core::proxy::render_nginx(&plan.forwards);
    let target =
        SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let reload = "nginx -t && nginx -s reload";

    for s in &plan.skipped {
        eprintln!("warning: skipped {} — {}", s.name, s.reason);
    }

    if !apply {
        println!(
            "[dry-run] would deploy {} forward(s) to {}@{}:{}",
            plan.forwards.len(),
            pxcfg.proxy_user,
            pxcfg.proxy_host,
            pxcfg.nginx_path
        );
        println!("  {}", target.display_scp("<rendered nginx>", &pxcfg.nginx_path));
        println!("  {}", target.display_command(reload));
        println!("(preview only — run without --dry-run to deploy and reload nginx)");
        return Ok(());
    }

    // Write the rendered config locally, scp it to the proxy, then reload nginx.
    let tmp = std::env::temp_dir().join("arena-proxy.conf");
    std::fs::write(&tmp, &nginx).context("writing rendered nginx config to a temp file")?;
    let scp = ssh::scp(&target, &tmp.to_string_lossy(), &pxcfg.nginx_path).await?;
    if !scp.success {
        anyhow::bail!("scp to proxy {} failed: {}", pxcfg.proxy_host, scp.stderr.trim());
    }
    let out = ssh::run(&target, reload).await?;
    if out.success {
        println!(
            "[proxy] deployed {} forward(s) to {} and reloaded nginx",
            plan.forwards.len(),
            pxcfg.proxy_host
        );
        Ok(())
    } else {
        anyhow::bail!(
            "nginx reload on {} failed (exit {:?}): {}",
            pxcfg.proxy_host,
            out.code,
            out.stderr.trim()
        )
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

async fn handle_plan(cmd: PlanCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    use arena_core::plan::{within_window, Plan};
    use arena_core::schedule::{days_from_civil, parse_ymd, ymd_string};

    let (today_days, now_min, today_ymd, tz) = local_now()?;

    match cmd {
        PlanCmd::Check { file } => {
            let plan = Plan::load(&file)?;
            println!("plan file:  {}", file.display());
            println!("local now:  {today_ymd} {:02}:{:02} {tz}", now_min / 60, now_min % 60);
            match plan.window_minutes() {
                Some((s, e)) => println!(
                    "window:     {}–{} local — now is {}",
                    plan.window[0],
                    plan.window[1],
                    if within_window(now_min, s, e) { "INSIDE" } else { "outside" }
                ),
                None => println!("window:     INVALID {:?} — fix HH:MM", plan.window),
            }
            println!(
                "caps:       max_total={:?}  max_hourly={:?}  max_replace={:?}",
                plan.max_total, plan.max_hourly, plan.max_replace
            );
            println!("gpus:       {}", plan.gpus.join(" > "));
            println!("providers:  {}", plan.providers.join(" > "));
            println!("days:");
            if plan.days.is_empty() {
                println!("  (none)");
            }
            for d in &plan.days {
                let date = d.effective_days(today_days).map(ymd_string).unwrap_or_else(|| "??".into());
                let today = (d.effective_days(today_days) == Some(today_days)).then_some(" (today)").unwrap_or("");
                println!(
                    "  {date}{today}: {} pods × {}gpu{}",
                    d.count,
                    d.gpus_per_pod,
                    if d.replace { "  [REPLACE]" } else { "" }
                );
            }
            println!("\n✓ plan parses. (Scheduled apply + arming aren't wired yet — preview with `plan show`.)");
        }

        PlanCmd::Show { file, date } => {
            let plan = Plan::load(&file)?;
            let target_days = match &date {
                Some(s) => {
                    let (y, m, d) = parse_ymd(s).context("--date must be YYYY-MM-DD")?;
                    days_from_civil(y, m, d)
                }
                None => today_days,
            };
            let target_ymd = ymd_string(target_days);
            let Some(day) = plan.day_for(today_days, target_days) else {
                println!("No plan entry for {target_ymd}.");
                return Ok(());
            };

            println!(
                "Plan for {target_ymd}: {} pods × {} GPU/pod{}",
                day.count,
                day.gpus_per_pod,
                if day.replace { "  [REPLACE]" } else { "" }
            );

            // Existing fleet on the current provider (for the fill-to-target preview).
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let pods = provider.list_pods().await.context("listing pods")?;
            let existing = pods.iter().filter(|p| p.name.starts_with(&format!("{prefix}-"))).count();
            let remaining = day.count.saturating_sub(existing);
            println!(
                "Current fleet on {}: {existing} pod(s) named {prefix}-*  →  would create up to {remaining} more to reach {}.",
                provider.name(),
                day.count
            );
            if day.replace {
                println!(
                    "REPLACE: would back up, then terminate those {existing} pod(s) (cap max_replace={:?}), then create {} fresh.",
                    plan.max_replace, day.count
                );
            }

            println!("\nFallback order (GPU-first; stop once {} filled):", day.count);
            for (i, c) in day.candidates(&plan).iter().enumerate() {
                let tier = c.cloud.as_deref().map(|t| format!(":{t}")).unwrap_or_default();
                println!("  {:>2}. {} on {}{tier}", i + 1, c.gpu, c.provider);
            }

            if let Some((s, e)) = plan.window_minutes() {
                let inside = within_window(now_min, s, e);
                println!(
                    "\nWindow {}–{} local — a scheduled run right now would be {}.",
                    plan.window[0],
                    plan.window[1],
                    if inside { "ELIGIBLE" } else { "SKIPPED (outside window)" }
                );
            }
            println!("(Preview only — the executor that actually creates/replaces is the next step.)");
        }
    }
    Ok(())
}

/// Render and print the proxy plan for the given pods: a summary table, the nginx
/// `stream` config (optionally also written to `out`, locally), and any skipped
/// pods. Shared by `proxy plan` and `pods up`. Never connects to the proxy.
fn emit_proxy_plan(pods: &[arena_core::Pod], cfg: &Config, out: Option<&std::path::Path>) -> Result<()> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let plan = arena_core::proxy::plan_forwards(&pxcfg, prefix, &cfg.machine_names, pods);

    println!(
        "\n# proxy host: {}@{}  (nginx config path: {})",
        pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path
    );
    if plan.forwards.is_empty() {
        println!("(no forwardable pods — nothing with an SSH endpoint in the name list)");
    } else {
        println!("{:<24} {:<14} {}", "NAME", "PUBLIC", "-> POD SSH");
        for f in &plan.forwards {
            println!(
                "{:<24} {}:{:<8} {}:{}",
                f.name, pxcfg.proxy_host, f.public_port, f.target_ip, f.target_port
            );
        }
    }
    for s in &plan.skipped {
        eprintln!("warning: skipped {} — {}", s.name, s.reason);
    }

    let nginx = arena_core::proxy::render_nginx(&plan.forwards);
    println!("\n# ----- nginx config (write to {} on the proxy) -----", pxcfg.nginx_path);
    println!("{nginx}");

    if let Some(path) = out {
        std::fs::write(path, &nginx)
            .with_context(|| format!("writing nginx config to {}", path.display()))?;
        eprintln!("wrote nginx config to {} (local only — not deployed)", path.display());
    } else {
        println!(
            "# Review, then apply on the proxy: write the above to {}, then \
             `nginx -t && nginx -s reload`. (--out <file> saves it locally.)",
            pxcfg.nginx_path
        );
    }
    Ok(())
}

async fn handle_pods(cmd: PodCmd, provider: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    match cmd {
        PodCmd::List { json, probe } => {
            let policy = arena_core::retry::RetryPolicy::default();
            let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await?;
            pods.sort_by(|a, b| a.name.cmp(&b.name));
            if probe {
                // The list API omits GPU type; fill it from nvidia-smi over SSH
                // (same source as the TUI), concurrently across the fleet.
                use arena_core::metrics::{self, ProbeOpts};
                use arena_core::ssh::SshTarget;
                let mut set = tokio::task::JoinSet::new();
                for pod in &pods {
                    if let Ok(mut t) = SshTarget::from_pod(pod, cfg) {
                        t.connect_timeout_secs = 5;
                        let name = pod.name.clone();
                        set.spawn(async move { (name, metrics::fetch(&t, &ProbeOpts::default()).await) });
                    }
                }
                let mut gpus = std::collections::HashMap::new();
                while let Some(joined) = set.join_next().await {
                    if let Ok((name, m)) = joined {
                        if let Some(g) = m.gpu_summary() {
                            gpus.insert(name, g);
                        }
                    }
                }
                for p in &mut pods {
                    if let Some(g) = gpus.get(&p.name) {
                        p.gpu_type = Some(g.clone());
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&pods)?);
                return Ok(());
            }
            if pods.is_empty() {
                println!("(no pods)");
                return Ok(());
            }
            println!(
                "{:<22} {:<14} {:<10} {:<16} {:<16} {}",
                "NAME", "ID", "STATUS", "GPU", "IP", "PORT"
            );
            for p in &pods {
                println!(
                    "{:<22} {:<14} {:<10} {:<16} {:<16} {}",
                    p.name,
                    p.id,
                    arena_core::status::short_status(&p.status),
                    p.gpu_type.as_deref().unwrap_or("-"),
                    p.ssh_ip.as_deref().unwrap_or("-"),
                    p.ssh_port.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }

        PodCmd::Create { count, add, gpu, gpus, cloud, disk, volume, dry_run, keep_trying } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume };
            let names = plan_names(provider, cfg, resolve_want(count, add)?).await?;
            if names.is_empty() {
                eprintln!("no free machine names available — nothing to do");
                return Ok(());
            }
            if dry_run {
                let spec = spec_with_overrides(cfg, &ov);
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                println!("\nDry-run only — no pods created (this is a preview).");
                return Ok(());
            }
            let spec = spec_with_overrides(cfg, &ov);
            if !confirm(yes, &format!(
                "Will create {} pod(s) on {} ({}).",
                names.len(), provider.name(), provider.describe(&spec)
            ))? {
                println!("aborted.");
                return Ok(());
            }
            create_pods(provider, cfg, &names, keep_trying, &ov).await?;
        }

        PodCmd::Up { count, add, gpu, gpus, cloud, disk, volume, dry_run, no_wait, keep_trying, proxy, setup, timeout, interval } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume };
            let names = plan_names(provider, cfg, resolve_want(count, add)?).await?;
            if names.is_empty() {
                eprintln!("no free machine names available — nothing to do");
                return Ok(());
            }

            if dry_run {
                let spec = spec_with_overrides(cfg, &ov);
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                let extra = match (setup, proxy) {
                    (true, true) => " then run setup and deploy the proxy",
                    (true, false) => " then run setup",
                    (false, true) => " then deploy the proxy",
                    (false, false) => "",
                };
                println!(
                    "\nDry-run only — no pods created (preview): would create the above, \
                     poll up to {timeout}s for SSH endpoints, print the proxy plan{extra}."
                );
                return Ok(());
            }

            let spec = spec_with_overrides(cfg, &ov);
            if !confirm(yes, &format!(
                "Will create {} pod(s) on {} ({}), wait for endpoints{}{}.",
                names.len(),
                provider.name(),
                provider.describe(&spec),
                if setup { ", run setup" } else { "" },
                if proxy { ", deploy proxy" } else { "" }
            ))? {
                println!("aborted.");
                return Ok(());
            }
            // Create as many as capacity allows; we only wait on the ones we got.
            let created = create_pods(provider, cfg, &names, keep_trying, &ov).await?;
            if created.is_empty() {
                eprintln!("no pods were created — nothing to wait for");
                return Ok(());
            }
            // Match readiness by pod id, not name: a provider's reported name doesn't
            // always equal the name we asked for (e.g. Vast's `vast-<id>` fallback),
            // which would make us wait forever on a pod that's actually up.
            let want_ids: std::collections::HashSet<String> =
                created.iter().map(|p| p.id.clone()).collect();

            if no_wait {
                println!(
                    "\n--no-wait: not polling. Run `arena proxy plan` once endpoints are assigned."
                );
                return Ok(());
            }

            // Poll the whole fleet (one list call per tick) until our pods have SSH
            // endpoints or we hit the timeout. Stateless: each tick re-reads truth.
            let policy = arena_core::retry::RetryPolicy::default();
            let interval = interval.max(1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            println!(
                "\nWaiting up to {timeout}s for SSH endpoints (polling every {interval}s)…"
            );
            let is_ready =
                |p: &arena_core::Pod| p.ssh_ip.is_some() && p.ssh_port.is_some();
            let pods = loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                let pods = match arena_core::retry::retrying(&policy, || provider.list_pods()).await
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("  poll failed ({e}); retrying");
                        if std::time::Instant::now() >= deadline {
                            break Vec::new();
                        }
                        continue;
                    }
                };
                let ready = pods
                    .iter()
                    .filter(|p| want_ids.contains(&p.id) && is_ready(p))
                    .count();
                println!("  {ready}/{} ready", want_ids.len());
                if ready == want_ids.len() || std::time::Instant::now() >= deadline {
                    break pods;
                }
            };

            let not_ready: Vec<&str> = created
                .iter()
                .filter(|c| !pods.iter().any(|p| p.id == c.id && is_ready(p)))
                .map(|c| c.name.as_str())
                .collect();
            if !not_ready.is_empty() {
                eprintln!(
                    "warning: timed out waiting for: {} (still starting?). \
                     Re-run `arena proxy plan` once they're up.",
                    not_ready.join(", ")
                );
            }
            emit_proxy_plan(&pods, cfg, None)?;

            // Optional chaining so spin-up is one command (provision + wire the proxy).
            if setup {
                println!("\nProvisioning pods over SSH…");
                handle_setup(provider, cfg, true, false).await?;
            }
            if proxy {
                println!("\nDeploying proxy config…");
                deploy_proxy(cfg, &pods, true).await?;
            }
        }

        PodCmd::Stop { target, dry_run } => {
            let (id, label) = resolve_target(provider, &target).await?;
            if dry_run {
                println!("[dry-run] would stop {label}");
            } else {
                if !confirm(yes, &format!("Will stop {label}."))? {
                    println!("aborted.");
                    return Ok(());
                }
                provider.stop_pod(&id).await?;
                println!("[stopped] {label}");
            }
        }

        PodCmd::Restart { target, dry_run } => {
            let (id, label) = resolve_target(provider, &target).await?;
            if dry_run {
                println!("[dry-run] would restart {label}");
            } else {
                if !confirm(yes, &format!("Will restart {label}."))? {
                    println!("aborted.");
                    return Ok(());
                }
                provider.restart_pod(&id).await?;
                println!("[restarted] {label}");
            }
        }

        PodCmd::Terminate { target, all, dry_run } => match (all, target) {
            (true, _) => {
                let policy = arena_core::retry::RetryPolicy::default();
                let mut pods =
                    arena_core::retry::retrying(&policy, || provider.list_pods()).await?;
                pods.sort_by(|a, b| a.name.cmp(&b.name));
                if pods.is_empty() {
                    println!("(no pods to terminate)");
                    return Ok(());
                }
                if dry_run {
                    for p in &pods {
                        println!("[dry-run] would terminate {} (id={})", p.name, p.id);
                    }
                    println!("\nDry-run only — would terminate ALL {} pod(s) (preview).", pods.len());
                    return Ok(());
                }
                if !confirm(yes, &format!("Will TERMINATE ALL {} pod(s) — irreversible.", pods.len()))? {
                    println!("aborted.");
                    return Ok(());
                }
                let total = pods.len();
                let mut ok = 0;
                for p in &pods {
                    match provider.terminate_pod(&p.id).await {
                        Ok(()) => {
                            println!("[terminated] {}", p.name);
                            ok += 1;
                        }
                        Err(e) => eprintln!("[FAILED] {}: {e}", p.name),
                    }
                }
                println!("\nterminated {ok}/{total}");
                if ok < total {
                    anyhow::bail!("{} pod(s) failed to terminate", total - ok);
                }
            }
            (false, Some(target)) => {
                let (id, label) = resolve_target(provider, &target).await?;
                if dry_run {
                    println!("[dry-run] would terminate {label}");
                } else {
                    if !confirm(yes, &format!("Will TERMINATE {label} — irreversible."))? {
                        println!("aborted.");
                        return Ok(());
                    }
                    provider.terminate_pod(&id).await?;
                    println!("[terminated] {label}");
                }
            }
            (false, None) => {
                anyhow::bail!("specify a pod (name or id) to terminate, or pass --all");
            }
        },

        PodCmd::Backup { dry_run, message, week, day } => {
            if !dry_run && !confirm(yes, "Will commit + push each pod's ARENA tree over SSH.")? {
                println!("aborted.");
                return Ok(());
            }
            handle_backup(provider, cfg, !dry_run, message, week, day).await?;
        }
        PodCmd::Setup { dry_run, force } => {
            if !dry_run && !confirm(yes, "Will provision each pod over SSH (deploy key, ~/.name, repo).")? {
                println!("aborted.");
                return Ok(());
            }
            handle_setup(provider, cfg, !dry_run, force).await?;
        }
        PodCmd::SetBranch { branch, target, all, dry_run } => {
            handle_set_branch(provider, cfg, &branch, target.as_deref(), all, dry_run, yes).await?;
        }
    }
    Ok(())
}

/// Switch one pod (or, with `all`, every pod with an SSH endpoint) to `branch` via a
/// gentle fetch+checkout+ff-pull over SSH. Dry-run prints the exact command per pod.
async fn handle_set_branch(
    provider: &dyn Provider,
    cfg: &Config,
    branch: &str,
    target: Option<&str>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE");
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, key);

    // Which pods: --all (every reachable one) or a single resolved target.
    let pods = provider.list_pods().await.context("listing pods")?;
    let mut targets: Vec<(String, SshTarget)> = Vec::new();
    if all {
        for pod in &pods {
            if let Ok(t) = SshTarget::from_pod(pod, cfg) {
                targets.push((pod.name.clone(), t));
            }
        }
    } else if let Some(want) = target {
        match pods.iter().find(|p| p.name == want || p.id == want) {
            Some(pod) => targets.push((pod.name.clone(), SshTarget::from_pod(pod, cfg)?)),
            None => anyhow::bail!("no pod with name or id '{want}' (run `arena pods list`)"),
        }
    } else {
        anyhow::bail!("specify a pod (name or id) or pass --all");
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to switch)");
        return Ok(());
    }

    if dry_run {
        for (name, t) in &targets {
            println!("# {name}");
            println!("{}\n", t.display_command(&cmd));
        }
        println!("Dry-run only — nothing changed (preview).");
        return Ok(());
    }
    if !confirm(yes, &format!("Will switch {} pod(s) to branch '{branch}' (gentle, no reset).", targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let (mut ok, mut failed) = (0, 0);
    for (name, t) in &targets {
        match ssh::run(t, &cmd).await {
            Ok(out) if out.success => {
                println!("[on {branch}]  {name}");
                ok += 1;
            }
            Ok(out) => {
                eprintln!("[FAILED]  {name}: {}", out.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                eprintln!("[FAILED]  {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nswitched {ok}/{}", ok + failed);
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to switch branch");
    }
    Ok(())
}

/// Resolve a user-supplied target (machine name OR raw provider id) to a concrete
/// `(id, label)`, by listing pods. Requires the pod to actually exist, so a typo'd
/// name/id fails clearly instead of issuing a no-op or wrong mutation. Read-only.
async fn resolve_target(provider: &dyn Provider, target: &str) -> Result<(String, String)> {
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context("listing pods to resolve target")?;
    match pods.iter().find(|p| p.name == target || p.id == target) {
        Some(p) => Ok((p.id.clone(), format!("{} (id={})", p.name, p.id))),
        None => anyhow::bail!("no pod with name or id '{target}' (run `arena pods list`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_arena_block, with_arena_block, CRON_BEGIN, CRON_END};

    #[test]
    fn install_preserves_other_crontab_entries() {
        let existing = "0 9 * * * /usr/bin/other-job\n# my note\n";
        let line = "0 * * * * arena pods backup --yes".to_string();
        let out = with_arena_block(existing, &[line.clone()]);
        // keeps the user's entries…
        assert!(out.contains("/usr/bin/other-job"));
        assert!(out.contains("# my note"));
        // …and adds a fenced arena block
        assert!(out.contains(CRON_BEGIN));
        assert!(out.contains(CRON_END));
        assert!(out.contains(&line));
    }

    #[test]
    fn install_is_idempotent_replacing_old_block() {
        let existing = "0 9 * * * keep-me".to_string();
        let v1 = with_arena_block(&existing, &["A".to_string()]);
        let v2 = with_arena_block(&v1, &["B".to_string()]);
        assert!(v2.contains("keep-me"));
        assert!(v2.contains('B'));
        assert!(!v2.contains("* A") && v2.matches(CRON_BEGIN).count() == 1); // only one block
    }

    #[test]
    fn remove_strips_only_the_arena_block() {
        let existing = "keep-me\n# >>> arena-infra-rs >>>\njob\n# <<< arena-infra-rs <<<\nalso-keep\n";
        let out = with_arena_block(existing, &[]);
        assert!(out.contains("keep-me"));
        assert!(out.contains("also-keep"));
        assert!(!out.contains("job"));
        assert!(!out.contains(CRON_BEGIN));
    }

    #[test]
    fn strip_handles_no_block() {
        assert_eq!(strip_arena_block("a\nb"), "a\nb");
    }
}
