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
use arena_core::config::ConfigSource;
use arena_core::{Config, PodSpec};

#[derive(Parser)]
#[command(
    name = "arena",
    version = env!("ARENA_VERSION"),
    about = "Streamlined ARENA infra control plane",
    // Cisco-style shorthands: any unambiguous prefix works (`arena po l` == `pods
    // list`). Ambiguous prefixes (e.g. `p` for pods/proxy) error and ask you to
    // disambiguate. Applies at each level.
    infer_subcommands = true
)]
struct Cli {
    /// Path to config.env. If omitted: $ARENA_CONFIG, else /home/dev/prod-ro/config.env
    /// if present, else $XDG_CONFIG_HOME/arena/config.env (~/.config/arena/config.env).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

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
    /// Manage OpenRouter API keys for the cohort (generate / list / rotate / revoke) via
    /// the provisioning API. Needs OPENROUTER_PROVISIONING_KEY in config.
    #[command(subcommand, infer_subcommands = true)]
    Keys(KeysCmd),
    /// List the known GPU types (names to pass to `--gpu`, with VRAM + rough $/hr).
    Gpus,
    /// Print the participant-facing `~/.ssh/config` for the fleet (read-only). Direct
    /// pod endpoints by default, or stable proxy ports with --proxy.
    SshConfig {
        /// Use the proxy layout (stable `SSH_PROXY_*` ports) instead of direct pod IPs.
        #[arg(long)]
        proxy: bool,
        /// Also write the rendered config to this local path (else just prints it).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeysCmd {
    /// Generate an OpenRouter runtime key per machine (named `<prefix>-<machine>`), each
    /// with a USD credit cap, and save to `keys/openrouter_api_keys.csv`. Skips machines
    /// that already have a key (use `rotate`). `--copy` also distributes them to the pods.
    Gen {
        /// Machine names to generate for (bare names get the prefix). Omit + use --all.
        machines: Vec<String>,
        /// Generate for every current pod.
        #[arg(long)]
        all: bool,
        /// USD credit cap per key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After generating, copy the key(s) onto the pod(s) (runs copy-keys for them).
        #[arg(long)]
        copy: bool,
        /// Preview only: show what would be created, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// List the provisioned OpenRouter keys (name, USD limit, usage). By default shows
    /// only this iteration's keys (named `<MACHINE_NAME_PREFIX>-*`) and notes how many
    /// others were hidden; `--all` shows every key on the account.
    List {
        /// Show every key on the account, not just this iteration's `<prefix>-*` keys.
        #[arg(long)]
        all: bool,
    },
    /// Show the local OpenRouter keys file (path, whether it exists, and which pods have
    /// a key) — never prints the secret values.
    Which,
    /// Rotate a machine's key — delete it and generate a fresh one (e.g. after a leak).
    /// Updates the CSV; `--copy` re-pushes to the pod(s). One machine or --all.
    Rotate {
        /// Machine name (bare ok). Omit with --all.
        machine: Option<String>,
        /// Rotate every current pod's key.
        #[arg(long)]
        all: bool,
        /// USD credit cap for the new key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After rotating, copy the new key(s) onto the pod(s).
        #[arg(long)]
        copy: bool,
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Revoke (delete) a machine's key without regenerating. One machine or --all.
    Revoke {
        /// Machine name (bare ok). Omit with --all.
        machine: Option<String>,
        /// Revoke every current pod's key.
        #[arg(long)]
        all: bool,
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum CronCmd {
    /// Install/replace the arena backup cron job.
    Install {
        /// Cron schedule expression (default: every 15 minutes).
        #[arg(long, default_value = "*/15 * * * *")]
        schedule: String,
        /// Bake `ARENA_START_DATE=YYYY-MM-DD` into the cron line, so the scheduled
        /// backup computes the right wNdM label without it being in config.env
        /// (a crontab line doesn't inherit your shell environment).
        #[arg(long)]
        start_date: Option<String>,
        /// Also run `pods pull` (the rsync file backup) each tick, after the git backup.
        #[arg(long)]
        pull: bool,
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
    /// appends `KEY="value"`. Writes to the --config file; never echoes the value.
    ///
    /// Script-friendly: `config set KEY VALUE` sets it directly, or give just `KEY` and
    /// pipe the value on stdin (keeps secrets out of argv / `ps`):
    ///   printf %s "$TOKEN" | arena config set HF_TOKEN
    /// With no KEY at all it prompts interactively (pick a key — showing which are
    /// already set — then type the value); that path needs a terminal.
    #[command(verbatim_doc_comment)]
    Set {
        /// Config key, e.g. RUNPOD_API_KEY. Omit to choose interactively.
        key: Option<String>,
        /// Value to store (will be quoted). Omit to read from stdin / be prompted.
        value: Option<String>,
    },
    /// Show which config file is active (path + whether it's readable/writable), plus a
    /// summary of what's loaded and any keys currently coming from the environment.
    Which,
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
enum MigrateCmd {
    /// Build + set up `<name>-new` and sync `<name>`'s files onto it. Repeatable (an
    /// incremental rsync each run), and it never renames or touches the proxy — the
    /// participant keeps using `<name>` and can SSH the new pod directly to test it.
    Copy {
        /// Machine name (or id) to migrate.
        target: String,
        /// GPU type for the new pod (default: same as the source).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (default: same as the source).
        #[arg(long)]
        gpus: Option<u32>,
        /// Cloud tier COMMUNITY/SECURE (default: same as the source).
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
        /// Bring up sshd on boot via a start script — needed when `--image` is a bare
        /// (non-arena, non-RunPod) base image that doesn't already run sshd.
        #[arg(long)]
        bootstrap: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Cut over: final delta sync, swap names (`<name>`→`<name>-old`, `<name>-new`→`<name>`),
    /// re-point the proxy, then VERIFY the new pod is reachable through the proxy. If the
    /// verification fails it auto-reverts (names + proxy) so the participant stays on the
    /// original. `<name>-old` is kept (delete it later with `migrate finish`).
    Cutover {
        /// Machine name (or id) being migrated.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Don't touch the proxy at all (just swap names); you run `arena proxy apply`.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Delete the parked `<name>-old` pod (run once you've confirmed the new pod is good).
    Finish {
        /// Machine name (or id) that was migrated.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Roll a cutover back: swap `<name>`↔`<name>-old` and re-point the proxy to the original.
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
    /// Show the migration pair (`<name>`, `<name>-new`, `<name>-old`) and what each step does next.
    Status {
        /// Machine name (or id).
        target: String,
    },
}

#[derive(Subcommand)]
enum PodCmd {
    /// List current pods (read-only).
    List {
        /// Emit JSON instead of a table (for scripting).
        #[arg(long)]
        json: bool,
        /// Force the GPU-via-SSH probe (default for `--json`, which skips it otherwise).
        #[arg(long)]
        probe: bool,
        /// Skip the GPU-via-SSH probe (faster; GPU shows "-"). The table view probes
        /// by default since the provider list API omits GPU type.
        #[arg(long)]
        no_probe: bool,
    },
    /// Create pods — by name (`create apple bloom`), `-n` total, or `-a` add.
    Create {
        /// Explicit machine names to create (e.g. `apple bloom`). Bare names get the
        /// configured prefix. Mutually exclusive with -n/-a.
        names: Vec<String>,
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
        /// Docker image (overrides config IMAGE/RUNPOD_DOCKER_IMAGE).
        /// NOTE: for a custom (non-arena) image you probably want --bootstrap too,
        /// or the pod won't come up SSH-reachable.
        #[arg(long)]
        image: Option<String>,
        /// Set a start command that installs + launches sshd on boot, so a non-arena base
        /// image (e.g. an NVIDIA NGC image) is SSH-reachable. The prebuilt arena image
        /// doesn't need this.
        #[arg(long)]
        bootstrap: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
        /// Keep re-attempting (topping up to the target) for up to this many minutes,
        /// e.g. while waiting for capacity. 0 = a single attempt. Ctrl+C stops early.
        #[arg(long, default_value_t = 0)]
        retry_mins: u64,
        /// Seconds between retry rounds.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
    },
    /// Spin up: create pods, wait for SSH endpoints, then wire the proxy.
    Up {
        /// Explicit machine names to create (e.g. `apple bloom`), like `pods create`.
        /// Bare names get the configured prefix. Mutually exclusive with -n/-a.
        names: Vec<String>,
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
        /// Docker image (overrides config IMAGE/RUNPOD_DOCKER_IMAGE).
        /// NOTE: for a custom (non-arena) image you probably want --bootstrap too,
        /// or the pod won't come up SSH-reachable.
        #[arg(long)]
        image: Option<String>,
        /// Set a start command that installs + launches sshd on boot, so a non-arena base
        /// image (e.g. an NVIDIA NGC image) is SSH-reachable. The prebuilt arena image
        /// doesn't need this.
        #[arg(long)]
        bootstrap: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Don't poll after creating; just print ids (run `proxy plan` later).
        #[arg(long)]
        no_wait: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping.
        #[arg(long)]
        keep_trying: bool,
        /// Keep re-attempting (topping up to the target) for up to this many minutes
        /// before waiting for endpoints / deploying the proxy. 0 = a single attempt.
        /// Ctrl+C stops the retry loop early.
        #[arg(long, default_value_t = 0)]
        retry_mins: u64,
        /// Seconds between retry rounds.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
        /// Skip provisioning the new pods over SSH. By default `up` runs setup on the
        /// pods it creates (deploy key + repo + tokens, or the hetzner bare-VM script).
        #[arg(long)]
        no_setup: bool,
        /// Give up waiting for endpoints after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between readiness polls (one list call per poll, whole fleet).
        #[arg(long, default_value_t = 12)]
        interval: u64,
    },
    /// Provision pods over SSH. Acts by default; --dry-run to preview.
    ///
    /// Per pod, in order:
    ///   1. copy the git deploy key (scp) and chmod it;
    ///   2. add a github.com block to ~/.ssh/config pointing at that key;
    ///   3. add the shared + deploy public keys to ~/.ssh/authorized_keys;
    ///   4. point the ARENA repo's origin at GitHub, fetch, and update the branch
    ///      (stay on the current branch by default; --force checks out the default
    ///      branch and hard-resets);
    ///   5. update submodules;
    ///   6. write ~/.name (export MACHINE_NAME=…);
    ///   7. (optional) export any broadcast tokens that are set — Hugging Face
    ///      (HF_TOKEN + HUGGING_FACE_HUB_TOKEN) and Claude Code (CLAUDE_CODE_OAUTH_TOKEN)
    ///      — into ~/.bashrc & ~/.zshrc; tokens not set are skipped.
    ///
    /// It does NOT distribute per-host API keys (use `pods copy-keys`) or back anything
    /// up (use `pods backup` / `pods pull`).
    #[command(verbatim_doc_comment)]
    Setup {
        /// Pods to provision (machine names, e.g. `bulk` / `bulk apple` / `arena8-bulk`).
        /// Omit to provision the whole fleet.
        names: Vec<String>,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Force-checkout the default branch and hard-reset (else stay on current).
        #[arg(long)]
        force: bool,
        /// Hugging Face token to broadcast (overrides config HF_TOKEN).
        #[arg(long)]
        hf_token: Option<String>,
        /// Claude Code OAuth token to broadcast (overrides config CLAUDE_CODE_OAUTH_TOKEN).
        #[arg(long)]
        cc_token: Option<String>,
        /// Install the full shell the GPU pods get: zsh + oh-my-zsh + powerlevel10k + the
        /// shared arena-infra dotfiles, set as the login shell (no conda/ARENA_3.0). For
        /// bare / non-arena base images — the prebuilt arena image already has this.
        #[arg(long)]
        zsh_install: bool,
    },
    /// Stop a pod, or many with --all (+ --include/--exclude).
    Stop {
        /// Machine name (e.g. arena8-apple) or raw provider id. Omit with --all.
        target: Option<String>,
        /// Stop every running pod (subject to --include/--exclude).
        #[arg(long)]
        all: bool,
        /// With --all: only stop these names/ids (repeatable).
        #[arg(long)]
        include: Vec<String>,
        /// With --all: never stop these names/ids (repeatable).
        #[arg(long)]
        exclude: Vec<String>,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Restart a pod in place.
    Restart {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Replace a pod with a fresh one on a new host, keeping its name + files (blue-green).
    ///
    /// Unlike `restart` (same host — useless when the *host* is the problem), `replace`
    /// builds a brand-new pod, copies the old one's files onto it, then swaps it into the
    /// canonical machine name so its proxy port / ssh-config identity carry over. The old
    /// pod is parked as `<name>-old` until you confirm, never deleted out from under you.
    /// Same spec as the source by default; the flags below override for a spec migration.
    Replace {
        /// Machine name (e.g. arena8-apple) or raw provider id to replace.
        target: String,
        /// GPU type for the replacement (default: same as the source).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (default: same as the source).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod cloud tier COMMUNITY/SECURE (default: same as the source).
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
        /// Don't terminate the parked `<name>-old` pod at the end — leave it for manual
        /// teardown once you've confirmed the replacement is healthy.
        #[arg(long)]
        keep_old: bool,
        /// Skip the final `proxy apply` step. The swap still happens; you re-point nginx
        /// yourself with `arena proxy apply`. Useful when testing on a throwaway name, or
        /// to avoid rewriting the whole fleet's proxy config from a replace.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print the full plan, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Staged, low-downtime migration to a fresh machine (safer than one-shot `replace`).
    ///
    /// Three manual steps so the participant keeps working and can TEST the new pod before
    /// any cutover — and the proxy switch is verified + auto-reverts if it breaks:
    ///   migrate copy <name>     build+setup <name>-new and sync files (repeatable; no swap)
    ///   migrate cutover <name>  final delta sync, swap names + proxy, verify, revert if bad
    ///   migrate finish <name>   delete the parked <name>-old once you're happy
    ///   migrate revert <name>   roll a cutover back (swap names + proxy back to the original)
    ///   migrate status <name>   show the migration pair's state
    Migrate {
        #[command(subcommand)]
        cmd: MigrateCmd,
    },
    /// Terminate (delete) a pod, or the whole fleet with --all.
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
    /// Full backup: git-push each pod's current branch (skips main/master) AND rsync its
    /// home to the local backups folder (`pull`). One pod or all. --no-pull = git only.
    Backup {
        /// Machine name or id to back up. Omit to back up every reachable pod.
        target: Option<String>,
        /// Only do the git push; skip the rsync file backup.
        #[arg(long)]
        no_pull: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Commit message (default: "arena backup <machine>").
        #[arg(long)]
        message: Option<String>,
    },
    /// Rsync each pod's home to a local backup folder (the file backup).
    Pull {
        /// Backup label, e.g. `w1d3`. Defaults to the computed `wNdM` iteration.
        label: Option<String>,
        /// Local base directory for backups (default: config LOCAL_BACKUP_DIR, else ./backup).
        #[arg(long)]
        dir: Option<String>,
        /// Max file size for the dated `wNdM` snapshot tier (rsync --max-size), e.g. `50M`:
        /// files below it get point-in-time history. The `big/` mirror always holds the
        /// complete home regardless (default: config BACKUP_MAX_SIZE, else 50M).
        #[arg(long)]
        max_size: Option<String>,
        /// Remote path to pull, relative to the home dir (default: config
        /// BACKUP_REMOTE_PATH, else the whole home dir).
        #[arg(long)]
        remote_path: Option<String>,
        /// Exclude `.git` (by default the repo's git history/state IS backed up).
        #[arg(long)]
        no_git: bool,
        /// Skip the all-files big backup (`<dir>/big/<pod>/`); write only the dated snapshot
        /// tier. By default both run.
        #[arg(long)]
        no_big: bool,
        /// Preview only: print the rsync commands, copy nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Create + push each pod's wNdM autocommit branch (no commit).
    InitBranches {
        /// Override the iteration week (default: computed from ARENA_START_DATE).
        #[arg(long)]
        week: Option<u32>,
        /// Override the day-within-week (default: computed from ARENA_START_DATE).
        #[arg(long)]
        day: Option<u32>,
        /// Preview only: print the per-pod commands, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Switch a pod's branch (gentle ff-only; --hard force-resets to origin).
    SetBranch {
        /// Branch to check out (e.g. main, or a feature branch).
        branch: String,
        /// Machine name or id. Omit with --all.
        target: Option<String>,
        /// Apply to every pod with an SSH endpoint.
        #[arg(long)]
        all: bool,
        /// DESTRUCTIVE: hard-reset the branch to `origin/<branch>`, discarding local
        /// commits/changes (untracked files are left alone).
        #[arg(long)]
        hard: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Run a shell command on every pod over SSH (concurrent).
    ///
    /// Runs inside the pod's shell with its rc sourced (zsh + ~/.zshrc if present, else
    /// bash + ~/.bashrc, else sh) and the conda env active when conda exists (default
    /// `arena-env`, override via `CONDA_ENV`; set it empty to disable), so commands see
    /// the participants' python/packages and the token exports written by `setup`.
    Run {
        /// The command to run (everything after `run`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        /// Preview only: print the command + target pods, run nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Health check: torch version on every pod (read-only).
    Test,
    /// Distribute API keys to pods' shells (per-host CSVs + broadcast HF token).
    CopyKeys {
        /// Pod(s) to copy to (name or id) — a positional shorthand for --include. Omit to
        /// copy to every reachable pod. Merged with any --include values.
        target: Vec<String>,
        /// Directory holding the per-host `<provider>_api_keys.csv` files.
        #[arg(long, default_value = "./keys")]
        keys_dir: String,
        /// Hugging Face token to set on every pod (HF_TOKEN + HUGGING_FACE_HUB_TOKEN).
        /// Overrides config HF_TOKEN. Use to enable pulls from gated repos.
        #[arg(long)]
        hf_token: Option<String>,
        /// Claude Code OAuth token to set on every pod (CLAUDE_CODE_OAUTH_TOKEN).
        /// Overrides config CLAUDE_CODE_OAUTH_TOKEN.
        #[arg(long)]
        cc_token: Option<String>,
        /// Only copy to these pods (name or id, repeatable). Default: all reachable.
        #[arg(long)]
        include: Vec<String>,
        /// Never copy to these pods (name or id, repeatable).
        #[arg(long)]
        exclude: Vec<String>,
        /// Preview only: print what would be set per pod (key values redacted).
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// scp a local file (or dir, with -r) to every pod (mirrors the repo path if no DEST).
    #[command(visible_alias = "copy")]
    Cp {
        /// Local file (or directory, with -r) to copy.
        file: PathBuf,
        /// Destination path on each pod. Omit to mirror the repo path / land in `~`.
        dest: Option<String>,
        /// Recurse into a directory (scp -r).
        #[arg(short = 'r', long)]
        recursive: bool,
        /// Only copy to these pods (name or id, repeatable). Default: all reachable.
        #[arg(long)]
        include: Vec<String>,
        /// Never copy to these pods (name or id, repeatable).
        #[arg(long)]
        exclude: Vec<String>,
        /// Preview only: print the scp commands, copy nothing.
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

/// Count the pods belonging to this provider's cohort: its backend name *and* the
/// configured machine-name prefix. `pods` is the whole-fleet snapshot from `list_pods()`
/// (which spans every backend), so the provider filter is load-bearing — without it a
/// per-provider top-up gets sized against the entire fleet. (`up -a 1 --provider hetzner`
/// once tried to make ~23 pods because this very count wasn't provider-scoped.)
fn provider_pod_count(pods: &[arena_core::Pod], provider_name: &str, prefix: &str) -> usize {
    let pre = format!("{prefix}-");
    pods.iter()
        .filter(|p| p.provider.as_str() == provider_name && p.name.starts_with(&pre))
        .count()
}

/// The provider-scoped *total* a create aims for: `-n` is that total outright; `-a` adds
/// to what this provider already has. One definition, used for both preview and executor,
/// so they can never disagree about how big the create is.
fn target_total(want: Want, pods: &[arena_core::Pod], provider_name: &str, prefix: &str) -> usize {
    match want {
        Want::Total(n) => n,
        Want::Add(a) => provider_pod_count(pods, provider_name, prefix) + a,
    }
}

/// A resolved create plan: the provider-scoped total to reach, plus the concrete new names
/// to make this round (`target - have`, capped by free names). Built in ONE place
/// (`plan_create`) so the dry-run preview, the `[y/N]` confirmation, and the `[created] …`
/// lines are the *same* plan — not three independent recomputations that can drift apart
/// (which is exactly how a confirmed "1 pod" turned into a 23-pod create).
struct CreatePlan {
    /// Provider-scoped total we're topping up to (carried into the retry loop).
    target: usize,
    /// New names to create this round.
    names: Vec<String>,
}

/// Resolve a `-n`/`-a` request into a concrete [`CreatePlan`] against the current fleet.
/// Lists pods (failing closed — a failed list could otherwise duplicate pods) and picks
/// the next free machine names for the shortfall.
async fn plan_create(provider: &dyn Provider, cfg: &Config, want: Want) -> Result<CreatePlan> {
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context(
            "listing existing pods (refusing to allocate names — a failed list could \
             create duplicate pods)",
        )?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let have = provider_pod_count(&existing, provider.name(), prefix);
    let target = target_total(want, &existing, provider.name(), prefix);
    if let Want::Total(n) = want {
        if n <= have {
            eprintln!("already have {have} {} pod(s) (target {n}) — nothing to create", provider.name());
        }
    }
    let to_create = target.saturating_sub(have);
    // `existing` spans every provider, so a name live on another backend won't be reused.
    let names = arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, to_create);
    if names.len() < to_create {
        eprintln!(
            "warning: need {to_create} but only {} free machine name(s) available",
            names.len()
        );
    }
    Ok(CreatePlan { target, names })
}

/// Resolve operator-supplied machine names for `create <names…>`: prefix bare names
/// with `MACHINE_NAME_PREFIX`, warn about any not in the configured `MACHINE_NAME_LIST`
/// (allowed, but usually a typo), and drop any that already exist (so re-running is
/// safe). Propagates a list failure rather than risking a duplicate create.
async fn resolve_explicit_names(
    provider: &dyn Provider,
    cfg: &Config,
    raw: &[String],
) -> Result<Vec<String>> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context("listing existing pods (refusing to allocate names — a failed list could create duplicates)")?;
    // `existing` spans every provider, so the same name can't be live on two backends.
    let taken: std::collections::HashSet<&str> = existing.iter().map(|p| p.name.as_str()).collect();

    let mut out: Vec<String> = Vec::new();
    for n in raw {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        let full = arena_core::naming::canonical_name(prefix, &cfg.machine_names, n);
        // Membership check against the list: compare on the qualified name so absolute
        // (`@james-gpu` → `james-gpu`) and prefixed entries both match without false warnings.
        let known = cfg.machine_names.iter().any(|m| arena_core::naming::qualify(prefix, m) == full);
        if !known {
            eprintln!("warning: '{full}' is not in MACHINE_NAME_LIST (creating anyway)");
        }
        if taken.contains(full.as_str()) {
            eprintln!("skip {full} — already exists");
            continue;
        }
        if !out.contains(&full) {
            out.push(full);
        }
    }
    Ok(out)
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
/// Start-command script for `--bootstrap`: makes a non-arena base image (e.g. an NVIDIA
/// NGC image) SSH-reachable. Installs openssh-server, authorizes `$PUBLIC_KEY` (which
/// RunPod injects from the pod's env), generates host keys, and runs sshd in the
/// foreground so it's the long-lived process — if it dies, RunPod restarts the container.
/// `$PUBLIC_KEY` stays a runtime variable so it expands inside the container, not here.
/// Sent as `dockerStartCmd` argv `["bash","-c", THIS]` so no outer shell-quoting is needed.
/// The prebuilt arena image needs none of this, so `--bootstrap` is opt-in.
const BOOTSTRAP_SCRIPT: &str = r#"export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y --no-install-recommends openssh-server && mkdir -p ~/.ssh && echo "$PUBLIC_KEY" >> ~/.ssh/authorized_keys && chmod 700 ~/.ssh && ssh-keygen -A && mkdir -p /run/sshd && /usr/sbin/sshd -D"#;

#[derive(Debug, Clone, Default)]
struct SpecOverrides {
    gpu: Option<String>,
    gpus: Option<u32>,
    cloud: Option<String>,
    disk: Option<u32>,
    volume: Option<u32>,
    image: Option<String>,
    /// Set the `--bootstrap` start command (`dockerArgs`) so a non-arena base image
    /// brings up sshd on boot. Off => use the image's own entrypoint.
    bootstrap: bool,
}

/// The base spec from config, with any command-line overrides applied.
fn spec_with_overrides(cfg: &Config, ov: &SpecOverrides) -> PodSpec {
    let mut spec = PodSpec::from_config(cfg);
    apply_spec_overrides(&mut spec, ov);
    spec
}

/// Apply CLI `--gpu/--disk/...` overrides onto an existing spec base. Factored out so
/// `replace` can layer the same flags onto a *snapshot* of the source pod (rather than the
/// config base), giving "same spec as the source unless overridden".
fn apply_spec_overrides(spec: &mut PodSpec, ov: &SpecOverrides) {
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
    if let Some(img) = &ov.image {
        spec.image = img.clone();
    }
    if ov.bootstrap {
        spec.docker_args =
            Some(vec!["bash".to_string(), "-c".to_string(), BOOTSTRAP_SCRIPT.to_string()]);
    }
}

/// Top up toward the target, retrying for up to `retry_mins` (rounds every
/// `retry_secs`) while capacity is short — Ctrl+C stops the loop early and keeps what
/// was made. With `retry_mins == 0` it's a single attempt (honoring `keep_trying`).
/// Returns every pod created across all rounds.
async fn create_with_retry(
    provider: &dyn Provider,
    cfg: &Config,
    initial: Vec<String>,
    target: usize,
    ov: &SpecOverrides,
    keep_trying: bool,
    retry_mins: u64,
    retry_secs: u64,
) -> Result<Vec<arena_core::Pod>> {
    // Round 1 creates exactly the names that were previewed + confirmed, so the
    // `[created] …` output matches the `[y/N]` prompt. `target` (a provider-scoped total,
    // computed once in plan_create) bounds the whole operation: retries only ever re-plan
    // toward it, so a top-up can never balloon past what the operator agreed to.
    let retry_secs = retry_secs.max(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(retry_mins * 60);
    let mut all: Vec<arena_core::Pod> = Vec::new();
    let mut names = initial;
    let mut round = 0u32;
    loop {
        round += 1;
        if names.is_empty() {
            break; // target reached (or no free names left)
        }
        // In retry mode the *loop* is the keep-trying, so each round is one-shot.
        let kt = retry_mins == 0 && keep_trying;
        let made = create_pods(provider, cfg, &names, kt, ov).await?;
        let got = made.len();
        all.extend(made);
        if got == names.len() || retry_mins == 0 {
            break; // filled what this round needed, or no retry requested
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("retry window ({retry_mins}m) elapsed — have {} of {target}", all.len());
            break;
        }
        eprintln!(
            "round {round}: {} short of target {target} (capacity); retrying in {retry_secs}s (Ctrl+C to stop)…",
            names.len() - got
        );
        let interrupted = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(retry_secs)) => false,
            _ = tokio::signal::ctrl_c() => true,
        };
        if interrupted {
            eprintln!("interrupted — stopping retries with {} of {target}", all.len());
            break;
        }
        // Re-plan toward the SAME target so the next round only fills the shortfall.
        names = plan_create(provider, cfg, Want::Total(target)).await?.names;
    }
    Ok(all)
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
                            "[stop] no capacity for this GPU right now (created {}/{}). \
                             Re-run with --retry-mins N (or --keep-trying) to wait for it to free up.",
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
    // One resolver shared with arena-tui (flag > ARENA_CONFIG > prod copy > user config).
    let (config_path, config_source) = arena_core::config::resolve_config_path(cli.config.as_deref())?;
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;

    // `config check` must work even when a provider key is missing (that's what it's
    // for), so build the provider lazily — only for commands that actually talk to one.
    let provider = match cli.cmd {
        Cmd::Config(_) | Cmd::Cron(_) | Cmd::Tui | Cmd::Gpus => None,
        // Fleet-wide: every command spans all configured providers (create still targets
        // --provider). One configured backend behaves like that single provider.
        _ => Some(arena_core::provider::build_fleet(&cli.provider, &cfg, true)?),
    };

    match cli.cmd {
        Cmd::Tui => launch_tui(&cli.provider, &config_path),
        Cmd::Plan(c) => handle_plan(c, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Config(c) => handle_config(c, &cfg, &cli.provider, &config_path, config_source),
        Cmd::Cron(c) => handle_cron(c, &config_path).await,
        Cmd::Pods(p) => handle_pods(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::SshConfig { proxy, out } => {
            handle_ssh_config(provider.unwrap().as_ref(), &cfg, proxy, out.as_deref()).await
        }
        Cmd::Keys(k) => handle_keys(k, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::Gpus => handle_gpus(&cfg, &cli.provider).await,
    }
}

/// `arena gpus`: list the GPU types you can pass to `--gpu`. Fetches RunPod's **full,
/// live** catalog (via GraphQL) when on RunPod with a key; otherwise falls back to the
/// local curated presets. Prices come from the local presets where known.
async fn handle_gpus(cfg: &Config, provider_name: &str) -> Result<()> {
    use arena_core::gpu;

    let price = |api: &str| {
        gpu::find(api).map(|g| format!("${:.2}/${:.2}", g.community, g.secure)).unwrap_or_else(|| "—".into())
    };

    if provider_name == "runpod" {
        if let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) {
            match arena_core::provider::runpod::fetch_gpu_types(key).await {
                Ok(mut types) if !types.is_empty() => {
                    // Drop RunPod's "unknown" placeholder.
                    types.retain(|t| t.id != "unknown" && t.memory_gb > 0);
                    // RunPod's create-validation enum can be NARROWER than the gpuTypes
                    // catalog — listing a GPU that `--gpu` then gets a 400 for. Intersect
                    // with the creatable enum so we only advertise types that actually work.
                    // If the enum can't be fetched, fall back to showing all (with no claim).
                    let creatable = arena_core::provider::runpod::fetch_creatable_gpu_ids(key).await.ok().filter(|v| !v.is_empty());
                    let hidden = match &creatable {
                        Some(ok) => {
                            let before = types.len();
                            types.retain(|t| ok.contains(&t.id));
                            before - types.len()
                        }
                        None => 0,
                    };
                    types.sort_by(|a, b| a.memory_gb.cmp(&b.memory_gb).then(a.display_name.cmp(&b.display_name)));
                    println!("{:<16} {:>5}  {:>14}   {}", "GPU", "VRAM", "$/hr comm/sec", "API name (pass to --gpu)");
                    for t in &types {
                        println!("{:<16} {:>4}G  {:>14}   {}", t.display_name, t.memory_gb, price(&t.id), t.id);
                    }
                    println!(
                        "\n{} GPU types {}. Pass the API name (or a short alias like \
                         A4000 / 3090) to --gpu. Prices are rough preset rates where known.",
                        types.len(),
                        if creatable.is_some() { "(live from RunPod, creatable via the create API)" } else { "(live from RunPod)" }
                    );
                    if hidden > 0 {
                        println!(
                            "({hidden} more in RunPod's catalog are hidden — listed but rejected by the create API, so --gpu can't use them.)"
                        );
                    }
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => eprintln!("(couldn't fetch the live GPU list: {e} — showing local presets)\n"),
            }
        }
    }

    // Fallback: the curated presets (no network / non-RunPod).
    println!("{:<14} {:>5}  {:>9}  {:>8}   {}", "GPU", "VRAM", "$/hr comm", "$/hr sec", "RunPod API name (--gpu)");
    for g in gpu::PRESETS {
        println!(
            "{:<14} {:>4}G  {:>9}  {:>8}   {}",
            g.label, g.vram_gb, format!("${:.2}", g.community), format!("${:.2}", g.secure), g.api,
        );
    }
    println!(
        "\n(local presets — run with --provider runpod + a key for the full live list.)\n\
         Pass the API name, label, or alias (e.g. `A4000`, `3090`, \"A100 SXM\") to --gpu."
    );
    Ok(())
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

/// Bare-VM provisioning script for hetzner (CPU) pods — embedded so `arena pods setup`
/// can push + run it: system deps, docker + compose, a uv venv with the ARENA packages
/// (CPU substitutions), and a zsh rc that activates the venv. GPU pods come from a
/// prebuilt image and take the lighter post-image config path instead.
const HETZNER_SETUP: &str = include_str!("hetzner_setup.sh");

/// One step of provisioning a pod over SSH: push a file, or run a command.
#[derive(Debug, Clone, PartialEq)]
enum ProvisionStep {
    Scp { local: String, remote: String },
    Run { cmd: String },
}

/// The ordered provisioning steps for a pod, chosen by provider — pure and unit-tested,
/// so the executor never has to know what a provider *is*. Adding a backend = add its
/// steps here; the runner and the dry-run preview stay generic.
///   - bare-VM (hetzner): push the full setup script, then run it.
///   - image-based (runpod/vast): push the git deploy key, then the post-image config.
fn provisioning_steps(
    provider: &str,
    scfg: &arena_core::setup::SetupConfig,
    name: &str,
    force: bool,
    hetzner_script_local: &str,
) -> Vec<ProvisionStep> {
    match provider {
        "hetzner" => vec![
            // Copy the git deploy key first, so the script can clone (and later push to)
            // the PRIVATE cohort repo over SSH — not just the public mirror.
            ProvisionStep::Scp { local: scfg.key_local.clone(), remote: scfg.key_remote.clone() },
            ProvisionStep::Scp {
                local: hetzner_script_local.to_string(),
                remote: "/root/hetzner_setup.sh".into(),
            },
            ProvisionStep::Run {
                cmd: format!(
                    "REPO_URL={} REPO_DIR={} REPO_KEY={} bash /root/hetzner_setup.sh",
                    shell_quote(&scfg.repo_url),
                    shell_quote(&scfg.repo_path),
                    shell_quote(&scfg.key_remote),
                ),
            },
        ],
        _ => vec![
            ProvisionStep::Scp { local: scfg.key_local.clone(), remote: scfg.key_remote.clone() },
            ProvisionStep::Run { cmd: scfg.remote_command(name, force) },
        ],
    }
}

/// Run a pod's provisioning steps in order over SSH, stopping at the first failure.
/// Returns the last command's output (so the caller's ✓/✗ tally works) or an error.
async fn run_provisioning(
    target: &arena_core::ssh::SshTarget,
    steps: Vec<ProvisionStep>,
) -> arena_core::Result<arena_core::ssh::SshOutput> {
    use arena_core::ssh;
    let mut last = None;
    for step in steps {
        match step {
            ProvisionStep::Scp { local, remote } => {
                let out = ssh::scp(target, &local, &remote).await?;
                if !out.success {
                    return Err(arena_core::Error::provider(format!(
                        "scp {local} -> {remote} failed: {}",
                        out.stderr.trim()
                    )));
                }
                last = Some(out);
            }
            ProvisionStep::Run { cmd } => {
                let out = ssh::run(target, &cmd).await?;
                if !out.success {
                    return Ok(out); // command failed — surface it as a failed pod
                }
                last = Some(out);
            }
        }
    }
    last.ok_or_else(|| arena_core::Error::provider("no provisioning steps"))
}

/// True if an SSH error looks like a host that isn't reachable *yet* (worth waiting on a
/// freshly-booted VM) rather than a real provisioning failure (e.g. auth, or a script
/// error). Used to ride out the create-vs-sshd-up boot race.
fn is_connection_error(e: &arena_core::Error) -> bool {
    let s = e.to_string().to_lowercase();
    ["connect", "timed out", "connection closed", "refused", "no route", "unreachable"]
        .iter()
        .any(|m| s.contains(m))
}

async fn handle_setup(
    provider: &dyn Provider,
    cfg: &Config,
    apply: bool,
    force: bool,
    hf_token: Option<String>,
    cc_token: Option<String>,
    zsh_install: bool,
    // Restrict to these pod names (e.g. the ones `up` just created). None = whole fleet.
    only: Option<&[String]>,
) -> Result<()> {
    use arena_core::ssh::SshTarget;

    // Broadcast token values: CLI flags override config (so a token can be supplied
    // without editing the read-only prod config).
    let token_value = |k: &str| -> Option<String> {
        let flag = match k {
            "HF_TOKEN" => hf_token.clone(),
            "CLAUDE_CODE_OAUTH_TOKEN" => cc_token.clone(),
            _ => None,
        };
        flag.or_else(|| cfg.get(k).filter(|s| !s.is_empty()).map(String::from))
    };
    let mut scfg = arena_core::setup::SetupConfig::from_config(cfg)?;
    scfg.broadcast_exports = arena_core::apikeys::broadcast_env_vars(&token_value);
    scfg.zsh_install = zsh_install;
    // Report which broadcast tokens (HF, Claude Code) will be exported (optional step).
    let token_summary: Vec<String> = arena_core::apikeys::BROADCAST_TOKENS
        .iter()
        .map(|(key, display, _)| format!("{display} {}", if token_value(key).is_some() { "✓" } else { "✗" }))
        .collect();
    println!("Broadcast tokens: {} (✓ exported on each pod; ✗ skipped)", token_summary.join(", "));
    let pods = provider.list_pods().await.context("listing pods for setup")?;
    let mut targets = Vec::new();
    for pod in &pods {
        match SshTarget::from_pod(pod, cfg) {
            // Carry the provider so we can pick bare-VM vs image-based provisioning.
            Ok(t) => targets.push((pod.name.clone(), pod.provider.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    // Scope to a subset by name when asked (e.g. `up` provisions only what it created, or
    // the operator passed explicit names). Warn about any requested name that matched no
    // reachable pod (typo, terminated, or no SSH endpoint yet) instead of silently dropping
    // it; if NONE matched, fail loudly rather than provisioning zero pods.
    if let Some(only) = only {
        let reachable: std::collections::HashSet<&str> =
            targets.iter().map(|(n, _, _)| n.as_str()).collect();
        let missing: Vec<&str> =
            only.iter().map(String::as_str).filter(|n| !reachable.contains(n)).collect();
        if !missing.is_empty() {
            eprintln!("warning: no reachable pod matched: {}", missing.join(", "));
        }
        targets.retain(|(name, _, _)| only.iter().any(|n| n == name));
        if targets.is_empty() {
            anyhow::bail!(
                "no reachable pod matched {only:?} (check the name(s) against `arena pods list`)"
            );
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to set up)");
        return Ok(());
    }

    // Bare-VM (hetzner) pods scp this script; write it once to a temp file. Only stage it
    // when a hetzner pod is actually in the target set — a runpod-only setup (e.g. `replace`)
    // needs no hetzner script, and writing one is pure overhead. The filename carries the pid
    // so concurrent runs (or another user's run) never collide on a fixed path and hit EACCES
    // overwriting a file they don't own.
    let hetzner_script = if targets.iter().any(|(_, prov, _)| prov == "hetzner") {
        let p = std::env::temp_dir().join(format!("arena-hetzner-setup-{}.sh", std::process::id()));
        std::fs::write(&p, HETZNER_SETUP).context("writing hetzner setup script to a temp file")?;
        p.to_string_lossy().into_owned()
    } else {
        String::new()
    };

    if !apply {
        // Redact broadcast token values in the *previewed* command — the dry-run prints
        // the exact shell, and real token values must never land in a terminal/log. (The
        // actual run below uses `scfg` with real values and never prints the command.)
        let mut display_scfg = scfg.clone();
        for (name, value) in display_scfg.broadcast_exports.iter_mut() {
            *value = format!("<{name}>");
        }
        println!("Dry-run — would provision {} pod(s):\n", targets.len());
        for (name, provider_name, target) in &targets {
            println!("# {name}");
            for step in provisioning_steps(provider_name, &display_scfg, name, force, &hetzner_script) {
                match step {
                    ProvisionStep::Scp { local, remote } => println!("  {}", target.display_scp(&local, &remote)),
                    ProvisionStep::Run { cmd } => println!("  {}", target.display_command(&cmd)),
                }
            }
            println!();
        }
        let keys_present = arena_core::apikeys::PROVIDERS.iter().any(|(base, _, _)| {
            std::fs::read_to_string(format!("./keys/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        });
        let key_scope = match only {
            Some(names) if !names.is_empty() => names.join(", "),
            _ => "all reachable pods".to_string(),
        };
        println!(
            "API keys: {}",
            if keys_present { format!("found in ./keys — would distribute to {key_scope} after provisioning") } else { "none in ./keys — would skip".to_string() }
        );
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Provision concurrently across the fleet, printing a [done/total] line as each pod
    // finishes (scp+ssh is slow serially). The per-pod flow is *data* (provisioning_steps)
    // run by a generic runner — the executor below never names a provider.
    let total = targets.len();
    println!("Provisioning {total} pod(s) over SSH…");
    let mut set = tokio::task::JoinSet::new();
    for (name, provider_name, target) in targets {
        let steps = provisioning_steps(&provider_name, &scfg, &name, force, &hetzner_script);
        set.spawn(async move {
            // A just-created VM can report an SSH endpoint before sshd is up (hetzner
            // assigns the IP at create). Retry on connection errors for ~2.5 min to ride
            // out the boot race; real failures (auth, script errors) break immediately.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(150);
            let result = loop {
                match run_provisioning(&target, steps.clone()).await {
                    Err(e) if is_connection_error(&e) && std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                    }
                    other => break other,
                }
            };
            (name, result)
        });
    }

    let (mut ok, mut failed, mut done) = (0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, result)) = joined else { continue };
        match result {
            Ok(out) if out.success => {
                println!("[{done}/{total}] ✓ {name}");
                ok += 1;
            }
            Ok(out) => {
                println!("[{done}/{total}] ✗ {name} (exit {:?}): {}", out.code, out.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} provisioned, {failed} failed.");

    // Auto-handle API keys: if per-host CSVs have been generated, distribute them and say
    // so; otherwise report they're not set up (rather than silently doing nothing). HF is
    // already handled inline above. Best-effort — never fails the setup.
    let keys_dir = "./keys";
    let csv_sources: Vec<&str> = arena_core::apikeys::PROVIDERS
        .iter()
        .filter(|(base, _, _)| {
            std::fs::read_to_string(format!("{keys_dir}/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        })
        .map(|(_, display, _)| *display)
        .collect();
    if csv_sources.is_empty() {
        println!(
            "API keys: none generated in {keys_dir}/ — skipping (run `arena keys gen --all` \
             or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`)."
        );
    } else {
        // Scope key distribution to the SAME pods we just provisioned: when `setup` was
        // given explicit names, only those pods get keys — otherwise (whole-fleet setup)
        // `&[]` means all reachable. Without this, `setup <one-pod>` provisioned one pod
        // but copied keys to the entire fleet.
        let key_include: &[String] = only.unwrap_or(&[]);
        let scope_note = if key_include.is_empty() { "all reachable pods".to_string() } else { key_include.join(", ") };
        println!("API keys: found {} — distributing to {scope_note}…", csv_sources.join(", "));
        if let Err(e) = handle_copy_keys(provider, cfg, keys_dir, None, None, key_include, &[], false, true).await {
            eprintln!("  (API-key distribution failed: {e})");
        }
    }

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
        CronCmd::Install { schedule, start_date, pull } => {
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
            let exe = exe.display();
            let cfg_abs = cfg_abs.display();
            // `pods backup` now also rsyncs the home (the file backup); default the cron to
            // git-only (`--no-pull`) since it runs frequently, and let `--pull` opt into the
            // full backup each tick (rsync is incremental, so repeats only move deltas).
            let cmd = if pull {
                format!("{env_prefix}{exe} --config {cfg_abs} pods backup --yes")
            } else {
                format!("{env_prefix}{exe} --config {cfg_abs} pods backup --no-pull --yes")
            };
            let line = format!("{schedule} {cmd} >> {home}/arena-cron.log 2>&1");
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
    config_source: ConfigSource,
) -> Result<()> {
    match cmd {
        ConfigCmd::Check => config_check(cfg, provider_name),
        ConfigCmd::Which => config_which(cfg, provider_name, config_path, config_source),
        ConfigCmd::Set { key, value } => {
            let (key, value) = match (key, value) {
                (Some(k), Some(v)) => (k, v),
                // Key given but no value: read it from stdin when piped (script-friendly,
                // and keeps secrets out of argv/`ps`), or prompt at a terminal.
                (Some(k), None) => {
                    let v = read_value_for(&k)?;
                    (k, v)
                }
                // No key: pick interactively (needs a terminal).
                (None, _) => interactive_config_set(cfg)?,
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
/// records it in `missing` when it's required but absent/empty. An absent optional key
/// reads "not set" rather than "(missing)", which is reserved for required ones.
fn cfg_row(cfg: &Config, missing: &mut Vec<String>, key: &str, required: bool, secret: bool) {
    let ok = cfg.get(key).map(|v| !v.is_empty()).unwrap_or(false);
    let mark = if ok { "✓" } else if required { "✗" } else { "·" };
    let shown = if !ok {
        if required { "(missing)" } else { "not set" }.to_string()
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

/// Report the keys of an opt-in feature (proxy, git backups). When none are set, print a
/// single `·` "not configured" line naming what the feature is for, instead of a row of
/// "missing" keys that reads like a failure on a setup that simply doesn't use it. When
/// any are set, show every row so a half-configured feature shows what's absent. Never
/// counts toward `missing` (exit code), since the feature is optional.
fn cfg_optional_group(cfg: &Config, missing: &mut Vec<String>, keys: &[&str], needed_for: &str) {
    if keys.iter().all(|k| cfg.get(k).is_none_or(str::is_empty)) {
        println!("  · not configured (only needed for {needed_for})");
        return;
    }
    for k in keys {
        cfg_row(cfg, missing, k, false, false);
    }
}

/// The keys offered in the interactive `config set` picker, with whether each is a
/// secret (so we show "set" rather than the value). Order roughly by how often they're
/// set per run.
const SETTABLE_KEYS: &[(&str, bool)] = &[
    ("RUNPOD_API_KEY", true),
    ("VAST_API_KEY", true),
    ("HETZNER_API_KEY", true),
    ("HF_TOKEN", true), // broadcast to pods for gated repos (Llama 3 …)
    ("CLAUDE_CODE_OAUTH_TOKEN", true), // broadcast to pods for Claude Code access
    ("OPENROUTER_PROVISIONING_KEY", true), // mints per-machine OpenRouter keys (`arena keys`)
    ("ARENA_START_DATE", false),
    ("MACHINE_NAME_PREFIX", false),
    ("SHARED_SSH_KEY_PATH", false),
];

/// How a key currently reads for the picker: "not set", "set" (secret), or its value.
fn key_state(cfg: &Config, key: &str, secret: bool) -> String {
    match cfg.get(key).filter(|v| !v.is_empty()) {
        None => "· not set".to_string(),
        Some(_) if secret => "✓ set".to_string(),
        Some(v) => format!("✓ {v}"),
    }
}

/// Read the value for `key` when it wasn't given on the command line: from **stdin** if
/// it's piped (so scripts can do `printf %s "$TOKEN" | arena config set KEY` — the secret
/// never lands in argv / `ps` / shell history), or by prompting at a terminal. Trailing
/// newline is stripped; an empty value is an error.
fn read_value_for(key: &str) -> Result<String> {
    use std::io::{IsTerminal, Read, Write};
    if std::io::stdin().is_terminal() {
        eprint!("value for {key}: ");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        let s = s.trim().to_string();
        if s.is_empty() {
            anyhow::bail!("empty value — nothing set");
        }
        Ok(s)
    } else {
        // Piped: take all of stdin, trimming a trailing newline (the usual `echo`/here-string).
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading value from stdin")?;
        let s = s.trim_end_matches(['\n', '\r']).to_string();
        if s.is_empty() {
            anyhow::bail!(
                "no value for {key}: pass it as an argument or pipe it \
                 (e.g. `printf %s \"$VALUE\" | arena config set {key}`)"
            );
        }
        Ok(s)
    }
}

/// Interactively pick a config key and read its value (for `config set` with no args).
/// Shows which keys are already set (secrets as "set", others as their value) so the
/// operator can see at a glance what's configured. Requires a terminal.
fn interactive_config_set(cfg: &Config) -> Result<(String, String)> {
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

    eprintln!("Which key to set?  (✓ = already set in this config)\n");
    for (i, (k, secret)) in SETTABLE_KEYS.iter().enumerate() {
        eprintln!("  {:>2}) {k:<22} {}", i + 1, key_state(cfg, k, *secret));
    }
    eprintln!("  {:>2}) other (type the key name)", SETTABLE_KEYS.len() + 1);
    let choice = read("\n> ")?;
    let (key, secret) = match choice.parse::<usize>() {
        Ok(n) if (1..=SETTABLE_KEYS.len()).contains(&n) => {
            let (k, s) = SETTABLE_KEYS[n - 1];
            (k.to_string(), s)
        }
        Ok(n) if n == SETTABLE_KEYS.len() + 1 => (read("key name: ")?, true),
        _ => (choice, true), // a key name typed directly; treat as secret-ish
    };
    if key.is_empty() {
        anyhow::bail!("no key chosen");
    }
    // Flag an overwrite so it's never a surprise.
    if cfg.get(&key).map(|v| !v.is_empty()).unwrap_or(false) {
        eprintln!("({key} is already set — entering a value overwrites it; blank keeps it)");
    }
    let value = read(&format!("value for {key}: "))?;
    if value.is_empty() {
        anyhow::bail!("empty value — nothing set");
    }
    let _ = secret; // (reserved: could mask the input later)
    Ok((key, value))
}

/// `config which`: show the active config file (path, readable/writable), where that path
/// came from (`--config` / `ARENA_CONFIG` / which default), a one-line summary of what
/// parsed, and any keys currently being supplied by the environment (which silently
/// override the file) so it's clear where values are coming from.
fn config_which(
    cfg: &Config,
    provider_name: &str,
    config_path: &std::path::Path,
    source: ConfigSource,
) -> Result<()> {
    let abs = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let meta = std::fs::metadata(&abs).ok();
    let readable = std::fs::File::open(&abs).is_ok();
    let writable = meta
        .as_ref()
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false);

    println!("Active config: {}", abs.display());
    println!(
        "  {}  ·  {}",
        if readable { "✓ readable" } else { "✗ not readable" },
        if writable { "writable" } else { "read-only (config set can't write here)" },
    );
    // The source actually used (a set ARENA_CONFIG is shadowed by --config, so don't just
    // check the env var).
    println!("  source: {}", source.describe());
    println!("\nLoaded: provider {provider_name} · {} key(s) · {} machine name(s)",
        cfg.values.len(), cfg.machine_names.len());

    // Keys whose value is currently coming from the environment (env overrides the file
    // at load — easy to forget, so surface it).
    let mut from_env: Vec<&String> = cfg
        .values
        .keys()
        .filter(|k| std::env::var(k.as_str()).is_ok())
        .collect();
    from_env.sort();
    if from_env.is_empty() {
        println!("\nEnvironment overrides: none (all values come from the file)");
    } else {
        println!("\nEnvironment overrides in effect (env wins over the file):");
        for k in from_env {
            println!("  • {k}");
        }
    }
    Ok(())
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
    cfg_optional_group(
        cfg,
        &mut missing,
        &["SSH_PROXY_HOST", "SSH_PROXY_STARTING_PORT"],
        "`proxy` / `ssh-config --proxy`",
    );

    println!("\nBackup (git `backup` + file `pull`):");
    // ARENA_START_DATE = the wNdM label (init-branches / pull).
    cfg_optional_group(
        cfg,
        &mut missing,
        &["ARENA_REPO_NAME", "GIT_SSH_KEY_REMOTE", "ARENA_START_DATE"],
        "ARENA git backups: `pods backup` / `init-branches`",
    );
    // Where `pods pull` rsyncs pod files to locally (config LOCAL_BACKUP_DIR, else ./backup),
    // shown as an absolute path so it's obvious where backups land.
    let backup_dir = local_backup_dir(cfg);
    let abs = std::fs::canonicalize(&backup_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| {
            // Not created yet — resolve relative to cwd, dropping a leading "./".
            let rel = backup_dir.trim_start_matches("./");
            std::env::current_dir()
                .map(|d| d.join(rel).display().to_string())
                .unwrap_or_else(|_| backup_dir.clone())
        });
    let exists = std::path::Path::new(&backup_dir).is_dir();
    println!(
        "  {} local rsync backup dir     {abs}{}",
        if exists { "✓" } else { "·" },
        if exists { "" } else { "  (created on first `pods pull`)" }
    );
    // The `pods pull` knobs (all optional; shown so it's clear what's configurable).
    println!(
        "  · pull max file size         {} (BACKUP_MAX_SIZE)",
        cfg.get("BACKUP_MAX_SIZE").filter(|s| !s.is_empty()).unwrap_or("50M (default)")
    );
    println!(
        "  · pull remote source         {} (BACKUP_REMOTE_PATH)",
        cfg.get("BACKUP_REMOTE_PATH").filter(|s| !s.is_empty()).unwrap_or("~/ (home, default)")
    );

    println!("\nDashboard (optional):");
    cfg_row(cfg, &mut missing, "PROGRESS_CMD", false, false);

    println!("\nEvals / model access (optional, `pods copy-keys` / `arena keys`):");
    cfg_row(cfg, &mut missing, "HF_TOKEN", false, true); // broadcast for gated repos (Llama 3 …)
    cfg_row(cfg, &mut missing, "CLAUDE_CODE_OAUTH_TOKEN", false, true); // broadcast for Claude Code
    cfg_row(cfg, &mut missing, "OPENROUTER_PROVISIONING_KEY", false, true); // mints runtime keys
    cfg_row(cfg, &mut missing, "OPENROUTER_KEY_LIMIT", false, false); // USD cap per generated key

    // Read-only readiness: are the local files this user needs actually there/readable?
    // Answers "is it set up yet?" without touching any API.
    println!("\nSetup readiness (local, read-only):");
    let readable = |p: &str| std::fs::File::open(p).is_ok();
    if let Some(k) = cfg.get("SHARED_SSH_KEY_PATH") {
        // Resolve the same way pods/proxy do (readable ~/.ssh fallback).
        let resolved = arena_core::ssh::resolve_key_path(k);
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
    // Optional: only ARENA backup branches are labelled by it, so unset isn't a failure.
    println!(
        "  {} iteration start date       {}",
        if cfg.get("ARENA_START_DATE").is_some() { "✓" } else { "·" },
        cfg.get("ARENA_START_DATE")
            .unwrap_or("not set (only needed to label ARENA backup branches wNdM)")
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
    target_filter: Option<&str>,
) -> Result<()> {
    use arena_core::backup::{self, parse_backup_output};
    use arena_core::ssh::{self, SshTarget};

    // Backup commits the *current* branch (never switches/creates one), so it just needs
    // the repo path + push key — no week/day / autocommit-branch naming.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE").map(String::from);
    let msg_for = |name: &str| message.clone().unwrap_or_else(|| format!("arena backup {name}"));

    let pods = provider.list_pods().await.context("listing pods for backup")?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    // One pod (by name/id/bare name) if a target was given, else the whole fleet.
    let selected: Vec<&arena_core::Pod> = match target_filter {
        Some(t) => match pods.iter().find(|p| pod_matches(p, t, prefix)) {
            Some(p) => vec![p],
            None => anyhow::bail!("no pod with name or id '{t}' (run `arena pods list`)"),
        },
        None => pods.iter().collect(),
    };
    // Back up only pods that actually have an SSH endpoint; report the rest.
    let mut targets = Vec::new();
    for pod in selected {
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
        println!(
            "Dry-run — would commit + push the current branch on {} pod(s) (skips main/master):\n",
            targets.len()
        );
        for (name, target) in &targets {
            let cmd = backup::backup_command(&repo_path, key.as_deref(), &msg_for(name));
            println!("# {name}");
            println!("{}\n", target.display_command(&cmd));
        }
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Run concurrently with a live [done/total] counter (one SSH per pod, slow serially).
    let total = targets.len();
    println!("Backing up {total} pod(s) over SSH (current branch; main/master skipped)…");
    let mut set = tokio::task::JoinSet::new();
    for (name, target) in targets {
        let cmd = backup::backup_command(&repo_path, key.as_deref(), &msg_for(&name));
        set.spawn(async move { (name, ssh::run(&target, &cmd).await) });
    }
    let (mut backed_up, mut no_changes, mut skipped, mut failed, mut done) = (0, 0, 0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, res)) = joined else { continue };
        match res {
            Ok(out) if out.success => match parse_backup_output(&out.stdout) {
                Some((backup::BACKUP_PUSHED, branch)) => {
                    println!("[{done}/{total}] ✓ {name} -> {branch}");
                    backed_up += 1;
                }
                Some((backup::BACKUP_NO_CHANGES, branch)) => {
                    println!("[{done}/{total}] = {name} (no changes, on {branch})");
                    no_changes += 1;
                }
                Some((backup::BACKUP_SKIPPED, branch)) => {
                    println!("[{done}/{total}] ⊘ {name} (skipped — on protected branch {branch})");
                    skipped += 1;
                }
                _ => {
                    println!("[{done}/{total}] ✓ {name} (done)");
                    backed_up += 1;
                }
            },
            Ok(out) => {
                println!("[{done}/{total}] ✗ {name} (exit {:?}): {}", out.code, out.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {backed_up} pushed, {no_changes} unchanged, {skipped} skipped (main/master), {failed} failed.");
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

/// The proxy step for `pods up`: print a short forward summary, then — if nginx is
/// actually set up on the proxy host — deploy + reload it; otherwise just say how to
/// get the config (don't dump it). Never errors the spin-up: a proxy hiccup is reported,
/// not fatal.
async fn smart_proxy(cfg: &Config, pods: &[arena_core::Pod]) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let plan = arena_core::proxy::plan_forwards(&pxcfg, prefix, &cfg.machine_names, pods);
    let where_ = if pxcfg.local { "locally".to_string() } else { format!("{}@{}", pxcfg.proxy_user, pxcfg.proxy_host) };
    println!("\nproxy: {} forward(s) ({where_})", plan.forwards.len());

    // Is nginx present where we'd deploy — on this box (local) or the proxy host (SSH)?
    let has_nginx = if pxcfg.local {
        std::process::Command::new("sh")
            .args(["-c", "command -v nginx >/dev/null 2>&1"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        let target = SshTarget::for_host(
            &pxcfg.proxy_user,
            &pxcfg.proxy_host,
            22,
            cfg.get("SHARED_SSH_KEY_PATH"),
        );
        matches!(
            ssh::run(&target, "command -v nginx >/dev/null 2>&1 && echo yes").await,
            Ok(out) if out.success && out.stdout.contains("yes")
        )
    };

    if has_nginx {
        // nginx is set up — update it. (Part of the already-confirmed `up` flow.)
        if let Err(e) = deploy_proxy(cfg, pods, true).await {
            eprintln!("proxy update failed (pods are up): {e}");
        }
    } else if pxcfg.local {
        println!(
            "nginx not found on this host — not deploying. Install nginx (or run \
             `arena proxy plan` to print the config), then `arena proxy apply`."
        );
    } else {
        println!(
            "proxy host {} has no nginx (or is unreachable) — not deploying. Run \
             `arena proxy plan` to print/save the config, or `arena proxy apply` once \
             nginx is set up. (If this box IS the proxy, unset PROXY_LOCAL / set it true.)",
            pxcfg.proxy_host
        );
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
    let reload = "nginx -t && nginx -s reload";
    // One-line summary of pods still without an endpoint (instead of a line each).
    let starting = if plan.skipped.is_empty() {
        String::new()
    } else {
        format!(" ({} still starting)", plan.skipped.len())
    };

    if !apply {
        let dest = if pxcfg.local { "this host".into() } else { format!("{}@{}", pxcfg.proxy_user, pxcfg.proxy_host) };
        println!("[dry-run] would deploy {} forward(s) to {dest}:{}{starting}", plan.forwards.len(), pxcfg.nginx_path);
        if pxcfg.local {
            println!("  write {} + run `{reload}` locally", expand_tilde(&pxcfg.nginx_path));
        } else {
            let target = SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
            println!("  {}", target.display_scp("<rendered nginx>", &pxcfg.nginx_path));
            println!("  {}", target.display_command(reload));
        }
        println!("(preview only — run without --dry-run to deploy and reload nginx)");
        return Ok(());
    }

    // Local: this box IS the proxy — write the config + reload nginx directly, no SSH.
    if pxcfg.local {
        let path = expand_tilde(&pxcfg.nginx_path);
        // Idempotent: skip the write+reload if the live config already matches.
        if std::fs::read_to_string(&path).map(|c| c == nginx).unwrap_or(false) {
            return Ok(());
        }
        std::fs::write(&path, &nginx).with_context(|| format!("writing nginx config to {path}"))?;
        let out = std::process::Command::new("sh")
            .args(["-c", reload])
            .output()
            .context("reloading nginx locally")?;
        if out.status.success() {
            println!("[proxy] deployed {} forward(s) locally and reloaded nginx{starting}", plan.forwards.len());
            return Ok(());
        }
        anyhow::bail!("local nginx reload failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }

    // Remote: SSH to the proxy host. Idempotent — only scp + reload on a real change.
    let target =
        SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let current = ssh::run(&target, &format!("cat {} 2>/dev/null", pxcfg.nginx_path)).await;
    if let Ok(out) = &current {
        if out.success && out.stdout == nginx {
            return Ok(());
        }
    }
    let tmp = std::env::temp_dir().join("arena-proxy.conf");
    std::fs::write(&tmp, &nginx).context("writing rendered nginx config to a temp file")?;
    let scp = ssh::scp(&target, &tmp.to_string_lossy(), &pxcfg.nginx_path).await?;
    if !scp.success {
        anyhow::bail!("scp to proxy {} failed: {}", pxcfg.proxy_host, scp.stderr.trim());
    }
    let out = ssh::run(&target, reload).await?;
    if out.success {
        println!("[proxy] deployed {} forward(s) to {} and reloaded nginx{starting}", plan.forwards.len(), pxcfg.proxy_host);
        Ok(())
    } else {
        anyhow::bail!("nginx reload on {} failed (exit {:?}): {}", pxcfg.proxy_host, out.code, out.stderr.trim())
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
        PodCmd::List { json, probe, no_probe } => {
            // Fleet view across every configured provider (the aggregate provider), so
            // e.g. hetzner CPU pods show up alongside the GPU fleet. Grouped by provider.
            let mut pods = provider.list_pods().await?;
            pods.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
            // Probe GPU by default for the human table (the list API omits GPU type);
            // JSON stays fast/scriptable unless asked. `--no-probe` always wins.
            let probe = !no_probe && (probe || !json);
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
                "{:<22} {:<8} {:<14} {:<10} {:<16} {:<16} {}",
                "NAME", "PROVIDER", "ID", "STATUS", "GPU", "IP", "PORT"
            );
            for p in &pods {
                println!(
                    "{:<22} {:<8} {:<14} {:<10} {:<16} {:<16} {}",
                    p.name,
                    p.provider,
                    p.id,
                    arena_core::status::short_status(&p.status),
                    p.gpu_type.as_deref().unwrap_or("-"),
                    p.ssh_ip.as_deref().unwrap_or("-"),
                    p.ssh_port.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }

        PodCmd::Create { names, count, add, gpu, gpus, cloud, disk, volume, image, bootstrap, dry_run, keep_trying, retry_mins, retry_secs } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
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
            let spec = spec_with_overrides(cfg, &ov);
            if dry_run {
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                println!("\nDry-run only — no pods created (this is a preview).");
                return Ok(());
            }
            if !confirm(yes, &format!(
                "Will create {} pod(s) on {} ({}):\n  {}",
                names.len(), provider.name(), provider.describe(&spec), names.join(", ")
            ))? {
                println!("aborted.");
                return Ok(());
            }
            // Explicit names: create them directly. -n/-a: use the top-up retry loop.
            if count.is_none() && add.is_none() {
                create_pods(provider, cfg, &names, keep_trying, &ov).await?;
            } else {
                create_with_retry(provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs).await?;
            }
        }

        PodCmd::Up { names, count, add, gpu, gpus, cloud, disk, volume, image, bootstrap, dry_run, no_wait, keep_trying, retry_mins, retry_secs, no_setup, timeout, interval } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
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

            let spec = spec_with_overrides(cfg, &ov);
            if dry_run {
                let desc = provider.describe(&spec);
                for name in &names {
                    println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                }
                warn_no_volume(provider, &spec);
                let extra = if no_setup { "" } else { " then provision them," };
                let retry = if retry_mins > 0 { format!(" (retrying up to {retry_mins}m for capacity)") } else { String::new() };
                println!(
                    "\nDry-run only — no pods created (preview){retry}: would create the above, \
                     poll up to {timeout}s for SSH endpoints,{extra} then update the proxy (if nginx is set up)."
                );
                return Ok(());
            }

            if !confirm(yes, &format!(
                "Will create {} pod(s) on {} ({}){}, wait for endpoints{}, then update the proxy if nginx is set up:\n  {}",
                names.len(),
                provider.name(),
                provider.describe(&spec),
                if retry_mins > 0 { format!(", retrying up to {retry_mins}m") } else { String::new() },
                if no_setup { "" } else { ", provision them" },
                names.join(", ")
            ))? {
                println!("aborted.");
                return Ok(());
            }
            // Create as many as capacity allows (retrying if requested); only wait on
            // the ones we got. Proxy is deployed *after* this returns — i.e. once the
            // retry loop has finished topping up. Explicit names create directly.
            let created = if explicit {
                create_pods(provider, cfg, &names, keep_trying, &ov).await?
            } else {
                create_with_retry(provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs).await?
            };
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

            // Is nginx set up on the proxy host? (Decides deploy-as-they-come vs.
            // just instructing at the end — checked once up front.)
            let nginx_present = match arena_core::proxy::ProxyConfig::from_config(cfg) {
                Ok(px) => {
                    let tgt = arena_core::ssh::SshTarget::for_host(
                        &px.proxy_user, &px.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
                    matches!(
                        arena_core::ssh::run(&tgt, "command -v nginx >/dev/null 2>&1 && echo yes").await,
                        Ok(o) if o.success && o.stdout.contains("yes")
                    )
                }
                Err(_) => false,
            };

            // Poll until our pods have SSH endpoints or we hit the timeout, updating
            // nginx as endpoints appear (idempotent — reloads only on a real change).
            // Ctrl+C stops the wait early. Stateless: each tick re-reads truth.
            let policy = arena_core::retry::RetryPolicy::default();
            let interval = interval.max(1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            let is_ready = |p: &arena_core::Pod| p.ssh_ip.is_some() && p.ssh_port.is_some();
            println!(
                "\nWaiting up to {timeout}s for SSH endpoints{} (Ctrl+C to stop)…",
                if nginx_present { ", updating nginx as they come up" } else { "" }
            );
            let pods = loop {
                let pods = arena_core::retry::retrying(&policy, || provider.list_pods())
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("  poll failed ({e}); retrying");
                        Vec::new()
                    });
                let ready = pods.iter().filter(|p| want_ids.contains(&p.id) && is_ready(p)).count();
                println!("  {ready}/{} ready", want_ids.len());
                if nginx_present && !pods.is_empty() {
                    if let Err(e) = deploy_proxy(cfg, &pods, true).await {
                        eprintln!("  proxy update failed: {e}");
                    }
                }
                if ready == want_ids.len() || std::time::Instant::now() >= deadline {
                    break pods;
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("interrupted — stopping wait");
                        break pods;
                    }
                }
            };

            let not_ready: Vec<&str> = created
                .iter()
                .filter(|c| !pods.iter().any(|p| p.id == c.id && is_ready(p)))
                .map(|c| c.name.as_str())
                .collect();
            if !not_ready.is_empty() {
                eprintln!(
                    "{} pod(s) still without an endpoint: {} — re-run `arena proxy apply` once they're up.",
                    not_ready.len(),
                    not_ready.join(", ")
                );
            }
            // Provision the pods we just created over SSH (deploy key + repo + tokens, or
            // the bare-VM script for hetzner), unless --no-setup. Scoped to the new pods so
            // a top-up `up` doesn't re-provision the whole fleet.
            if !no_setup {
                println!("\nProvisioning the new pod(s) over SSH…");
                let new: Vec<String> = created.iter().map(|p| p.name.clone()).collect();
                handle_setup(provider, cfg, true, false, None, None, false, Some(&new)).await?;
            }
            // If there's no nginx to deploy to, say how to wire it (don't dump config).
            if !nginx_present {
                smart_proxy(cfg, &pods).await?;
            }
        }

        PodCmd::Stop { target, all, include, exclude, dry_run } => {
            match (all, target) {
                (false, Some(target)) => {
                    let (owner, id, label) = resolve_target_any(cfg, &target).await?;
                    if dry_run {
                        println!("[dry-run] would stop {label}");
                    } else {
                        if !confirm(yes, &format!("Will stop {label}."))? {
                            println!("aborted.");
                            return Ok(());
                        }
                        owner.stop_pod(&id).await?;
                        println!("[stopped] {label}");
                    }
                }
                (true, _) => {
                    let pods = select_pods(provider, cfg, &include, &exclude, Some("RUNNING")).await?;
                    if pods.is_empty() {
                        println!("(no running pods to stop)");
                        return Ok(());
                    }
                    if dry_run {
                        for p in &pods {
                            println!("[dry-run] would stop {} (id={})", p.name, p.id);
                        }
                        println!("\nDry-run only — would stop {} pod(s).", pods.len());
                        return Ok(());
                    }
                    if !confirm(yes, &format!("Will stop {} running pod(s).", pods.len()))? {
                        println!("aborted.");
                        return Ok(());
                    }
                    let (mut ok, total) = (0, pods.len());
                    for p in &pods {
                        match provider.stop_pod(&p.id).await {
                            Ok(()) => {
                                println!("[stopped] {}", p.name);
                                ok += 1;
                            }
                            Err(e) => eprintln!("[FAILED] {}: {e}", p.name),
                        }
                    }
                    println!("\nstopped {ok}/{total}");
                    if ok < total {
                        anyhow::bail!("{} pod(s) failed to stop", total - ok);
                    }
                }
                (false, None) => anyhow::bail!("specify a pod (name or id) to stop, or pass --all"),
            }
        }


        PodCmd::InitBranches { week, day, dry_run } => {
            handle_init_branches(provider, cfg, week, day, dry_run, yes).await?;
        }

        PodCmd::Pull { label, dir, max_size, remote_path, no_git, no_big, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            handle_pull(provider, cfg, label, &dir, max_size, remote_path, no_git, no_big, None, dry_run, yes).await?;
        }

        PodCmd::CopyKeys { target, keys_dir, hf_token, cc_token, include, exclude, dry_run } => {
            // Positional targets are a friendlier spelling of --include; merge the two.
            let include: Vec<String> = include.into_iter().chain(target).collect();
            handle_copy_keys(provider, cfg, &keys_dir, hf_token, cc_token, &include, &exclude, dry_run, yes).await?;
        }

        PodCmd::Cp { file, dest, recursive, include, exclude, dry_run } => {
            handle_copy(provider, cfg, &file, dest.as_deref(), recursive, &include, &exclude, dry_run, yes).await?;
        }

        PodCmd::Restart { target, dry_run } => {
            let (owner, id, label) = resolve_target_any(cfg, &target).await?;
            if dry_run {
                println!("[dry-run] would restart {label}");
            } else {
                if !confirm(yes, &format!("Will restart {label}."))? {
                    println!("aborted.");
                    return Ok(());
                }
                owner.restart_pod(&id).await?;
                println!("[restarted] {label}");
            }
        }

        PodCmd::Replace { target, gpu, gpus, cloud, disk, volume, image, keep_old, skip_proxy, dry_run } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap: false };
            handle_replace(cfg, &target, &ov, keep_old, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Migrate { cmd } => match cmd {
            MigrateCmd::Copy { target, gpu, gpus, cloud, disk, volume, image, bootstrap, dry_run } => {
                let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
                handle_migrate_copy(cfg, &target, &ov, dry_run, yes).await?;
            }
            MigrateCmd::Cutover { target, yes: y, skip_proxy, dry_run } => {
                handle_migrate_cutover(cfg, &target, skip_proxy, dry_run, yes || y).await?;
            }
            MigrateCmd::Finish { target, yes: y, dry_run } => {
                handle_migrate_finish(cfg, &target, dry_run, yes || y).await?;
            }
            MigrateCmd::Revert { target, yes: y, skip_proxy, dry_run } => {
                handle_migrate_revert(cfg, &target, skip_proxy, dry_run, yes || y).await?;
            }
            MigrateCmd::Status { target } => {
                handle_migrate_status(cfg, &target).await?;
            }
        },

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
                let (owner, id, label) = resolve_target_any(cfg, &target).await?;
                if dry_run {
                    println!("[dry-run] would terminate {label}");
                } else {
                    if !confirm(yes, &format!("Will TERMINATE {label} — irreversible."))? {
                        println!("aborted.");
                        return Ok(());
                    }
                    owner.terminate_pod(&id).await?;
                    println!("[terminated] {label}");
                }
            }
            (false, None) => {
                anyhow::bail!("specify a pod (name or id) to terminate, or pass --all");
            }
        },

        PodCmd::Backup { target, no_pull, dry_run, message } => {
            let scope = match &target {
                Some(t) => format!("{t}'s ARENA tree"),
                None => "each pod's ARENA tree".to_string(),
            };
            let what = if no_pull {
                format!("commit + push {scope} on its current branch (main/master skipped)")
            } else {
                // Show the real destination subfolder (base/<wNdM>/<pod>) so it's clear where
                // the rsync lands; fall back to just the base if the wNdM label can't be
                // computed (e.g. ARENA_START_DATE unset).
                let base = local_backup_dir(cfg);
                let dest = match resolve_week_day(cfg, None, None) {
                    Ok((w, d)) => format!("{base}/w{w}d{d}/<pod> (snapshot) + {base}/big/<pod> (all files)"),
                    Err(_) => base,
                };
                format!("commit + push {scope} (git), then rsync the home(s) to {dest}")
            };
            if !dry_run && !confirm(yes, &format!("Will {what}."))? {
                println!("aborted.");
                return Ok(());
            }
            // 1) git push, then 2) rsync file backup (unless --no-pull). The file backup is
            // INDEPENDENT of git, so a git failure on one pod (e.g. a missing repo, or a pod
            // sitting on main) must NOT skip the rsync for the whole fleet. Capture the git
            // result, always run the pull, then surface the git error at the end.
            let git_result = handle_backup(provider, cfg, !dry_run, message, target.as_deref()).await;
            if !no_pull {
                println!();
                let dir = local_backup_dir(cfg);
                handle_pull(provider, cfg, None, &dir, None, None, false, false, target.as_deref(), dry_run, true).await?;
            }
            git_result?;
        }
        PodCmd::Setup { names, dry_run, force, hf_token, cc_token, zsh_install } => {
            // Normalize bare names to full ones (`bulk` -> `arena8-bulk`); empty = whole fleet.
            let only: Option<Vec<String>> = if names.is_empty() {
                None
            } else {
                let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
                Some(
                    names
                        .iter()
                        .map(|n| arena_core::naming::canonical_name(prefix, &cfg.machine_names, n))
                        .collect(),
                )
            };
            let scope = match &only {
                Some(v) => format!("{} pod(s) ({})", v.len(), v.join(", ")),
                None => "each pod".to_string(),
            };
            if !dry_run
                && !confirm(yes, &format!("Will provision {scope} over SSH (deploy key, ~/.name, repo)."))?
            {
                println!("aborted.");
                return Ok(());
            }
            handle_setup(provider, cfg, !dry_run, force, hf_token, cc_token, zsh_install, only.as_deref()).await?;
        }
        PodCmd::SetBranch { branch, target, all, hard, dry_run } => {
            handle_set_branch(provider, cfg, &branch, target.as_deref(), all, hard, dry_run, yes).await?;
        }
        PodCmd::Run { command, dry_run } => {
            let cmd = command.join(" ");
            handle_run(provider, cfg, &cmd, dry_run, yes, /*confirm*/ true, /*compact*/ false).await?;
        }
        PodCmd::Test => {
            // Read-only: no confirm, compact one-line-per-pod output.
            handle_run(
                provider,
                cfg,
                "python -c 'import torch; print(torch.__version__)' 2>&1 || python3 -c 'import torch; print(torch.__version__)'",
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

/// Run `cmd` on every pod with an SSH endpoint, concurrently. `compact` prints one line
/// per pod (last stdout line); otherwise a per-pod block. `confirm_needed` gates it
/// behind the y/N prompt (arbitrary exec); read-only checks pass false.
async fn handle_run(
    provider: &dyn Provider,
    cfg: &Config,
    cmd: &str,
    dry_run: bool,
    yes: bool,
    confirm_needed: bool,
    compact: bool,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let pods = provider.list_pods().await.context("listing pods")?;
    let mut targets: Vec<(String, SshTarget)> = Vec::new();
    for pod in &pods {
        // Report (don't silently drop) pods we can't reach yet, so a "run on every pod"
        // can't quietly skip part of the fleet while reporting success.
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint)");
        return Ok(());
    }

    // Source the participants' rc (~/.zshrc, else ~/.bashrc) and activate the conda env,
    // so commands see the participants' `arena-env` (python/packages) and the token
    // exports from setup. Activation is skipped on the pod when there's no conda, so the
    // ARENA default is harmless elsewhere; `CONDA_ENV=""` in config disables it outright
    // (still sources the rc for tokens).
    let conda_env = cfg.get("CONDA_ENV").unwrap_or("arena-env");
    let remote = ssh::login_shell_wrap(cmd, Some(conda_env));

    if dry_run {
        println!("[dry-run] would run on {} pod(s):\n  {remote}", targets.len());
        return Ok(());
    }
    if confirm_needed && !confirm(yes, &format!("Run `{cmd}` on {} pod(s) over SSH.", targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let mut set = tokio::task::JoinSet::new();
    for (name, t) in targets {
        let remote = remote.clone();
        set.spawn(async move { (name, ssh::run(&t, &remote).await) });
    }
    let mut results: Vec<(String, String, bool)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((name, res)) => {
                // a no-PTY shell can emit harmless job-control chatter; strip it.
                let (text, ok) = match res {
                    Ok(out) if out.success => {
                        (ssh::strip_interactive_noise(out.stdout.trim()).trim().to_string(), true)
                    }
                    Ok(out) => (
                        format!(
                            "exit {:?}: {}",
                            out.code,
                            ssh::strip_interactive_noise(out.stderr.trim()).trim()
                        ),
                        false,
                    ),
                    Err(e) => (e.to_string(), false),
                };
                results.push((name, text, ok));
            }
            // A panicked/cancelled task must count as a failure, not vanish from the tally.
            Err(e) => results.push(("?".to_string(), format!("task did not complete: {e}"), false)),
        }
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let (mut ok, mut bad) = (0, 0);
    for (name, text, success) in &results {
        if *success { ok += 1 } else { bad += 1 }
        if compact {
            let line = text.lines().last().unwrap_or("").trim();
            let shown = if *success { line.to_string() } else { format!("✗ {line}") };
            println!("{name:<22} {shown}");
        } else {
            println!("\n── {name} {}", if *success { "" } else { "(FAILED)" });
            println!("{text}");
        }
    }
    println!("\n{ok} ok, {bad} failed");
    // Propagate partial failure to the exit code, like the mutating sibling handlers — so
    // a scripted `pods test` / `pods run` can't pass while the command failed on pods.
    if bad > 0 {
        anyhow::bail!("{bad} pod(s) failed");
    }
    Ok(())
}

/// Switch one pod (or, with `all`, every pod with an SSH endpoint) to `branch` over SSH.
/// Gentle (fetch+checkout+ff-pull) by default; `hard` force-resets to `origin/<branch>`,
/// discarding local commits/changes. Dry-run prints the exact command per pod.
#[allow(clippy::too_many_arguments)]
async fn handle_set_branch(
    provider: &dyn Provider,
    cfg: &Config,
    branch: &str,
    target: Option<&str>,
    all: bool,
    hard: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE");
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, key, hard);

    // Which pods: --all (every reachable one) or a single resolved target.
    let pods = provider.list_pods().await.context("listing pods")?;
    let mut targets: Vec<(String, SshTarget)> = Vec::new();
    if all {
        for pod in &pods {
            // Report unreachable pods — a silently-skipped pod left on a stale/diverged
            // branch (especially under --hard) is exactly what we don't want.
            match SshTarget::from_pod(pod, cfg) {
                Ok(t) => targets.push((pod.name.clone(), t)),
                Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
            }
        }
    } else if let Some(want) = target {
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
        match pods.iter().find(|p| pod_matches(p, want, prefix)) {
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
    let how = if hard {
        format!("HARD-reset {} pod(s) to 'origin/{branch}' — DISCARDS local commits/changes", targets.len())
    } else {
        format!("switch {} pod(s) to branch '{branch}' (gentle, no reset)", targets.len())
    };
    if !confirm(yes, &format!("Will {how}."))? {
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

/// Does `token` identify `pod`? Matches the full name, the provider id, or a **bare
/// short name** (`zebra` ⇒ `<prefix>-zebra`), so targets/filters accept either form.
fn pod_matches(pod: &arena_core::Pod, token: &str, prefix: &str) -> bool {
    pod.name == token || pod.id == token || pod.name == format!("{prefix}-{token}")
}

/// Derive the staging + parked names for a blue-green replace of `canonical`. The
/// replacement is built under `<canonical>-new`; at the swap the source is parked at
/// `<canonical>-old` and the replacement is renamed onto `canonical` itself — so the
/// proxy port / ssh-config identity (which key off the canonical name) carry straight
/// over. Pure so the naming contract is unit-tested.
fn replace_stage_names(canonical: &str) -> (String, String) {
    (format!("{canonical}-new"), format!("{canonical}-old"))
}

/// Build the spec for a replacement pod: snapshot the source's spec, fall GPU/cloud/image
/// back to config (the REST API can't report GPU type or cloud tier — empty `machine`
/// object), re-seed the shared SSH key, then apply CLI overrides. disk/volume/ports/env come
/// from the snapshot so per-pod differences are preserved. Errors if no GPU type is known.
async fn build_replacement_spec(
    cfg: &Config,
    owner: &dyn Provider,
    src_id: &str,
    src_label: &str,
    ov: &SpecOverrides,
) -> Result<PodSpec> {
    let mut spec = owner
        .pod_spec(src_id)
        .await
        .with_context(|| format!("snapshotting spec of {src_label}"))?;
    let base = PodSpec::from_config(cfg);
    if spec.image.is_empty() {
        spec.image = base.image;
    }
    if spec.gpu_type.is_empty() {
        spec.gpu_type = base.gpu_type;
    }
    if spec.cloud_type.is_empty() {
        spec.cloud_type = base.cloud_type;
    }
    let pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    if !pubkeys.is_empty() {
        spec.env.retain(|(k, _)| k != "PUBLIC_KEY");
        spec.env.push(("PUBLIC_KEY".to_string(), pubkeys.join("\n")));
    }
    apply_spec_overrides(&mut spec, ov);
    if spec.gpu_type.is_empty() {
        anyhow::bail!(
            "couldn't determine a GPU type for the replacement (the API doesn't report it and \
             no GPU_TYPE is configured) — pass --gpu <type> (see `arena gpus`)"
        );
    }
    Ok(spec)
}

/// Resolve a migration's canonical name + the owning provider, rejecting a `-new`/`-old`
/// staging name (the user must pass the live machine name). Returns (owner, canonical, pods).
async fn resolve_migration(
    cfg: &Config,
    target: &str,
) -> Result<(Box<dyn Provider>, String, Vec<arena_core::Pod>)> {
    let (owner, src_id, src_label) = resolve_target_any(cfg, target).await?;
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
    Ok((owner, canonical, pods))
}

/// Can we reach pod `expected_id` *through the proxy*? Computes the machine's stable proxy
/// port (`starting_port + its index in MACHINE_NAME_LIST`), SSHes to `proxy_host:port`, and
/// confirms the pod answering is the expected one (RUNPOD_POD_ID via /proc/1/environ). This
/// is the cutover's safety gate — a swap that leaves the proxy pointing at an unreachable or
/// wrong pod is exactly what broke a participant before.
async fn proxy_reaches_pod(cfg: &Config, name: &str, expected_id: &str) -> Result<bool> {
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
    };
    let probe = "tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p'";
    match arena_core::ssh::run(&target, probe).await {
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

/// Apply the current fleet's proxy config (re-point nginx). Best-effort wrapper used by the
/// migration cutover/revert so the proxy follows the rename.
async fn apply_proxy(cfg: &Config, owner: &dyn Provider) -> Result<()> {
    let pods = owner.list_pods().await.context("listing pods for proxy apply")?;
    deploy_proxy(cfg, &pods, true).await
}

/// `migrate copy`: build + set up `<name>-new` (or reuse it) and sync `<name>`'s files onto
/// it. No rename, no proxy — the participant keeps using `<name>` and can test the new pod.
async fn handle_migrate_copy(
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let src_id = pods.iter().find(|p| p.name == canonical).unwrap().id.clone();
    let new_name = format!("{canonical}-new");
    let existing_new = pods.iter().find(|p| p.name == new_name).cloned();

    if dry_run {
        println!("migrate copy {canonical} (dry-run):");
        if existing_new.is_some() {
            println!("  - {new_name} exists → incremental re-sync {canonical} → {new_name}");
        } else {
            let spec = build_replacement_spec(cfg, owner.as_ref(), &src_id, &canonical, ov).await?;
            println!("  - create {new_name} ({})", owner.describe(&spec));
            println!("  - wait for it to stabilize, then set it up");
            println!("  - sync {canonical} → {new_name}");
        }
        println!("  (no rename, no proxy change)\n[dry-run] nothing changed.");
        return Ok(());
    }

    let new_id = if let Some(np) = existing_new {
        println!("Re-using existing {new_name} (id={}) — incremental re-sync.", np.id);
        np.id
    } else {
        let spec = build_replacement_spec(cfg, owner.as_ref(), &src_id, &canonical, ov).await?;
        if !confirm(
            yes,
            &format!(
                "Will create {new_name} ({}) and sync {canonical}'s files onto it — no rename, \
                 no proxy change.",
                owner.describe(&spec)
            ),
        )? {
            println!("aborted.");
            return Ok(());
        }
        let mut nspec = spec.clone();
        nspec.name = new_name.clone();
        nspec.env.push(("MACHINE_NAME".to_string(), new_name.clone()));
        println!("[1/3] creating {new_name} ({})…", owner.describe(&nspec));
        let policy = arena_core::retry::RetryPolicy::default();
        let created = arena_core::retry::retrying(&policy, || owner.create_pod(&nspec))
            .await
            .with_context(|| format!("creating {new_name}"))?;
        println!("      created id={}", created.id);
        println!("[2/3] waiting for {new_name} to come up and stabilize…");
        wait_for_endpoint(owner.as_ref(), &created.id, 900)
            .await
            .with_context(|| format!("{new_name} never came up"))?;
        wait_for_stable_endpoint(owner.as_ref(), &created.id, 90, 600)
            .await
            .with_context(|| format!("{new_name} never stabilized"))?;
        println!("      provisioning {new_name}…");
        handle_setup(owner.as_ref(), cfg, true, false, None, None, false, Some(&[new_name.clone()]))
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        created.id
    };

    println!("[3/3] syncing {canonical} → {new_name}…");
    copy_pod_files(cfg, owner.as_ref(), &src_id, &new_id)
        .await
        .with_context(|| format!("syncing {canonical} -> {new_name}"))?;
    clean_marker(owner.as_ref(), &new_id, cfg).await;

    println!("\n✓ {new_name} is built and synced (participant still on {canonical}, untouched).");
    if let Ok(Some(p)) =
        owner.list_pods().await.map(|ps| ps.into_iter().find(|p| p.id == new_id))
    {
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

/// `migrate cutover`: final delta sync, swap names, re-point + VERIFY the proxy, auto-revert
/// if the new pod isn't reachable through the proxy. Keeps `<name>-old`.
async fn handle_migrate_cutover(
    cfg: &Config,
    target: &str,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh;
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let new_name = format!("{canonical}-new");
    let old_name = format!("{canonical}-old");
    let src_id = pods.iter().find(|p| p.name == canonical).unwrap().id.clone();
    let new_id = pods
        .iter()
        .find(|p| p.name == new_name)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no {new_name} — run `arena pods migrate copy {canonical}` first"))?;
    if pods.iter().any(|p| p.name == old_name) {
        anyhow::bail!("{old_name} already exists — finish/revert the previous migration first");
    }

    if dry_run {
        println!("migrate cutover {canonical} (dry-run):");
        println!("  1. final delta sync {canonical} → {new_name}");
        println!("  2. rename {canonical} → {old_name}, then {new_name} → {canonical}");
        if skip_proxy {
            println!("  3. proxy SKIPPED — run `arena proxy apply` yourself");
        } else {
            println!("  3. proxy apply, then SSH through the proxy to confirm {canonical} works");
            println!("  4. if unreachable → auto-revert names + proxy (participant stays put)");
        }
        println!("  {old_name} kept (delete later with `migrate finish {canonical}`).\n[dry-run] nothing changed.");
        return Ok(());
    }

    if !confirm(
        yes,
        &format!(
            "Cut over {canonical} to {new_name}: final sync, then swap names{} and verify. \
             {old_name} kept.",
            if skip_proxy { " (no proxy)" } else { " + proxy" }
        ),
    )? {
        println!("aborted.");
        return Ok(());
    }

    // 1. final delta sync + verify it landed.
    println!("[1/4] final delta sync {canonical} → {new_name}…");
    copy_pod_files(cfg, owner.as_ref(), &src_id, &new_id)
        .await
        .with_context(|| format!("final sync {canonical} -> {new_name}"))?;
    clean_marker(owner.as_ref(), &new_id, cfg).await;
    println!("[2/4] verifying {new_name} health…");
    verify_replacement(owner.as_ref(), &new_id, cfg).await?;

    // 3. swap names (old-first so the canonical name is never on two pods).
    println!("[3/4] swapping names…");
    let policy = arena_core::retry::RetryPolicy::default();
    arena_core::retry::retrying(&policy, || owner.rename_pod(&src_id, &old_name))
        .await
        .with_context(|| format!("renaming {canonical} -> {old_name}"))?;
    if let Err(e) =
        arena_core::retry::retrying(&policy, || owner.rename_pod(&new_id, &canonical)).await
    {
        eprintln!("      promote failed ({e}); rolling back {old_name} -> {canonical}");
        let _ = owner.rename_pod(&src_id, &canonical).await;
        return Err(e).context("promoting new pod (rolled back; original intact)");
    }
    // set ~/.name on the now-canonical pod
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let short = canonical.strip_prefix(&format!("{prefix}-")).unwrap_or(&canonical);
    if let Ok(t) = fresh_target(owner.as_ref(), &new_id, cfg).await {
        let _ = ssh::run(
            &t,
            &format!("printf %s {} > \"$HOME/.name\"", shell_quote(&format!("export MACHINE_NAME='{short}'"))),
        )
        .await;
    }

    // 4. proxy apply + verify through the proxy; auto-revert on failure.
    if skip_proxy {
        println!("[4/4] proxy skipped — run `arena proxy apply` to route {canonical} to the new pod.");
    } else {
        println!("[4/4] re-pointing proxy and verifying {canonical} through it…");
        if let Err(e) = apply_proxy(cfg, owner.as_ref()).await {
            eprintln!("      proxy apply failed: {e} — auto-reverting.");
            cutover_revert(cfg, owner.as_ref(), &canonical, &new_id, &src_id, true).await;
            anyhow::bail!("cutover reverted: proxy apply failed. {canonical} is back on the original.");
        }
        // `nginx -s reload` is GRACEFUL: for a few seconds after the reload, existing worker
        // processes keep serving the OLD config, so a fresh connection can still be routed to
        // the OLD pod. A single immediate probe therefore reads the old pod's id and false-
        // fails. Wait for the reload to settle and retry the through-proxy identity check —
        // succeed as soon as the proxy actually reaches the new pod (up to ~45s).
        let settle_tries = 15;
        let mut reached = false;
        for attempt in 1..=settle_tries {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if proxy_reaches_pod(cfg, &canonical, &new_id).await.unwrap_or(false) {
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
            cutover_revert(cfg, owner.as_ref(), &canonical, &new_id, &src_id, true).await;
            anyhow::bail!(
                "cutover reverted: the new pod wasn't reachable through the proxy. {canonical} is \
                 back on the original (untouched). Check the new pod, then retry."
            );
        }
    }

    println!(
        "\n✓ Cut over: {canonical} is now the new pod. Original parked as {old_name} (kept).\n  \
         If something's wrong, roll back:  arena pods migrate revert {canonical}\n  \
         When you're happy, remove the old pod:  arena pods migrate finish {canonical}"
    );
    Ok(())
}

/// Roll a cutover back: rename `<name>` → `<name>-new`, `<name>-old` → `<name>`, and (unless
/// `skip_proxy`) re-point the proxy to the restored original. Shared by the cutover's
/// auto-revert and the manual `migrate revert`.
async fn cutover_revert(
    cfg: &Config,
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
    if apply_px {
        if let Err(e) = apply_proxy(cfg, owner).await {
            eprintln!("  REVERT WARNING: proxy apply failed ({e}) — run `arena proxy apply`.");
        }
    }
}

/// `migrate revert`: manual rollback of a cutover (swap `<name>` ↔ `<name>-old` + proxy).
async fn handle_migrate_revert(
    cfg: &Config,
    target: &str,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let old_name = format!("{canonical}-old");
    let new_id = pods.iter().find(|p| p.name == canonical).unwrap().id.clone();
    let old_id = pods
        .iter()
        .find(|p| p.name == old_name)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no {old_name} to revert to (nothing to roll back)"))?;

    if dry_run {
        println!("migrate revert {canonical} (dry-run):");
        println!("  rename {canonical} → {canonical}-new, {old_name} → {canonical}");
        println!("  {}", if skip_proxy { "(no proxy)" } else { "then proxy apply (route back to the original)" });
        println!("[dry-run] nothing changed.");
        return Ok(());
    }
    if !confirm(yes, &format!("Roll {canonical} back to the original ({old_name}){}.", if skip_proxy { "" } else { " + proxy" }))? {
        println!("aborted.");
        return Ok(());
    }
    cutover_revert(cfg, owner.as_ref(), &canonical, &new_id, &old_id, !skip_proxy).await;
    println!("✓ Reverted: {canonical} is the original again; the new pod is parked as {canonical}-new.");
    Ok(())
}

/// `migrate finish`: terminate the parked `<name>-old` pod.
async fn handle_migrate_finish(cfg: &Config, target: &str, dry_run: bool, yes: bool) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let old_name = format!("{canonical}-old");
    let old = pods
        .iter()
        .find(|p| p.name == old_name)
        .ok_or_else(|| anyhow::anyhow!("no {old_name} to remove (nothing to finish)"))?;
    if dry_run {
        println!("migrate finish {canonical} (dry-run): would terminate {old_name} (id={}).", old.id);
        return Ok(());
    }
    if !confirm(yes, &format!("Permanently terminate {old_name} (id={})? This deletes the original pod.", old.id))? {
        println!("aborted.");
        return Ok(());
    }
    owner.terminate_pod(&old.id).await.with_context(|| format!("terminating {old_name}"))?;
    println!("✓ Terminated {old_name}. Migration of {canonical} complete.");
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
async fn handle_replace(
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    keep_old: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh;
    // Resolve the source pod and the backend that owns it. `resolve_target_any` accepts a
    // name or a raw id, so read the canonical name back from the fleet rather than trusting
    // the argument (a user may pass an id).
    let (owner, src_id, src_label) = resolve_target_any(cfg, target).await?;
    let pods = owner.list_pods().await.context("listing pods for replace")?;
    let src = pods
        .iter()
        .find(|p| p.id == src_id)
        .ok_or_else(|| anyhow::anyhow!("source pod {src_label} vanished while planning"))?;
    let canonical = src.name.clone();
    if canonical.ends_with("-new") || canonical.ends_with("-old") {
        anyhow::bail!(
            "{canonical} looks like a replace staging pod — pass the canonical machine name, \
             not a -new/-old leftover"
        );
    }
    let (new_name, old_name) = replace_stage_names(&canonical);

    let spec = build_replacement_spec(cfg, owner.as_ref(), &src_id, &src_label, ov).await?;

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
    println!("  target spec: {}", owner.describe(&spec));
    println!("  pipeline:");
    println!("    1. create   {new_name}   ({})", owner.describe(&spec));
    println!("    2. setup    {new_name}   (deploy key, ssh config, repo, ~/.name={canonical})");
    println!("    3. copy     {canonical} -> {new_name}   (home dir, pod-to-pod rsync)");
    println!("    4. verify   {new_name}   (ssh health check + copied-size sanity)");
    println!(
        "    5. swap     rename {canonical} -> {old_name}, then {new_name} -> {canonical}"
    );
    if skip_proxy {
        println!("    6. proxy    SKIPPED (--skip-proxy) — run `arena proxy apply` yourself");
    } else {
        println!("    6. proxy    apply (repoint nginx; stable port reclaimed by the canonical name)");
    }
    if keep_old {
        println!("    7. cleanup  keep {old_name} (--keep-old) — terminate it yourself once happy");
    } else {
        println!("    7. cleanup  terminate {old_name} after the swap verifies");
    }

    if dry_run {
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

    // [1/7] create the replacement under the staging name.
    let mut new_spec = spec.clone();
    new_spec.name = new_name.clone();
    new_spec.env.push(("MACHINE_NAME".to_string(), new_name.clone()));
    println!("[1/7] creating {new_name} ({})…", owner.describe(&new_spec));
    let created = arena_core::retry::retrying(&policy, || owner.create_pod(&new_spec))
        .await
        .with_context(|| format!("creating {new_name}"))?;
    println!("      created {new_name} id={}", created.id);

    // [2/7] wait for an SSH endpoint, then for it to STABILIZE. A freshly-created pod's
    // ip:port churns while RunPod places it, and a still-moving pod can migrate and reset its
    // container disk — wiping any setup+copy. Provisioning a *settled* pod avoids most of that.
    println!("[2/7] waiting for {new_name} to come up and stabilize…");
    // RunPod can be slow to assign a fresh pod's SSH endpoint — 5+ min is not unusual,
    // especially on the secure tier — so give it a generous window before giving up.
    wait_for_endpoint(owner.as_ref(), &created.id, 900)
        .await
        .with_context(|| format!("{new_name} never came up — left in place for inspection"))?;
    wait_for_stable_endpoint(owner.as_ref(), &created.id, 90, 600)
        .await
        .with_context(|| format!("{new_name}'s endpoint never stabilized"))?;

    // [3-5/7] provision + copy, then confirm the copy SURVIVES a settle window. Even a settled
    // pod can still migrate and reset to image state (losing setup AND the copy), so re-do
    // setup+copy until the delivery marker is still present after a wait — or give up (no swap,
    // original untouched).
    let copy_attempts = 3;
    let mut persisted = false;
    for attempt in 1..=copy_attempts {
        println!("[3/7] provisioning {new_name}… (attempt {attempt}/{copy_attempts})");
        handle_setup(owner.as_ref(), cfg, true, false, None, None, false, Some(&[new_name.clone()]))
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        println!("[4/7] copying {canonical} → {new_name} (excludes caches, HF models, .claude, .ssh, shell-rc keys)…");
        copy_pod_files(cfg, owner.as_ref(), &src_id, &created.id)
            .await
            .with_context(|| format!("copying {canonical} -> {new_name}"))?;
        println!("[5/7] confirming the copy persists (~90s — pods can reset while still settling)…");
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        if marker_present(owner.as_ref(), &created.id, cfg).await {
            persisted = true;
            break;
        }
        eprintln!("      {new_name} reset to image state (lost setup+copy) — re-provisioning…");
        let _ = wait_for_stable_endpoint(owner.as_ref(), &created.id, 90, 600).await;
    }
    if !persisted {
        anyhow::bail!(
            "{new_name} keeps resetting its disk (it's migrating between hosts), so the copy \
             won't stick. NOT swapping — {canonical} is untouched. Try again when capacity is \
             less contended, or terminate {new_name} with `arena pods terminate {new_name}`."
        );
    }
    clean_marker(owner.as_ref(), &created.id, cfg).await;

    // Health check before the swap (re-resolves the endpoint fresh and retries).
    println!("      verifying health of {new_name}…");
    verify_replacement(owner.as_ref(), &created.id, cfg)
        .await
        .with_context(|| format!("verifying {new_name}"))?;

    // Manual gate: nothing destructive has happened yet. A "no" leaves the built+copied
    // replacement in place under its staging name for inspection.
    if !confirm(
        yes,
        &format!(
            "{new_name} is built, copied and verified. Swap it in as {canonical} now? \
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
    // Make the promoted pod's `~/.name` match its new (canonical) name. The copy overwrote
    // whatever setup wrote (with the *source's* file), so re-assert it here — in the same
    // `export MACHINE_NAME='<short>'` form setup uses, so the rest of the tooling reads it
    // consistently. Re-resolve the endpoint first (it can churn) and warn (don't swallow) on
    // failure, since a wrong `~/.name` mislabels the pod in backups/metrics.
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let short = canonical.strip_prefix(&format!("{prefix}-")).unwrap_or(&canonical);
    let dotname = format!("export MACHINE_NAME='{short}'");
    let name_cmd = format!("printf %s {} > \"$HOME/.name\"", shell_quote(&dotname));
    let name_written = match wait_for_endpoint(owner.as_ref(), &created.id, 60).await {
        Ok(p) => match ssh::SshTarget::from_pod(&p, cfg) {
            Ok(t) => ssh::run(&t, &name_cmd).await.map(|o| o.success).unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => false,
    };
    if !name_written {
        eprintln!(
            "      warning: couldn't update ~/.name on {canonical} — set it later with \
             `arena pods setup {canonical}` (or `echo \"{dotname}\" > ~/.name` on the pod)."
        );
    }

    // [7/7] proxy + cleanup.
    if skip_proxy {
        println!("[7/7] proxy skipped (--skip-proxy) — run `arena proxy apply` to repoint nginx.");
    } else {
        println!("[7/7] re-pointing proxy…");
        let fresh = owner.list_pods().await.unwrap_or_default();
        if let Err(e) = deploy_proxy(cfg, &fresh, true).await {
            eprintln!("      proxy apply failed: {e} — run `arena proxy apply` manually.");
        }
    }
    if keep_old {
        println!(
            "done — {canonical} is the fresh pod. {old_name} kept (--keep-old); terminate it with \
             `arena pods terminate {old_name}` once you're happy."
        );
    } else {
        println!("terminating parked {old_name}…");
        if let Err(e) = arena_core::retry::retrying(&policy, || owner.terminate_pod(&src_id)).await {
            eprintln!("      couldn't terminate {old_name}: {e} — remove it manually.");
        } else {
            println!("done — {canonical} replaced; {old_name} terminated.");
        }
    }
    Ok(())
}

/// Poll `provider` until the pod `id` has an SSH endpoint (ip + port), or `timeout_secs`
/// elapses. Returns the ready pod.
async fn wait_for_endpoint(
    provider: &dyn Provider,
    id: &str,
    timeout_secs: u64,
) -> Result<arena_core::Pod> {
    let policy = arena_core::retry::RetryPolicy::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let pods =
            arena_core::retry::retrying(&policy, || provider.list_pods()).await.unwrap_or_default();
        if let Some(p) = pods.into_iter().find(|p| p.id == id) {
            if p.ssh_ip.is_some() && p.ssh_port.is_some() {
                return Ok(p);
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out after {timeout_secs}s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
/// the marker. Any failure (unreachable, wrong recycled-port pod, marker gone) → false.
async fn marker_present(provider: &dyn Provider, dest_id: &str, cfg: &Config) -> bool {
    let token = copy_marker_token(dest_id);
    let Ok(target) = fresh_target(provider, dest_id, cfg).await else { return false };
    let probe = format!(
        "printf 'ID=%s\\nMARK=%s\\n' \
         \"$(tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p')\" \
         \"$(cat \"{}\" 2>/dev/null)\"",
        copy_marker_path()
    );
    let Ok(o) = arena_core::ssh::run(&target, &probe).await else { return false };
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
async fn target_is_pod(
    target: &arena_core::ssh::SshTarget,
    expected_id: &str,
    provider: &dyn Provider,
) -> bool {
    if provider.name() != "runpod" {
        return true;
    }
    let probe = "tr '\\0' '\\n' < /proc/1/environ 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p'";
    matches!(arena_core::ssh::run(target, probe).await, Ok(o) if o.success && o.stdout.trim() == expected_id)
}

/// Best-effort removal of the copy marker from a pod.
async fn clean_marker(provider: &dyn Provider, dest_id: &str, cfg: &Config) {
    if let Ok(t) = fresh_target(provider, dest_id, cfg).await {
        let _ = arena_core::ssh::run(&t, &format!("rm -f \"{}\"", copy_marker_path())).await;
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
    let policy = arena_core::retry::RetryPolicy::default();
    let start = std::time::Instant::now();
    let mut last: Option<(String, u16)> = None;
    let mut stable_since = std::time::Instant::now();
    let mut latest: Option<arena_core::Pod> = None;
    loop {
        let pods =
            arena_core::retry::retrying(&policy, || provider.list_pods()).await.unwrap_or_default();
        let pod = pods.into_iter().find(|p| p.id == id);
        let ep = pod.as_ref().and_then(|p| p.ssh_ip.clone().zip(p.ssh_port));
        latest = pod.or(latest);
        match ep {
            Some(cur) => {
                if last.as_ref() != Some(&cur) {
                    last = Some(cur);
                    stable_since = std::time::Instant::now();
                } else if stable_since.elapsed().as_secs() >= stable_secs {
                    return Ok(latest.unwrap());
                }
            }
            None => {
                last = None;
                stable_since = std::time::Instant::now();
            }
        }
        if start.elapsed().as_secs() >= timeout_secs {
            return latest
                .filter(|p| p.ssh_ip.is_some() && p.ssh_port.is_some())
                .ok_or_else(|| anyhow::anyhow!("endpoint never stabilized within {timeout_secs}s"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

/// Copy the source pod's home onto the destination pod for a replace. Tries a **direct**
/// pod-to-pod rsync (run on the source, pushing to the dest endpoint with the deploy key
/// both pods hold); on any failure falls back to **via-local** (pull the source home into a
/// control-side staging dir, then push it up to the dest). Both use the replication exclude
/// set (caches, HF models, `.claude`, `.ssh`).
///
/// Two correctness guards, both learned the hard way against churning pods:
/// 1. **Fresh endpoints per transfer** — `src`/`dest` SSH ip:port are re-resolved
///    immediately before each rsync. An endpoint captured before a multi-GB pull can go
///    stale mid-pull, sending the push to a since-reassigned port (which silently delivers
///    nothing).
/// 2. **Delivery is verified, not assumed** — a marker is planted on the source, carried by
///    the copy, and read back from a freshly-resolved dest. A push that "succeeded" against
///    a stale endpoint fails this check, so we never swap in a pod that didn't get the data.
async fn copy_pod_files(
    cfg: &Config,
    provider: &dyn Provider,
    src_id: &str,
    dest_id: &str,
) -> Result<()> {
    use arena_core::pull::{self, PullConfig};
    use arena_core::ssh;
    let pc = PullConfig::replication();
    let remote_key = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string();
    let user = cfg.get("SSH_USER").unwrap_or("root").to_string();

    // Plant a delivery marker on the source (a dotfile the copy carries, not matched by any
    // exclude). It's verified on the dest after the copy; the dest copy is left in place for
    // the caller's persistence re-check (then cleaned up there). Source copy is cleaned here.
    let token = copy_marker_token(dest_id);
    let marker = copy_marker_path();
    let src_target = fresh_target(provider, src_id, cfg).await.context("resolving source endpoint")?;
    // Identity-check the SOURCE before reading from it: if its ip:port was reassigned to a
    // different pod, we'd otherwise copy a STRANGER's home onto the new pod (and the dest
    // marker check would still pass, since the marker rides along). Fail closed.
    if !target_is_pod(&src_target, src_id, provider).await {
        anyhow::bail!(
            "source {src_id}'s SSH endpoint doesn't resolve to that pod (its ip:port was likely \
             reassigned to a different pod) — refusing to copy from the wrong pod. Re-run."
        );
    }
    ssh::run(&src_target, &format!("printf %s {} > \"{marker}\"", shell_quote(&token)))
        .await
        .context("planting copy marker on source")?;

    // --- direct attempt: rsync ON the source, pushing to the dest's fresh endpoint ---
    let dest_pod = wait_for_endpoint(provider, dest_id, 120).await.context("resolving dest endpoint")?;
    let direct = pull::pod_to_pod_command(
        dest_pod.ssh_ip.as_deref().unwrap_or_default(),
        dest_pod.ssh_port.unwrap_or(22),
        &user,
        &remote_key,
        &pc,
    );
    let direct_ok = matches!(ssh::run(&src_target, &direct).await, Ok(o) if o.success);
    if direct_ok {
        println!("      copied (direct pod-to-pod)");
    } else {
        eprintln!("      direct copy unavailable (source lacks pod-to-pod key) — using via-local staging…");
        // pull source -> staging (fresh source), then push staging -> dest (FRESH dest, right
        // before the push — this is the endpoint most likely to have gone stale during the pull).
        let stage = std::env::temp_dir().join(format!("arena-replace-{src_id}"));
        std::fs::create_dir_all(&stage)
            .with_context(|| format!("creating staging dir {}", stage.display()))?;
        let stage_s = format!("{}/", stage.to_string_lossy());
        let src_target = fresh_target(provider, src_id, cfg).await?;
        if !target_is_pod(&src_target, src_id, provider).await {
            anyhow::bail!(
                "source {src_id}'s endpoint resolved to a different pod before the pull — \
                 refusing to copy the wrong pod's data. Re-run."
            );
        }
        run_rsync(&pull::rsync_args(&src_target, &pc, &stage_s)).await.context("pull source -> staging")?;
        let dest_target = fresh_target(provider, dest_id, cfg).await?;
        run_rsync(&pull::push_rsync_args(&dest_target, &pc, &stage_s)).await.context("push staging -> dest")?;
        println!("      copied (via local staging {})", stage.display());
    }

    // Verify the data actually landed on the RIGHT pod (identity + marker). `marker_present`
    // re-resolves the endpoint fresh and confirms `RUNPOD_POD_ID` matches, so a push that
    // "succeeded" against a since-reassigned port (a different pod) is caught.
    let landed = marker_present(provider, dest_id, cfg).await;
    // Clean the SOURCE marker now; leave the DEST marker for the caller's persistence re-check.
    let _ = ssh::run(&src_target, &format!("rm -f \"{marker}\"")).await;
    if !landed {
        anyhow::bail!(
            "copy verification failed — the data didn't reach the intended new pod ({dest_id}) \
             (a churning endpoint likely got reassigned mid-transfer). NOT swapping; the \
             original is untouched. Re-run to retry."
        );
    }
    Ok(())
}

/// Spawn `rsync` with the given argv; error (with stderr) on a non-zero exit.
async fn run_rsync(args: &[String]) -> Result<()> {
    let out = tokio::process::Command::new("rsync")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawning rsync")?;
    if !out.status.success() {
        anyhow::bail!("rsync failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Pre-swap health check: the replacement must answer over SSH (and report its GPU if it
/// has one). Re-resolves the pod's endpoint fresh on each attempt and retries a few times,
/// because RunPod can reassign a pod's SSH ip/port right after provisioning — a single shot
/// against a stale endpoint spuriously reads as "Permission denied". (A precise copied-size
/// check is approximated by rsync's own success in `copy_pod_files`, plus the manual swap
/// confirm; the post-swap `~/.name` write re-resolves its own endpoint.)
async fn verify_replacement(provider: &dyn Provider, id: &str, cfg: &Config) -> Result<()> {
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
        match arena_core::ssh::run(
            &target,
            "nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1; echo SSH_OK",
        )
        .await
        {
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

/// Resolve a user-supplied target (machine name, bare short name, OR raw provider id) by
/// searching EVERY configured provider, returning the owning provider too — so
/// `stop`/`restart`/`terminate <name>` work whatever backend the pod lives on, without
/// passing `--provider`. The mutation must go to the owning provider's API. Requires the
/// pod to actually exist (a typo fails clearly). Best-effort listing; a provider that
/// errors is warned about and skipped.
async fn resolve_target_any(
    cfg: &Config,
    target: &str,
) -> Result<(Box<dyn Provider>, String, String)> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let policy = arena_core::retry::RetryPolicy::default();
    for name in ["runpod", "vast", "hetzner"] {
        let Ok(p) = arena_core::provider::build(name, cfg) else { continue };
        let pods = match arena_core::retry::retrying(&policy, || p.list_pods()).await {
            Ok(pods) => pods,
            Err(e) => {
                eprintln!("warning: couldn't list {name} pods ({e})");
                continue;
            }
        };
        if let Some(pod) = pods.iter().find(|pd| pod_matches(pd, target, prefix)) {
            let label = format!("{} (id={}, {name})", pod.name, pod.id);
            return Ok((p, pod.id.clone(), label));
        }
    }
    anyhow::bail!("no pod with name or id '{target}' on any provider (run `arena pods list`)")
}

/// List pods and apply the legacy filter semantics: drop `exclude` first, then keep only
/// `include` (if that list is non-empty), then optionally keep only pods whose status
/// contains `status_contains` (case-insensitive, e.g. "RUNNING"). Names (full or bare)
/// or ids match.
async fn select_pods(
    provider: &dyn Provider,
    cfg: &Config,
    include: &[String],
    exclude: &[String],
    status_contains: Option<&str>,
) -> Result<Vec<arena_core::Pod>> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let policy = arena_core::retry::RetryPolicy::default();
    let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods())
        .await
        .context("listing pods")?;
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    pods.retain(|p| !exclude.iter().any(|x| pod_matches(p, x, prefix)));
    if !include.is_empty() {
        pods.retain(|p| include.iter().any(|x| pod_matches(p, x, prefix)));
        // A non-empty include that matches nothing is a user error (typo'd name) — fail
        // loudly instead of looking like an idle/empty fleet.
        if pods.is_empty() {
            anyhow::bail!("no pods matched --include {include:?} (run `arena pods list`)");
        }
    }
    if let Some(s) = status_contains {
        let s = s.to_uppercase();
        pods.retain(|p| p.status.to_uppercase().contains(&s));
    }
    Ok(pods)
}

/// `pods init-branches`: create each pod's `autocommit-…-wNdM-…` branch and push it
/// upstream, without committing — so a new day's branch exists before backups run.
async fn handle_init_branches(
    provider: &dyn Provider,
    cfg: &Config,
    week: Option<u32>,
    day: Option<u32>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    let (week, day) = resolve_week_day(cfg, week, day)?;
    let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
    println!("Iteration: w{week}d{day}\n");

    let pods = provider.list_pods().await.context("listing pods for init-branches")?;
    let mut targets = Vec::new();
    for pod in &pods {
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint)");
        return Ok(());
    }

    if dry_run {
        println!("Dry-run — would init the w{week}d{day} branch on {} pod(s):\n", targets.len());
        for (name, t) in &targets {
            let cmd = arena_core::backup::init_branch_command(&bcfg, name);
            println!("# {name}  ->  branch {}", bcfg.branch_for(name));
            println!("{}\n", t.display_command(&cmd));
        }
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }
    if !confirm(yes, &format!(
        "Will create + push the w{week}d{day} autocommit branch on {} pod(s).",
        targets.len()
    ))? {
        println!("aborted.");
        return Ok(());
    }

    let total = targets.len();
    println!("Initializing branches on {total} pod(s) over SSH…");
    let mut set = tokio::task::JoinSet::new();
    for (name, target) in targets {
        let cmd = arena_core::backup::init_branch_command(&bcfg, &name);
        let branch = bcfg.branch_for(&name);
        set.spawn(async move { (name, branch, ssh::run(&target, &cmd).await) });
    }
    let (mut ok, mut failed, mut done) = (0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, branch, res)) = joined else { continue };
        match res {
            Ok(out) if out.success => {
                println!("[{done}/{total}] ✓ {name} -> {branch}");
                ok += 1;
            }
            Ok(out) => {
                println!("[{done}/{total}] ✗ {name} (exit {:?}): {}", out.code, out.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} initialized, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to init branch");
    }
    Ok(())
}

/// `pods pull`: rsync each pod's home directory to `<dir>/<label>/<pod-name>/`. The file
/// backup (legacy `backup.sh`), complementing the git autocommit `backup`.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
async fn handle_pull(
    provider: &dyn Provider,
    cfg: &Config,
    label: Option<String>,
    dir: &str,
    max_size: Option<String>,
    remote_path: Option<String>,
    no_git: bool,
    no_big: bool,
    target_filter: Option<&str>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::pull::{self, PullConfig};
    use arena_core::ssh::SshTarget;

    // Label: explicit, else the computed wNdM iteration.
    let label = match label {
        Some(l) => l,
        None => {
            let (w, d) = resolve_week_day(cfg, None, None)?;
            format!("w{w}d{d}")
        }
    };
    // Knobs come from flags, else config (BACKUP_MAX_SIZE / BACKUP_REMOTE_PATH), else
    // sensible defaults. `.git` is kept by default unless --no-git.
    let threshold = max_size
        .or_else(|| cfg.get("BACKUP_MAX_SIZE").filter(|s| !s.is_empty()).map(String::from))
        .unwrap_or_else(|| "50M".to_string());
    let remote_path = remote_path
        .or_else(|| cfg.get("BACKUP_REMOTE_PATH").filter(|s| !s.is_empty()).map(String::from))
        .unwrap_or_default();

    // Two tiers: (1) a dated `wNdM` snapshot of the small files (< threshold) for history, and
    // (2) the `big` tier — every file (no size cap), accumulated into `<dir>/big/<pod>/`. The
    // big tier never deletes, so files removed on the pod stay kept in the backup; it just
    // saves all the files. Both share the default excludes (HF cache, venvs, …). --no-big
    // writes only the snapshot tier.
    let mut small = PullConfig { remote_path: remote_path.clone(), ..PullConfig::small_tier(threshold.as_str()) };
    let mut big = PullConfig { remote_path: remote_path.clone(), ..PullConfig::big_tier() };
    if no_git {
        small = small.without_git();
        big = big.without_git();
    }

    let src = if remote_path.is_empty() { "~/ (home)".to_string() } else { remote_path.clone() };
    let big_note = if no_big {
        "big tier skipped".to_string()
    } else {
        format!("all files -> {dir}/big/<pod>/")
    };
    println!(
        "Source {src} · snapshot < {threshold} -> {dir}/{label}/<pod>/ · {big_note} · {}\n",
        if no_git { "no .git" } else { "incl .git" },
    );

    let pods = provider.list_pods().await.context("listing pods for pull")?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let mut targets: Vec<(String, SshTarget)> = Vec::new();
    for pod in &pods {
        if let Some(t) = target_filter {
            if !pod_matches(pod, t, prefix) {
                continue;
            }
        }
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to pull)");
        return Ok(());
    }

    // Per-pod tiers: (tier name, dest, config). `big` is dropped when --no-big.
    let tiers_for = |name: &str| -> Vec<(&'static str, String, &PullConfig)> {
        let mut v = vec![("snapshot", pull::local_dest(dir, &label, name), &small)];
        if !no_big {
            v.push(("big", pull::big_dest(dir, name), &big));
        }
        v
    };

    if dry_run {
        let n = if no_big { 1 } else { 2 };
        println!("Dry-run — would rsync {} pod(s), {n} tier(s) each:\n", targets.len());
        for (name, t) in &targets {
            println!("# {name}");
            for (tier, dest, pc) in tiers_for(name) {
                println!("  [{tier}] {}", pull::display_rsync(t, pc, &dest));
            }
            println!();
        }
        println!("Preview only — run without --dry-run to copy.");
        return Ok(());
    }
    let dest_msg = if no_big {
        format!("the dated snapshot {dir}/{label}/")
    } else {
        format!("{dir}/ (dated snapshot {label} + the all-files big/)")
    };
    if !confirm(yes, &format!("Will rsync {} pod home(s) into {dest_msg}.", targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let total_pods = targets.len();
    println!("Pulling {total_pods} pod(s) into {dest_msg}…");
    let mut set = tokio::task::JoinSet::new();
    let (mut jobs, mut failed) = (0, 0);
    for (name, t) in &targets {
        for (tier, dest, pc) in tiers_for(name) {
            // rsync needs the destination directory to exist.
            if let Err(e) = std::fs::create_dir_all(&dest) {
                eprintln!("[FAILED] {name} [{tier}]: creating {dest}: {e}");
                failed += 1;
                continue;
            }
            let args = pull::rsync_args(t, pc, &dest);
            let name = name.clone();
            jobs += 1;
            set.spawn(async move {
                let out = tokio::process::Command::new("rsync")
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await;
                (name, tier, out)
            });
        }
    }
    let (mut ok, mut done) = (0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, tier, out)) = joined else { continue };
        match out {
            Ok(o) if o.status.success() => {
                // Report what actually moved, so a pull isn't "silent".
                let stdout = String::from_utf8_lossy(&o.stdout);
                let summary = match pull::parse_rsync_stats(&stdout) {
                    Some((files, size)) => format!("{files} files, {size}"),
                    None => "done".into(),
                };
                println!("[{done}/{jobs}] ✓ {name} [{tier}] ({summary})");
                ok += 1;
            }
            Ok(o) => {
                println!("[{done}/{jobs}] ✗ {name} [{tier}]: {}", String::from_utf8_lossy(&o.stderr).trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{jobs}] ✗ {name} [{tier}]: spawning rsync: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} rsync job(s) ok, {failed} failed across {total_pods} pod(s).");
    if failed > 0 {
        anyhow::bail!("{failed} rsync job(s) failed");
    }
    Ok(())
}

/// Single-quote for safe inclusion in a remote `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the remote command that wires up *fleet* SSH on a pod: authorize every fleet
/// pubkey for incoming SSH (arena8 + arena_infra + admin), and write the fleet host map
/// into a managed block in `~/.ssh/config` so the pod can `ssh <prefix>-<peer>` any other
/// pod (using its arena_infra key, `id_ed25519`). Idempotent — re-running refreshes the
/// block and skips already-present authorized keys, and it preserves other `~/.ssh/config`
/// blocks (e.g. the github.com deploy-key block `setup` writes).
fn fleet_ssh_command(pubkeys: &[String], rendered_config: &str) -> String {
    let mut cmd = String::from(
        "mkdir -p \"$HOME/.ssh\" && chmod 700 \"$HOME/.ssh\" && \
         touch \"$HOME/.ssh/authorized_keys\" && chmod 600 \"$HOME/.ssh/authorized_keys\"",
    );
    for pk in pubkeys {
        let q = shell_quote(pk.trim());
        cmd.push_str(&format!(
            "; (grep -qxF {q} \"$HOME/.ssh/authorized_keys\" || echo {q} >> \"$HOME/.ssh/authorized_keys\")"
        ));
    }
    cmd.push_str("; touch \"$HOME/.ssh/config\" && chmod 600 \"$HOME/.ssh/config\"");
    cmd.push_str(
        "; sed -i '/^# BEGIN arena-infra fleet/,/^# END arena-infra fleet/d' \"$HOME/.ssh/config\"",
    );
    cmd.push_str("; cat >> \"$HOME/.ssh/config\" <<'ARENAFLEETCFG'\n# BEGIN arena-infra fleet\n");
    cmd.push_str(rendered_config);
    if !rendered_config.ends_with('\n') {
        cmd.push('\n');
    }
    cmd.push_str("# END arena-infra fleet\nARENAFLEETCFG");
    cmd
}

/// The local directory `pods pull` rsyncs into: config `LOCAL_BACKUP_DIR`, else `./backup`.
fn local_backup_dir(cfg: &Config) -> String {
    cfg.get("LOCAL_BACKUP_DIR").filter(|s| !s.is_empty()).unwrap_or("./backup").to_string()
}

/// Resolve where a copied file should land on the pod. With an explicit `dest`, use it
/// verbatim. Otherwise, if the local path runs through the repo dir (`<repo_name>/…`),
/// mirror that path under the pod's repo parent (so `…/ARENA_3.0/a/b.py` →
/// `/root/ARENA_3.0/a/b.py`); failing that, fall back to the file's basename (lands in
/// the login/home dir). Pure, so it's unit-tested.
fn resolve_remote_dest(
    local: &std::path::Path,
    dest: Option<&str>,
    repo_name: &str,
    repo_parent: &str,
) -> String {
    if let Some(d) = dest {
        return d.to_string();
    }
    let s = local.to_string_lossy().replace('\\', "/");
    let needle = format!("{repo_name}/");
    if let Some(idx) = s.find(&needle) {
        return format!("{}/{}", repo_parent.trim_end_matches('/'), &s[idx..]);
    }
    local.file_name().and_then(|n| n.to_str()).unwrap_or("copied_file").to_string()
}

/// `pods copy`: scp a local file to every (filtered) pod, creating the remote parent
/// dir first. Destination per [`resolve_remote_dest`].
#[allow(clippy::too_many_arguments)]
async fn handle_copy(
    provider: &dyn Provider,
    cfg: &Config,
    file: &std::path::Path,
    dest: Option<&str>,
    recursive: bool,
    include: &[String],
    exclude: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh::{self, SshTarget};

    if recursive {
        if !file.exists() {
            anyhow::bail!("not found: {}", file.display());
        }
    } else if !file.is_file() {
        anyhow::bail!("not a file: {} (pass -r to copy a directory)", file.display());
    }
    let local = file.to_string_lossy().into_owned();

    // Resolve the remote destination (same for every pod).
    let repo_name = cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0");
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| format!("/root/{repo_name}"));
    let repo_parent = std::path::Path::new(&repo_path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/root");
    let remote = resolve_remote_dest(file, dest, repo_name, repo_parent);
    // The parent dir to ensure exists: the dir itself if `remote` ends with `/`, else its dirname.
    let remote_parent = if remote.ends_with('/') {
        remote.trim_end_matches('/').to_string()
    } else {
        match remote.rsplit_once('/') {
            Some((dir, _)) if !dir.is_empty() => dir.to_string(),
            _ => ".".to_string(),
        }
    };

    let mut pods = provider.list_pods().await.context("listing pods for copy")?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    pods.retain(|p| !exclude.iter().any(|x| pod_matches(p, x, prefix)));
    if !include.is_empty() {
        pods.retain(|p| include.iter().any(|x| pod_matches(p, x, prefix)));
        // A non-empty include that matches nothing is a typo, not an empty fleet — fail
        // loudly so the file doesn't silently copy nowhere.
        if pods.is_empty() {
            anyhow::bail!("no pods matched --include {include:?} (run `arena pods list`)");
        }
    }
    let mut targets: Vec<(String, SshTarget)> = Vec::new();
    for pod in &pods {
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => targets.push((pod.name.clone(), t)),
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to copy to)");
        return Ok(());
    }

    let rflag = if recursive { "-r " } else { "" };
    // Clear input→output preview before the confirm. Label the *type* (file vs folder)
    // explicitly — that's the thing people get wrong — and, for folders, spell out the
    // scp -r nesting rule (an existing dest dir, or a trailing slash, lands the folder
    // INSIDE → <dest>/<name>) that quietly produces a `name/name` mess.
    let is_dir = file.is_dir();
    let kind = if is_dir { "folder" } else { "file" };
    let base = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    println!("Copy preview (same on every pod):");
    println!("  input  [{kind} — check the type!]: {local}");
    println!("  output [{kind}]:                   {remote}");
    // Surface a flag/type mismatch: -r on a file is harmless but usually a mistake.
    if recursive && !is_dir {
        println!("  note: -r was passed but '{local}' is a FILE on disk, not a folder — copying it as a single file.");
    }
    if is_dir {
        let nested = format!("{}/{base}", remote.trim_end_matches('/'));
        println!(
            "  ⚠ -r nesting: scp drops the folder AT that path, but if {dest} already exists \
             on the pod (or {dest} has a trailing slash) it lands INSIDE → {nested}\n  \
             To overwrite a folder's contents, copy to its PARENT (or `rm -rf` the remote dir first).",
            dest = remote.trim_end_matches('/'),
        );
    }
    if dry_run {
        println!("\nDry-run — would scp to {} pod(s):\n", targets.len());
        for (name, t) in &targets {
            println!("# {name}");
            println!("scp {rflag}{}\n", t.scp_args(&local, &remote).join(" "));
        }
        println!("Preview only — run without --dry-run to copy.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will copy {local} to {} on {} pod(s).", remote, targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    // For a single file we verify it actually landed afterward (scp can exit 0 without
    // writing what you intended — e.g. the dest already exists as a directory, so the
    // file lands *inside* it). Skipped for -r (the dest is a tree, not one file).
    let expect_size = if recursive { None } else { std::fs::metadata(file).ok().map(|m| m.len()) };
    let basename = file.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();

    let total = targets.len();
    let mut set = tokio::task::JoinSet::new();
    for (name, t) in targets {
        let (local, remote, remote_parent, basename) =
            (local.clone(), remote.clone(), remote_parent.clone(), basename.clone());
        set.spawn(async move {
            // Ensure the remote parent dir exists, then scp (with -r if recursive).
            let mk = ssh::run(&t, &format!("mkdir -p {}", shell_quote(&remote_parent))).await;
            match mk {
                Ok(o) if o.success => {}
                Ok(o) => return (name, Err(format!("mkdir failed: {}", o.stderr.trim().to_string()))),
                Err(e) => return (name, Err(e.to_string())),
            }
            let mut args = t.scp_args(&local, &remote);
            if recursive {
                args.insert(0, "-r".to_string());
            }
            let out = tokio::process::Command::new("scp")
                .args(&args)
                .stdin(std::process::Stdio::null())
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => return (name, Err(String::from_utf8_lossy(&o.stderr).trim().to_string())),
                Err(e) => return (name, Err(format!("spawning scp: {e}"))),
            }
            // Verify the single-file copy actually landed at the expected size. A `remote`
            // ending in `/` means "into this dir" (intended → check <remote><basename>);
            // otherwise `remote` should BE the file, and finding a directory there is a
            // silent misplacement (scp dropped the file inside it) we flag rather than pass.
            let res = if let Some(expected) = expect_size {
                let into_dir = remote.ends_with('/');
                let check = if into_dir {
                    let final_path = format!("{remote}{basename}");
                    format!(
                        "f={f}; [ -f \"$f\" ] && echo \"OK $(wc -c < \"$f\" | tr -d ' ')\" || echo MISSING",
                        f = shell_quote(&final_path),
                    )
                } else {
                    format!(
                        "f={r}; if [ -d \"$f\" ]; then echo MISPLACED; elif [ -f \"$f\" ]; then echo \"OK $(wc -c < \"$f\" | tr -d ' ')\"; else echo MISSING; fi",
                        r = shell_quote(&remote),
                    )
                };
                match ssh::run(&t, &check).await {
                    Ok(o) if o.success => {
                        let line = o.stdout.trim();
                        if let Some(n) = line.strip_prefix("OK ") {
                            match n.trim().parse::<u64>() {
                                Ok(sz) if sz == expected => Ok(()),
                                Ok(sz) => Err(format!("size mismatch after copy: {sz}B on pod vs {expected}B local (partial / clobbered)")),
                                Err(_) => Ok(()), // couldn't parse size; don't false-fail
                            }
                        } else if line == "MISPLACED" {
                            Err(format!("{remote} is a directory on the pod — the file landed *inside* it; pass an explicit file DEST or remove that dir"))
                        } else {
                            Err(format!("nothing at {remote} after scp (silent non-write)"))
                        }
                    }
                    // If the verify probe itself can't run, don't override a successful scp.
                    _ => Ok(()),
                }
            } else {
                Ok(())
            };
            (name, res)
        });
    }
    let (mut ok, mut failed, mut done) = (0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, res)) = joined else { continue };
        match res {
            Ok(()) => {
                println!("[{done}/{total}] ✓ {name}");
                ok += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} copied, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to receive the file");
    }
    Ok(())
}

/// `pods copy-keys`: distribute API keys to each pod's shell. Per-host keys come from
/// `<keys_dir>/<provider>_api_keys.csv`; a Hugging Face token (from `--hf-token` or
/// config `HF_TOKEN`) is broadcast to every pod (for gated repos like Llama 3).
#[allow(clippy::too_many_arguments)]
async fn handle_copy_keys(
    provider: &dyn Provider,
    cfg: &Config,
    keys_dir: &str,
    hf_token: Option<String>,
    cc_token: Option<String>,
    include: &[String],
    exclude: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::apikeys;
    use arena_core::ssh::{self, SshTarget};
    use std::collections::HashMap;

    // Per-host vars from the CSVs: host -> [(ENV_NAME, value), …].
    let mut per_host: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut sources: Vec<String> = Vec::new();
    for (base, display, env_names) in apikeys::PROVIDERS {
        let path = format!("{}/{base}_api_keys.csv", keys_dir.trim_end_matches('/'));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rows = apikeys::parse_csv(&text);
        if rows.is_empty() {
            continue;
        }
        sources.push(format!("{display} ({} host{})", rows.len(), if rows.len() == 1 { "" } else { "s" }));
        for (host, key) in rows {
            let entry = per_host.entry(host).or_default();
            for env in *env_names {
                entry.push((env.to_string(), key.clone()));
            }
        }
    }

    // Broadcast tokens (HF, Claude Code): --hf-token overrides config HF_TOKEN; the rest
    // (e.g. CLAUDE_CODE_OAUTH_TOKEN) come from config.
    let token_value = |k: &str| -> Option<String> {
        let flag = match k {
            "HF_TOKEN" => hf_token.clone(),
            "CLAUDE_CODE_OAUTH_TOKEN" => cc_token.clone(),
            _ => None,
        };
        flag.or_else(|| cfg.get(k).filter(|s| !s.is_empty()).map(String::from))
    };
    let broadcast = apikeys::broadcast_env_vars(&token_value);
    // "broadcast" = same value on every *targeted* pod (vs per-host CSV keys). Say "all pods"
    // only when there's no include/target filter — otherwise it misleadingly implies the whole
    // fleet when you've restricted to specific pods.
    let bcast_scope = if include.is_empty() { "broadcast to all pods" } else { "broadcast" };
    for (key, display, _) in apikeys::BROADCAST_TOKENS {
        if token_value(key).is_some() {
            sources.push(format!("{display} ({bcast_scope})"));
        }
    }

    if per_host.is_empty() && broadcast.is_empty() {
        anyhow::bail!(
            "no keys to copy: put `<provider>_api_keys.csv` in {keys_dir}/ \
             (openai/anthropic/openrouter), set HF_TOKEN / CLAUDE_CODE_OAUTH_TOKEN, or pass --hf-token"
        );
    }
    println!("Key sources: {}\n", sources.join(", "));

    // Build the per-pod var set (broadcast HF merged into every reachable pod).
    let mut pods = provider.list_pods().await.context("listing pods for copy-keys")?;
    // Apply --exclude then --include (full name, bare short name, or id).
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    pods.retain(|p| !exclude.iter().any(|x| pod_matches(p, x, prefix)));
    if !include.is_empty() {
        pods.retain(|p| include.iter().any(|x| pod_matches(p, x, prefix)));
        if pods.is_empty() {
            anyhow::bail!("no pods matched --include {:?} (run `arena pods list`)", include);
        }
    }
    let mut jobs: Vec<(String, SshTarget, Vec<(String, String)>)> = Vec::new();
    // Reachable pods that matched NO per-host key — they'd get broadcast tokens only.
    // Worth flagging loudly: a stale/misnamed CSV (e.g. last cohort's hosts) otherwise
    // hides behind a per-pod ✓, so nobody notices the per-host keys never landed.
    let mut broadcast_only: Vec<String> = Vec::new();
    for pod in &pods {
        let mut vars = broadcast.clone();
        let matched_per_host =
            per_host.get(&pod.name).map(|h| vars.extend(h.iter().cloned())).is_some();
        if vars.is_empty() {
            continue; // nothing for this pod
        }
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => {
                if !matched_per_host && !per_host.is_empty() {
                    broadcast_only.push(pod.name.clone());
                }
                jobs.push((pod.name.clone(), t, vars));
            }
            Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
        }
    }
    if jobs.is_empty() {
        println!("(no reachable pods matched any keys)");
        return Ok(());
    }
    if !broadcast_only.is_empty() {
        eprintln!(
            "⚠ {} reachable pod(s) matched NO per-host key — broadcast tokens only: {}\n  \
             (per-host keys come from <provider>_api_keys.csv, matched on exact pod name; \
             check that file actually lists these hosts)\n",
            broadcast_only.len(),
            broadcast_only.join(", ")
        );
    }

    // Also wire fleet SSH on every reachable pod: authorize the fleet pubkeys (arena8 +
    // arena_infra + admin) and write the host map into ~/.ssh/config so pods can ssh each
    // other with the arena_infra key. Prefer the stable proxy layout (survives restarts);
    // fall back to live endpoints if no proxy is configured. Built once — same on every pod.
    let fleet_pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    let ssh_user = cfg.get("SSH_USER").unwrap_or("root");
    let pod_identity = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519");
    let fleet_cfg = match arena_core::proxy::ProxyConfig::from_config(cfg) {
        Ok(px) if !cfg.machine_names.is_empty() => arena_core::sshconfig::render_proxy(
            prefix, ssh_user, pod_identity, &px.proxy_host, px.starting_port, &cfg.machine_names,
        ),
        _ => arena_core::sshconfig::render_manual(prefix, ssh_user, pod_identity, &pods),
    };
    let fleet_ssh = fleet_ssh_command(&fleet_pubkeys, &fleet_cfg);
    println!(
        "Also on each target ({} pod(s)): authorize {} fleet key(s) + write that pod's own \
         ~/.ssh/config (the fleet host map, so it can ssh the others).",
        jobs.len(),
        fleet_pubkeys.len()
    );

    if dry_run {
        println!("Dry-run — would set on {} pod(s) (values redacted):\n", jobs.len());
        for (name, _, vars) in &jobs {
            let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
            println!("  {name:<22} {}", names.join(", "));
        }
        println!("\nPreview only — run without --dry-run to write to ~/.bashrc & ~/.zshrc.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will export API keys into ~/.bashrc & ~/.zshrc, authorize the fleet keys, and write ~/.ssh/config on {} pod(s).", jobs.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    for (name, t, vars) in jobs {
        // Tokens (export lines) + fleet SSH (authorized_keys + ~/.ssh/config) in one round-trip.
        let cmd = format!("{}; {}", apikeys::remote_export_command(&vars), fleet_ssh);
        set.spawn(async move { (name, ssh::run(&t, &cmd).await) });
    }
    let (mut ok, mut failed, mut done) = (0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, res)) = joined else { continue };
        match res {
            Ok(o) if o.success => {
                println!("[{done}/{total}] ✓ {name}");
                ok += 1;
            }
            Ok(o) => {
                println!("[{done}/{total}] ✗ {name}: {}", o.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} updated, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed");
    }
    Ok(())
}

/// Where generated OpenRouter keys are persisted (also where `copy-keys` reads them).
const OPENROUTER_KEYS_CSV: &str = "./keys/openrouter_api_keys.csv";

/// Full pod name for a machine arg (prefix added once; honors absolute `@name` list entries).
fn full_machine_name(prefix: &str, candidates: &[String], m: &str) -> String {
    arena_core::naming::canonical_name(prefix, candidates, m)
}

/// Resolve which machines (full pod names) a `keys` action targets: `--all` = every
/// current pod; else the explicitly named ones (prefixed).
async fn keys_targets(
    provider: &dyn Provider,
    prefix: &str,
    candidates: &[String],
    machines: &[String],
    all: bool,
) -> Result<Vec<String>> {
    if all {
        let pods = provider.list_pods().await.context("listing pods")?;
        Ok(pods.iter().map(|p| p.name.clone()).collect())
    } else if !machines.is_empty() {
        Ok(machines.iter().map(|m| full_machine_name(prefix, candidates, m)).collect())
    } else {
        anyhow::bail!("specify machine name(s) or --all")
    }
}

/// Persist one machine's freshly minted OpenRouter key into the per-host CSV (upsert).
/// A new file is seeded with a header naming the arena iteration (`prefix`) so the CSV
/// is self-documenting about which cohort the keys belong to.
fn write_openrouter_key(host: &str, secret: &str, prefix: &str) -> Result<()> {
    std::fs::create_dir_all("./keys").context("creating ./keys")?;
    let mut existing = std::fs::read_to_string(OPENROUTER_KEYS_CSV).unwrap_or_default();
    if existing.trim().is_empty() {
        existing = format!(
            "# OpenRouter API keys — arena iteration: {prefix}\n# host,key (one runtime key per machine; managed by `arena keys`)\n"
        );
    }
    let updated = arena_core::apikeys::upsert_csv(&existing, host, secret);
    std::fs::write(OPENROUTER_KEYS_CSV, updated)
        .with_context(|| format!("writing {OPENROUTER_KEYS_CSV}"))?;
    Ok(())
}

/// `arena keys`: manage OpenRouter runtime keys (generate / list / rotate / revoke) via
/// the provisioning API. Secrets are saved to `keys/openrouter_api_keys.csv` (which
/// `pods copy-keys` then distributes); rotation/revocation find a key by its name
/// (`<prefix>-<machine>`), so no local hash bookkeeping is needed.
async fn handle_keys(cmd: KeysCmd, provider: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    use arena_core::openrouter::{key_name, OpenRouter};

    // `which` just reads the local CSV — no provisioning key / network needed.
    if let KeysCmd::Which = cmd {
        let abs = std::fs::canonicalize(OPENROUTER_KEYS_CSV)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|d| d.join(OPENROUTER_KEYS_CSV.trim_start_matches("./")).display().to_string())
                    .unwrap_or_else(|_| OPENROUTER_KEYS_CSV.to_string())
            });
        match std::fs::read_to_string(OPENROUTER_KEYS_CSV) {
            Ok(text) => {
                let rows = arena_core::apikeys::parse_csv(&text);
                println!("OpenRouter keys file: {abs}");
                println!("  ✓ exists · {} key(s) for: {}", rows.len(),
                    rows.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>().join(", "));
                println!("\n(distribute with `arena pods copy-keys`; values not shown)");
            }
            Err(_) => {
                println!("OpenRouter keys file: {abs}");
                println!("  · not created yet — run `arena keys gen --all` to mint keys");
            }
        }
        return Ok(());
    }

    let prov_key = cfg
        .get("OPENROUTER_PROVISIONING_KEY")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "set OPENROUTER_PROVISIONING_KEY first (openrouter.ai → Settings → \
                 Provisioning API Keys), e.g. `arena config set OPENROUTER_PROVISIONING_KEY <key>`"
            )
        })?;
    let or = OpenRouter::new(prov_key);
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let default_limit = cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").unwrap_or(5.0);

    match cmd {
        KeysCmd::Which => unreachable!("handled before the provisioning-key check"),
        KeysCmd::List { all } => {
            let mut keys = or.list_keys().await.context("listing OpenRouter keys")?;
            keys.sort_by(|a, b| a.name.cmp(&b.name));
            if keys.is_empty() {
                println!("(no provisioned OpenRouter keys)");
                return Ok(());
            }
            // Default to this iteration's keys (named `<prefix>-…`); count the rest.
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let needle = format!("{prefix}-");
            let total = keys.len();
            let shown: Vec<_> = if all {
                keys.iter().collect()
            } else {
                keys.iter()
                    .filter(|k| k.name.as_deref().map(|n| n.starts_with(&needle)).unwrap_or(false))
                    .collect()
            };
            let excluded = total - shown.len();

            if shown.is_empty() {
                println!("(no {prefix}-* keys; {excluded} other key(s) hidden — use --all to show all)");
                return Ok(());
            }
            println!("{:<28} {:>9} {:>9}  {}", "NAME", "LIMIT", "USAGE", "HASH");
            for k in &shown {
                let lim = k.limit.map(|l| format!("${l:.2}")).unwrap_or_else(|| "-".into());
                let used = k.usage.map(|u| format!("${u:.2}")).unwrap_or_else(|| "-".into());
                let short = &k.hash[..k.hash.len().min(12)];
                let dis = if k.disabled { "  (disabled)" } else { "" };
                println!("{:<28} {lim:>9} {used:>9}  {short}{dis}", k.name.clone().unwrap_or_default());
            }
            if !all && excluded > 0 {
                println!("\n(excluded {excluded} non-{prefix} key(s) — use --all to show all)");
            }
        }

        KeysCmd::Gen { machines, all, limit, copy, dry_run } => {
            let names = keys_targets(provider, &prefix, &cfg.machine_names, &machines, all).await?;
            let limit = limit.unwrap_or(default_limit);
            if names.is_empty() {
                println!("(no target machines)");
                return Ok(());
            }
            if dry_run {
                println!("Dry-run — would mint a ${limit:.2}-cap key for {} machine(s):", names.len());
                for h in &names {
                    println!("  {}", key_name(&prefix, &cfg.machine_names, h));
                }
                return Ok(());
            }
            if !confirm(yes, &format!(
                "Will mint {} OpenRouter key(s) (cap ${limit:.2} each) and write {OPENROUTER_KEYS_CSV}.",
                names.len()
            ))? {
                println!("aborted.");
                return Ok(());
            }
            let existing = or.list_keys().await.context("listing existing keys")?;
            let (mut made, mut skipped, mut failed) = (0, 0, 0);
            for host in &names {
                let kn = key_name(&prefix, &cfg.machine_names, host);
                if existing.iter().any(|k| k.name.as_deref() == Some(&kn) && !k.disabled) {
                    println!("= {host}: '{kn}' already exists — use `arena keys rotate {host}` to replace");
                    skipped += 1;
                    continue;
                }
                match or.create_key(&kn, Some(limit)).await {
                    Ok(ck) => {
                        write_openrouter_key(host, &ck.secret, &prefix)?;
                        println!("✓ {host}: minted {}", &ck.hash[..ck.hash.len().min(12)]);
                        made += 1;
                    }
                    Err(e) => {
                        eprintln!("✗ {host}: {e}");
                        failed += 1;
                    }
                }
            }
            println!("\nGenerated {made}, skipped {skipped}, failed {failed} → {OPENROUTER_KEYS_CSV}");
            if copy && made > 0 {
                println!();
                handle_copy_keys(provider, cfg, "./keys", None, None, &names, &[], false, yes).await?;
            }
        }

        KeysCmd::Rotate { machine, all, limit, copy, dry_run } => {
            let names = keys_targets(provider, &prefix, &cfg.machine_names, &machine.into_iter().collect::<Vec<_>>(), all).await?;
            let limit = limit.unwrap_or(default_limit);
            if dry_run {
                println!("Dry-run — would delete + re-mint a key for {} machine(s):", names.len());
                for h in &names {
                    println!("  {}", key_name(&prefix, &cfg.machine_names, h));
                }
                return Ok(());
            }
            if !confirm(yes, &format!("Will DELETE + re-mint {} OpenRouter key(s) (cap ${limit:.2}).", names.len()))? {
                println!("aborted.");
                return Ok(());
            }
            let (mut ok, mut failed) = (0, 0);
            for host in &names {
                let kn = key_name(&prefix, &cfg.machine_names, host);
                // Delete the existing key (by name) if present, then mint a fresh one.
                match or.find_by_name(&kn).await {
                    Ok(Some(k)) => {
                        if let Err(e) = or.delete_key(&k.hash).await {
                            eprintln!("✗ {host}: delete old: {e}");
                            failed += 1;
                            continue;
                        }
                    }
                    Ok(None) => {} // nothing to delete; just create
                    Err(e) => {
                        eprintln!("✗ {host}: lookup: {e}");
                        failed += 1;
                        continue;
                    }
                }
                match or.create_key(&kn, Some(limit)).await {
                    Ok(ck) => {
                        write_openrouter_key(host, &ck.secret, &prefix)?;
                        println!("✓ {host}: rotated");
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("✗ {host}: create: {e}");
                        failed += 1;
                    }
                }
            }
            println!("\nRotated {ok}, failed {failed}.");
            if copy && ok > 0 {
                println!();
                handle_copy_keys(provider, cfg, "./keys", None, None, &names, &[], false, yes).await?;
            }
        }

        KeysCmd::Revoke { machine, all, dry_run } => {
            let names = keys_targets(provider, &prefix, &cfg.machine_names, &machine.into_iter().collect::<Vec<_>>(), all).await?;
            if dry_run {
                println!("Dry-run — would revoke the key for {} machine(s):", names.len());
                for h in &names {
                    println!("  {}", key_name(&prefix, &cfg.machine_names, h));
                }
                return Ok(());
            }
            if !confirm(yes, &format!("Will DELETE {} OpenRouter key(s) — no regenerate.", names.len()))? {
                println!("aborted.");
                return Ok(());
            }
            let (mut ok, mut missing, mut failed) = (0, 0, 0);
            for host in &names {
                let kn = key_name(&prefix, &cfg.machine_names, host);
                match or.find_by_name(&kn).await {
                    Ok(Some(k)) => match or.delete_key(&k.hash).await {
                        Ok(()) => {
                            println!("✓ {host}: revoked");
                            ok += 1;
                        }
                        Err(e) => {
                            eprintln!("✗ {host}: {e}");
                            failed += 1;
                        }
                    },
                    Ok(None) => {
                        println!("= {host}: no key named '{kn}'");
                        missing += 1;
                    }
                    Err(e) => {
                        eprintln!("✗ {host}: {e}");
                        failed += 1;
                    }
                }
            }
            println!("\nRevoked {ok}, none-found {missing}, failed {failed}. (The CSV is left as-is; rotate to refresh.)");
        }
    }
    Ok(())
}

/// `arena ssh-config`: print (and optionally write) the participant-facing
/// `~/.ssh/config` — direct pod endpoints, or stable proxy ports with `--proxy`.
async fn handle_ssh_config(
    provider: &dyn Provider,
    cfg: &Config,
    proxy: bool,
    out: Option<&std::path::Path>,
) -> Result<()> {
    use arena_core::sshconfig;

    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let user = cfg.get("SSH_USER").unwrap_or("root");
    // The identity file as the *participant* will reference it — keep the configured
    // value verbatim (do not resolve to this operator's local copy).
    let identity = cfg.get("SHARED_SSH_KEY_PATH").unwrap_or("~/.ssh/shared_infra_key");

    let rendered = if proxy {
        let px = arena_core::proxy::ProxyConfig::from_config(cfg)?;
        sshconfig::render_proxy(prefix, user, identity, &px.proxy_host, px.starting_port, &cfg.machine_names)
    } else {
        let pods = provider.list_pods().await.context("listing pods for ssh-config")?;
        sshconfig::render_manual(prefix, user, identity, &pods)
    };

    print!("{rendered}");
    if let Some(path) = out {
        std::fs::write(path, &rendered).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("\n(wrote {})", path.display());
    }
    Ok(())
}

/// Scenario tests for pod-selection control flow, driven by a fake `Provider` (no real
/// API/SSH). Covers the filter logic + the "--include matched nothing" guard that keeps a
/// typo'd target from silently looking like an idle fleet.
#[cfg(test)]
mod selection_tests {
    use super::select_pods;
    use arena_core::{Config, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;

    struct FakeProvider {
        pods: Vec<Pod>,
    }

    fn pod(name: &str, status: &str) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.to_string(),
            provider: "fake".into(),
            status: status.to_string(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(self.pods.clone())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised by selection tests")
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

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=arena8")
    }
    fn names(pods: &[Pod]) -> Vec<String> {
        pods.iter().map(|p| p.name.clone()).collect()
    }
    fn fleet() -> FakeProvider {
        FakeProvider {
            pods: vec![
                pod("arena8-bloom", "RUNNING"),
                pod("arena8-apple", "RUNNING"),
                pod("arena8-zebra", "EXITED"),
            ],
        }
    }

    #[tokio::test]
    async fn include_matching_nothing_errors_not_silent() {
        // The regression guard: a typo'd --include must fail loudly, not return an empty
        // set that reads like "nothing to do".
        let r = select_pods(&fleet(), &cfg(), &["arena8-zebrra".into()], &[], None).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("no pods matched --include"));
    }

    #[tokio::test]
    async fn no_filters_returns_all_sorted() {
        let got = select_pods(&fleet(), &cfg(), &[], &[], None).await.unwrap();
        assert_eq!(names(&got), ["arena8-apple", "arena8-bloom", "arena8-zebra"]);
    }

    #[tokio::test]
    async fn exclude_then_include_then_status_compose() {
        // exclude apple; include bloom+zebra; keep only RUNNING => bloom.
        let got = select_pods(
            &fleet(),
            &cfg(),
            &["arena8-bloom".into(), "arena8-zebra".into()],
            &["arena8-apple".into()],
            Some("RUNNING"),
        )
        .await
        .unwrap();
        assert_eq!(names(&got), ["arena8-bloom"]);
    }

    #[tokio::test]
    async fn bare_short_name_matches_via_prefix() {
        // "bloom" should resolve to arena8-bloom through MACHINE_NAME_PREFIX.
        let got = select_pods(&fleet(), &cfg(), &["bloom".into()], &[], None).await.unwrap();
        assert_eq!(names(&got), ["arena8-bloom"]);
    }

    #[tokio::test]
    async fn status_filter_matching_nothing_is_empty_not_error() {
        // No --include here, so an all-stopped fleet legitimately yields an empty set
        // (distinct from the typo case above) — must NOT error.
        let got = select_pods(&fleet(), &cfg(), &[], &[], Some("PROVISIONING")).await.unwrap();
        assert!(got.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_arena_block, with_arena_block, CRON_BEGIN, CRON_END};

    #[test]
    fn provisioning_steps_branch_by_provider() {
        use super::{provisioning_steps, ProvisionStep};
        let scfg = arena_core::setup::SetupConfig {
            key_local: "/local/key".into(),
            key_remote: "/root/.ssh/id_ed25519".into(),
            repo_path: "/root/ARENA_3.0".into(),
            repo_url: "git@github.com:o/r.git".into(),
            branch: "main".into(),
            prefix: "arena8".into(),
            authorized_pubkeys: vec![],
            broadcast_exports: vec![],
            zsh_install: false,
        };
        // bare-VM (hetzner): copy the deploy key, push the script, run it with the repo
        // URL/key passed in (so it clones the PRIVATE repo over SSH).
        assert_eq!(
            provisioning_steps("hetzner", &scfg, "arena8-flutter", false, "/tmp/h.sh"),
            vec![
                ProvisionStep::Scp { local: "/local/key".into(), remote: "/root/.ssh/id_ed25519".into() },
                ProvisionStep::Scp { local: "/tmp/h.sh".into(), remote: "/root/hetzner_setup.sh".into() },
                ProvisionStep::Run {
                    cmd: "REPO_URL='git@github.com:o/r.git' REPO_DIR='/root/ARENA_3.0' REPO_KEY='/root/.ssh/id_ed25519' bash /root/hetzner_setup.sh".into(),
                },
            ]
        );
        // image-based (runpod/vast): scp the deploy key, then a config command that
        // re-points origin — i.e. the post-image flow, not the bare-VM script.
        let r = provisioning_steps("runpod", &scfg, "arena8-apple", false, "/tmp/h.sh");
        assert!(matches!(&r[0], ProvisionStep::Scp { local, remote } if local == "/local/key" && remote == "/root/.ssh/id_ed25519"));
        assert!(matches!(&r[1], ProvisionStep::Run { cmd } if cmd.contains("git remote set-url")));
    }

    fn mk_pod(name: &str, provider: &str) -> arena_core::Pod {
        arena_core::Pod {
            id: name.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
        }
    }

    #[test]
    fn create_sizing_is_provider_scoped() {
        use super::{provider_pod_count, target_total, Want};
        // A realistic mixed fleet: many runpod GPU pods + a single hetzner pod. `list_pods()`
        // returns this whole thing, which is what makes provider-scoping load-bearing.
        let mut fleet: Vec<arena_core::Pod> =
            (0..22).map(|i| mk_pod(&format!("arena8-gpu{i}"), "runpod")).collect();
        fleet.push(mk_pod("arena8-flutter", "hetzner"));

        // Counting is per-provider, not whole-fleet.
        assert_eq!(provider_pod_count(&fleet, "hetzner", "arena8"), 1);
        assert_eq!(provider_pod_count(&fleet, "runpod", "arena8"), 22);

        // THE REGRESSION: `-a 1 --provider hetzner` targets hetzner's 1 + 1 = 2 (creates
        // exactly 1) — NOT the whole-fleet 23 + 1 that once made it try to create ~23 pods.
        assert_eq!(target_total(Want::Add(1), &fleet, "hetzner", "arena8"), 2);
        assert_eq!(target_total(Want::Add(2), &fleet, "runpod", "arena8"), 24);
        // `-n N` is an absolute total, independent of any other provider's pods.
        assert_eq!(target_total(Want::Total(30), &fleet, "hetzner", "arena8"), 30);
    }

    #[test]
    fn pod_count_excludes_other_prefixes_on_same_provider() {
        use super::provider_pod_count;
        let fleet = vec![
            mk_pod("arena8-flutter", "hetzner"),
            mk_pod("unrelated-box", "hetzner"), // same provider, different cohort prefix
            mk_pod("arena8-bloom", "hetzner"),
        ];
        assert_eq!(provider_pod_count(&fleet, "hetzner", "arena8"), 2);
    }

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

    #[test]
    fn copy_dest_mirrors_repo_path_or_falls_back() {
        use super::resolve_remote_dest;
        use std::path::Path;
        // explicit dest wins
        assert_eq!(
            resolve_remote_dest(Path::new("./x/y.py"), Some("/tmp/z.py"), "ARENA_3.0", "/root"),
            "/tmp/z.py"
        );
        // path through the repo dir mirrors under the repo parent
        assert_eq!(
            resolve_remote_dest(Path::new("./ARENA_3.0/ch1/ex/tests.py"), None, "ARENA_3.0", "/root"),
            "/root/ARENA_3.0/ch1/ex/tests.py"
        );
        // not under the repo -> basename (lands in home)
        assert_eq!(
            resolve_remote_dest(Path::new("/home/dev/notes.txt"), None, "ARENA_3.0", "/root"),
            "notes.txt"
        );
    }

    #[test]
    fn replace_stage_names_suffix_canonical() {
        use super::replace_stage_names;
        let (new, old) = replace_stage_names("arena8-apple");
        assert_eq!(new, "arena8-apple-new");
        assert_eq!(old, "arena8-apple-old");
    }
}
