//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Every mutating
//! command defaults to a **dry-run** that prints exactly what it *would* do; you
//! must pass `--apply` to actually call the provider. This is deliberate — the
//! tool is developed against a live production account.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arena_core::provider::Provider;
use arena_core::{Config, PodSpec};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

#[derive(Parser)]
#[command(name = "arena", version, about = "Streamlined ARENA infra control plane")]
struct Cli {
    /// Path to config.env (defaults to the read-only prod copy).
    #[arg(long, default_value = DEFAULT_CONFIG, global = true)]
    config: PathBuf,

    /// Compute provider to target.
    #[arg(long, default_value = "runpod", global = true)]
    provider: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Machine (pod) lifecycle.
    #[command(subcommand)]
    Pods(PodCmd),
    /// Plan port-forwarding/proxy wiring (read-only; prints config to apply).
    #[command(subcommand)]
    Proxy(ProxyCmd),
    /// Commit + push each pod's ARENA working tree to its autocommit branch over SSH.
    /// Branch is autocommit-{prefix}-w{week}d{day}-{machine}. Dry-run unless --apply.
    Backup {
        #[arg(long)]
        apply: bool,
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
    /// repo at GitHub. Dry-run unless --apply.
    Setup {
        #[arg(long)]
        apply: bool,
        /// Force-checkout the default branch and hard-reset (else stay on current).
        #[arg(long)]
        force: bool,
    },
    /// Inspect the loaded config.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Manage a cron schedule for `arena backup` (edits your crontab, touching only
    /// arena-managed lines).
    #[command(subcommand)]
    Cron(CronCmd),
}

#[derive(Subcommand)]
enum CronCmd {
    /// Install/replace the arena backup cron job.
    Install {
        /// Cron schedule expression (default: hourly).
        #[arg(long, default_value = "0 * * * *")]
        schedule: String,
    },
    /// Remove the arena-managed cron lines.
    Remove,
    /// Show the currently-installed arena cron lines.
    Show,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate that the keys needed by the selected provider, proxy, and backup are
    /// present. Read-only; never prints secret values. Exits non-zero if a required
    /// key is missing.
    Check,
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
}

#[derive(Subcommand)]
enum PodCmd {
    /// List current pods (read-only).
    List {
        /// Emit JSON instead of a table (for scripting).
        #[arg(long)]
        json: bool,
    },
    /// Create N pods on the next free machine names. Dry-run unless --apply.
    Create {
        #[arg(short = 'n', long)]
        count: usize,
        #[arg(long)]
        apply: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
    },
    /// Create N pods, then poll until they have SSH endpoints and print the proxy
    /// plan — the one-command spin-up. Dry-run unless --apply. Polling stops at the
    /// timeout; it never runs in the background or mutates the proxy.
    Up {
        #[arg(short = 'n', long)]
        count: usize,
        #[arg(long)]
        apply: bool,
        /// Don't poll after creating; just print ids (run `proxy plan` later).
        #[arg(long)]
        no_wait: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
        /// Give up waiting for endpoints after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between readiness polls (one list call per poll, whole fleet).
        #[arg(long, default_value_t = 12)]
        interval: u64,
    },
    /// Stop a pod by name or id. Dry-run unless --apply.
    Stop {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        #[arg(long)]
        apply: bool,
    },
    /// Restart a pod in place by name or id. Dry-run unless --apply.
    Restart {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        #[arg(long)]
        apply: bool,
    },
    /// Terminate (delete) a pod by name or id. Dry-run unless --apply.
    Terminate {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        #[arg(long)]
        apply: bool,
    },
}


/// Build the base spec for a create, preferring provider-neutral keys and falling
/// back to the legacy `RUNPOD_*` names (which is what existing config.env files have).
/// So a Vast/Hetzner operator can set `GPU_TYPE`/`DISK_GB`/etc. without touching the
/// RunPod keys, while a pure-RunPod config keeps working unchanged.
fn base_spec(cfg: &Config) -> PodSpec {
    // first non-empty of the given keys, as &str
    let first = |keys: &[&str]| keys.iter().find_map(|k| cfg.get(k).filter(|v| !v.is_empty()));
    let first_parsed = |keys: &[&str], default| {
        keys.iter().find_map(|k| cfg.get_parsed(k)).unwrap_or(default)
    };
    PodSpec {
        name: String::new(),
        image: first(&["IMAGE", "RUNPOD_DOCKER_IMAGE"]).unwrap_or_default().to_string(),
        gpu_type: first(&["GPU_TYPE", "RUNPOD_GPU_TYPE"]).unwrap_or_default().to_string(),
        gpu_count: first_parsed(&["NUM_GPUS", "RUNPOD_NUM_GPUS"], 1),
        cloud_type: first(&["CLOUD_TYPE", "RUNPOD_CLOUD_TYPE"]).unwrap_or("COMMUNITY").to_string(),
        disk_gb: first_parsed(&["DISK_GB", "RUNPOD_DISK_SPACE_IN_GB"], 100),
        volume_gb: first_parsed(&["VOLUME_GB", "RUNPOD_VOLUME_SPACE_IN_GB"], 0),
        ports: "8888/http,22/tcp".to_string(),
        env: Vec::new(),
    }
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
/// pods" would make `--apply` create a duplicate of the entire fleet on the live
/// account. Better to abort with an error the operator can see.
async fn plan_names(provider: &dyn Provider, cfg: &Config, count: usize) -> Result<Vec<String>> {
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context(
            "listing existing pods (refusing to allocate names — a failed list could \
             create duplicate pods)",
        )?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let names = arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, count);
    if names.len() < count {
        eprintln!(
            "warning: requested {count} but only {} free machine names available",
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
async fn create_pods(
    provider: &dyn Provider,
    cfg: &Config,
    names: &[String],
    keep_trying: bool,
) -> Result<Vec<arena_core::Pod>> {
    use arena_core::ProviderErrorKind as K;

    let base = base_spec(cfg);
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
        Cmd::Config(_) | Cmd::Cron(_) => None,
        _ => Some(arena_core::provider::build(&cli.provider, &cfg)?),
    };

    match cli.cmd {
        Cmd::Config(c) => handle_config(c, &cfg, &cli.provider),
        Cmd::Cron(c) => handle_cron(c, &cli.config).await,
        Cmd::Pods(p) => handle_pods(p, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Backup { apply, message, week, day } => {
            handle_backup(provider.unwrap().as_ref(), &cfg, apply, message, week, day).await
        }
        Cmd::Setup { apply, force } => {
            handle_setup(provider.unwrap().as_ref(), &cfg, apply, force).await
        }
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
        println!("Re-run with --apply to execute over SSH.");
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
        CronCmd::Install { schedule } => {
            let exe = std::env::current_exe().context("finding the arena executable path")?;
            let cfg_abs = std::fs::canonicalize(config_path)
                .unwrap_or_else(|_| config_path.to_path_buf());
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let line = format!(
                "{schedule} {} --config {} backup --apply >> {}/arena-cron.log 2>&1",
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

fn handle_config(cmd: ConfigCmd, cfg: &Config, provider_name: &str) -> Result<()> {
    match cmd {
        ConfigCmd::Check => config_check(cfg, provider_name),
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

/// Print a config checklist for the selected provider + proxy + backup, never showing
/// secret values. Returns an error if a required key is missing.
fn config_check(cfg: &Config, provider_name: &str) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();

    let provider_key = match provider_name {
        "runpod" => "RUNPOD_API_KEY",
        "vast" => "VAST_API_KEY",
        "hetzner" => "HETZNER_API_KEY",
        _ => "RUNPOD_API_KEY",
    };

    println!("Provider ({provider_name}):");
    cfg_row(cfg, &mut missing, provider_key, true, true);
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
        let spec = base_spec(cfg);
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
        println!("Re-run with --apply to execute over SSH.");
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

async fn handle_proxy(cmd: ProxyCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    match cmd {
        ProxyCmd::Plan { out } => {
            let pods = provider.list_pods().await.context("listing pods for proxy plan")?;
            emit_proxy_plan(&pods, cfg, out.as_deref())?;
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

async fn handle_pods(cmd: PodCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    match cmd {
        PodCmd::List { json } => {
            let policy = arena_core::retry::RetryPolicy::default();
            let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await?;
            pods.sort_by(|a, b| a.name.cmp(&b.name));
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
                    p.status,
                    p.gpu_type.as_deref().unwrap_or("-"),
                    p.ssh_ip.as_deref().unwrap_or("-"),
                    p.ssh_port.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }

        PodCmd::Create { count, apply, keep_trying } => {
            let names = plan_names(provider, cfg, count).await?;
            if names.is_empty() {
                eprintln!("no free machine names available — nothing to do");
                return Ok(());
            }
            if !apply {
                let spec = base_spec(cfg);
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                println!("\nDry-run only — no pods created. Re-run with --apply to execute.");
                return Ok(());
            }
            create_pods(provider, cfg, &names, keep_trying).await?;
        }

        PodCmd::Up { count, apply, no_wait, keep_trying, timeout, interval } => {
            let names = plan_names(provider, cfg, count).await?;
            if names.is_empty() {
                eprintln!("no free machine names available — nothing to do");
                return Ok(());
            }

            if !apply {
                let spec = base_spec(cfg);
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                println!(
                    "\nDry-run only — no pods created. With --apply: create the above, \
                     poll up to {timeout}s for SSH endpoints, then print the proxy plan."
                );
                return Ok(());
            }

            // Create as many as capacity allows; we only wait on the ones we got.
            let created = create_pods(provider, cfg, &names, keep_trying).await?;
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
        }

        PodCmd::Stop { target, apply } => {
            let (id, label) = resolve_target(provider, &target).await?;
            if apply {
                provider.stop_pod(&id).await?;
                println!("[stopped] {label}");
            } else {
                println!("[dry-run] would stop {label} (--apply to execute)");
            }
        }

        PodCmd::Restart { target, apply } => {
            let (id, label) = resolve_target(provider, &target).await?;
            if apply {
                provider.restart_pod(&id).await?;
                println!("[restarted] {label}");
            } else {
                println!("[dry-run] would restart {label} (--apply to execute)");
            }
        }

        PodCmd::Terminate { target, apply } => {
            let (id, label) = resolve_target(provider, &target).await?;
            if apply {
                provider.terminate_pod(&id).await?;
                println!("[terminated] {label}");
            } else {
                println!("[dry-run] would terminate {label} (--apply to execute)");
            }
        }
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
        let line = "0 * * * * arena backup --apply".to_string();
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
