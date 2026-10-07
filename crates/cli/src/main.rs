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
use clap::{Parser, Subcommand};

use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::{Naming, SelectArgs, Selector};
use arena_core::ssh::SshTarget;
use arena_core::{Config, PodSpec};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

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
    /// Manage OpenRouter API keys for the cohort (generate / list / rotate / revoke) via
    /// the provisioning API. Needs OPENROUTER_PROVISIONING_KEY in config.
    #[command(subcommand, infer_subcommands = true)]
    Keys(KeysCmd),
    /// List the known GPU types (names to pass to `--gpu`, with VRAM, $/hr and stock —
    /// live from RunPod where available, else the local presets).
    Gpus {
        /// Emit JSON instead of a table (for scripting). Includes every live catalog entry,
        /// with `creatable` flagging the ones the create API rejects (the table hides them).
        #[arg(long)]
        json: bool,
    },
    /// What multi-option placement would try (read-only; a dry run of `pods create/up`
    /// without the names): the --gpu × --cloud options with $/h per pod (RunPod's live
    /// prices, ~ = preset estimate), price source and stock hint, ordered and filtered by
    /// --max-price exactly as create does.
    Offers {
        /// GPU type(s), comma-separated, e.g. `A4000,4000Ada,3090` (default: config GPU_TYPE).
        #[arg(long)]
        gpu: Option<String>,
        /// Cloud tier(s), comma-separated, e.g. `community,secure` (default: config
        /// CLOUD_TYPE). Only RunPod has tiers.
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod: options above it, or with no known price, are dropped.
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// GPUs per pod (default: config NUM_GPUS) — prices and the cap are per pod.
        #[arg(long)]
        gpus: Option<u32>,
        /// `cheapest` (default) or `listed`.
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
        /// Emit the option plan as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
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

// ---- Target selection (PLAN 1.C) --------------------------------------------------------
//
// Every fleet command picks its pods with the same syntax, parsed and resolved by
// `arena_core::selector`: names (`apple`, `arena8-apple`, `@james-gpu`), provider ids,
// ranges `apple..mayor`, `all`/`--all`, filtered by `--exclude`, `--gpus N`, `--on
// <provider>`. Typos fail loudly. The flags live in one struct (`SelectOpts`); the targets
// themselves are positional (`Select`) except where a command's positionals are already
// taken — `run`'s command, `cp`'s file/dest, `pull`'s label — which take `-t/--target`
// (`SelectByFlag`). Plain `//` comments here: a doc comment on a flattened struct can leak
// into the subcommand's help text. Each command flattens its selection as its *last* field:
// the "Target selection" help heading carries over to every arg declared after it.

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
struct SelectOpts {
    /// Every pod (narrow it with --exclude / --gpus / --on).
    #[arg(long)]
    all: bool,
    /// Leave these pods out: same syntax as the targets, ranges too (repeatable, or
    /// comma-separated). A name that matches no pod is an error, not a no-op.
    #[arg(long, value_name = "TARGET")]
    exclude: Vec<String>,
    /// Only pods with exactly N GPUs (the provider's count; a pod whose count is unknown
    /// never matches).
    #[arg(long, value_name = "N")]
    gpus: Option<u32>,
    /// Only pods on this provider: runpod, vast or hetzner. (Not `--provider`, which picks
    /// where `create`/`up` make pods.)
    #[arg(long, value_name = "PROVIDER")]
    on: Option<String>,
    /// The old spelling of the targets, still accepted. With --all it keeps its old meaning
    /// "of all pods, only these" (`stop --all --include apple` = `stop apple`).
    #[arg(long, hide = true, value_name = "TARGET")]
    include: Vec<String>,
}

impl SelectOpts {
    /// The core selection for `targets` (positional or `-t`) plus these flags. The hidden
    /// `--include` joins the targets — and keeps its legacy narrowing of `--all`, so
    /// existing scripts do exactly what they did (any other targets + `--all` is refused).
    fn args(&self, targets: &[String]) -> SelectArgs {
        let legacy_narrowing = self.all && targets.is_empty() && !self.include.is_empty();
        SelectArgs {
            targets: targets.iter().chain(&self.include).cloned().collect(),
            all: self.all && !legacy_narrowing,
            exclude: self.exclude.clone(),
            gpus: self.gpus,
            on: self.on.clone(),
        }
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
struct Select {
    /// Pods: names (`apple`, `arena8-apple`, `@james-gpu`), provider ids, ranges
    /// `apple..mayor` (inclusive, MACHINE_NAME_LIST order) or `all`.
    #[arg(value_name = "TARGET")]
    targets: Vec<String>,
    #[command(flatten)]
    opts: SelectOpts,
}

impl Select {
    fn args(&self) -> SelectArgs {
        self.opts.args(&self.targets)
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
struct SelectByFlag {
    /// Only these pods (repeatable, or comma-separated): names (`apple`, `arena8-apple`,
    /// `@james-gpu`), provider ids, ranges `apple..mayor` (inclusive, MACHINE_NAME_LIST
    /// order) or `all`. Default: every pod.
    #[arg(long = "target", short = 't', value_name = "TARGET")]
    targets: Vec<String>,
    #[command(flatten)]
    opts: SelectOpts,
}

impl SelectByFlag {
    fn args(&self) -> SelectArgs {
        self.opts.args(&self.targets)
    }
}

#[derive(Subcommand)]
enum KeysCmd {
    /// Generate an OpenRouter runtime key per machine (named `<prefix>-<machine>`), each
    /// with a USD credit cap, and save to `keys/openrouter_api_keys.csv`. Skips machines
    /// that already have a key (use `rotate`). `--copy` also distributes them to the pods.
    /// Targets are pods or MACHINE_NAME_LIST names (a key can be minted before its pod
    /// exists); `--all` = every current pod.
    Gen {
        /// USD credit cap per key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After generating, copy the key(s) onto the pod(s) (runs copy-keys for them).
        #[arg(long)]
        copy: bool,
        /// Preview only: show what would be created, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
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
    /// Rotate machines' keys — delete each and generate a fresh one (e.g. after a leak).
    /// Updates the CSV; `--copy` re-pushes to the pod(s). Targets as for `gen`, or --all.
    Rotate {
        /// USD credit cap for the new key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After rotating, copy the new key(s) onto the pod(s).
        #[arg(long)]
        copy: bool,
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Revoke (delete) machines' keys without regenerating. Targets as for `gen`, or --all.
    Revoke {
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
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
        /// Also re-sync the proxy every 5 minutes (`proxy apply --yes`, logged to
        /// ~/arena-proxy-cron.log): catches what the CLI didn't do itself — a pod
        /// terminated from the dashboard, a restart that moved an SSH endpoint.
        #[arg(long)]
        proxy: bool,
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
    /// Compute the proxy plan — the current config merged with every provider's listing
    /// (`+ ~ - =` per machine: added / changed / removed / kept-stale) — and print the
    /// nginx `stream` config (stable public port -> each pod's current SSH endpoint).
    /// Read-only: never connects to the proxy (a remote proxy's diff isn't shown).
    Plan {
        /// Also write the rendered nginx config to this local path for review.
        /// (Local only — this never copies anything to the proxy host.)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Merge the fleet's listing into the live proxy config, show the changes, then write
    /// it and run SSH_PROXY_RELOAD_CMD (default `nginx -t && nginx -s reload`; empty =
    /// write-only). A forward is only removed once its pod is confirmed gone; nothing is
    /// written if no provider answered. Acts by default; --dry-run to preview.
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
        /// Emit JSON instead of a table (for scripting): the pods, including gpu_count,
        /// cost_per_hr and the host maintenance window where the provider reports them.
        #[arg(long)]
        json: bool,
        /// Force the GPU-via-SSH probe (default for `--json`, which skips it otherwise).
        #[arg(long)]
        probe: bool,
        /// Skip the GPU-via-SSH probe (faster; GPU shows only what the provider reports,
        /// else "-"). The table view probes by default: `nvidia-smi` on the machine is the
        /// ground truth, and some listings omit the GPU.
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
        /// GPU type, e.g. `A4000`, `3090`, `A100`, `A100 SXM` (or a full provider name) — or
        /// a comma list of fallbacks (`A4000,4000Ada,3090`): each name tries the options one
        /// create at a time, in --order, until one has capacity (see `arena offers`).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod cloud tier: COMMUNITY or SECURE (overrides config) — or a comma list
        /// (`community,secure`) to fall back across tiers.
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod (per-GPU price × --gpus). Options above it, or with no known
        /// price, are not tried. Prices: RunPod's live catalog (else preset estimates).
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// Order to try several --gpu/--cloud options in: `cheapest` (by $/h per pod; a tie
        /// goes to the better stock hint, then the listed order) or `listed` (GPU-major).
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
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
        /// Don't sync the proxy afterwards. By default `create` re-syncs it once the pods
        /// exist (best-effort; pods still booting get their forward on a later sync).
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// On capacity exhaustion, wait and keep retrying instead of stopping (one GPU/cloud
        /// only — with several options use --retry-mins).
        #[arg(long)]
        keep_trying: bool,
        /// Keep re-attempting (topping up to the target) for up to this many minutes,
        /// e.g. while waiting for capacity. 0 = a single attempt. Ctrl+C stops early. With
        /// several options, each round re-tries every option.
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
        /// GPU type, e.g. `A4000`, `3090`, `A100`, `A100 SXM` (or a full provider name) — or
        /// a comma list of fallbacks (`A4000,4000Ada,3090`): each name tries the options one
        /// create at a time, in --order, until one has capacity (see `arena offers`).
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod cloud tier: COMMUNITY or SECURE (overrides config) — or a comma list
        /// (`community,secure`) to fall back across tiers.
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod (per-GPU price × --gpus). Options above it, or with no known
        /// price, are not tried. Prices: RunPod's live catalog (else preset estimates).
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// Order to try several --gpu/--cloud options in: `cheapest` (by $/h per pod; a tie
        /// goes to the better stock hint, then the listed order) or `listed` (GPU-major).
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
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
        /// On capacity exhaustion, wait and keep retrying instead of stopping (one GPU/cloud
        /// only — with several options use --retry-mins).
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
    ///   4. point the ARENA repo's origin at GitHub, fetch the default branch only (no
    ///      tags, not every participant's branch), and update the branch (stay on the
    ///      current branch by default; --force checks out the default branch and
    ///      hard-resets);
    ///   5. update submodules;
    ///   6. write ~/.name (export MACHINE_NAME=…);
    ///   7. (optional) export any broadcast tokens that are set — Hugging Face
    ///      (HF_TOKEN + HUGGING_FACE_HUB_TOKEN) and Claude Code (CLAUDE_CODE_OAUTH_TOKEN)
    ///      — into ~/.bashrc & ~/.zshrc; tokens not set are skipped.
    ///
    /// Pods run in parallel, each step on a time budget (copies 60s; the config step
    /// 300s, the hetzner bare-VM script 1800s — see --timeout / SETUP_TIMEOUT_SECS). A
    /// stuck pod reports `✗ <name> (timed out at <step> after Ns)`; the others carry on.
    ///
    /// It does NOT distribute per-host API keys (use `pods copy-keys`) or back anything
    /// up (use `pods backup` / `pods pull`).
    ///
    /// Targets: names / ids / ranges / `all` (see "Target selection"); none = the whole fleet.
    #[command(verbatim_doc_comment)]
    Setup {
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
        /// shared arena-infra dotfiles, set as the login shell (no conda/ARENA repo). For
        /// bare / non-arena base images — the prebuilt arena image already has this.
        #[arg(long)]
        zsh_install: bool,
        /// Per-pod budget in seconds for the main provisioning command (default 300 for
        /// image-based pods, 1800 for the hetzner bare-VM script; overrides config
        /// SETUP_TIMEOUT_SECS; at most 86400). A pod that runs over reports `timed out at
        /// <step>`; the others carry on.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: Option<u64>,
        #[command(flatten)]
        sel: Select,
    },
    /// Stop the selected running pods: names / ids / ranges, or --all (narrowed by
    /// --exclude / --gpus / --on). Needs targets or --all.
    ///
    /// On RunPod (and Vast, unverified) a stopped pod keeps NO data: its container disk is
    /// discarded and it starts again as a fresh image — only a persistent volume at
    /// /workspace survives. A pod with no volume (or none confirmed) needs --wipe-ok.
    /// Hetzner VMs keep their disk.
    Stop {
        /// Stop even pods whose container disk would be discarded with no volume to keep
        /// anything (required for them, even with --yes). Back them up first.
        #[arg(long)]
        wipe_ok: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Restart a pod in place — on RunPod this WIPES its container disk.
    ///
    /// RunPod's restart resets the container to its image: everything outside a persistent
    /// volume (/workspace) is gone — participants' files, ~/.name, setup's git remote, the
    /// distributed keys. A pod with no volume (or none confirmed) is refused unless
    /// --wipe-ok (even with --yes); Vast (stop+start) is treated the same. Afterwards it
    /// waits for the SSH endpoint, re-runs `setup` on the pod (--no-setup skips) and syncs
    /// the proxy, so it comes back usable. Hetzner's hard reset keeps the VM disk.
    Restart {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Restart even though the container disk is wiped and there's no volume to keep
        /// anything (required then, even with --yes). Back the pod up first.
        #[arg(long)]
        wipe_ok: bool,
        /// Don't re-run setup on the pod afterwards (it comes back as the bare image).
        #[arg(long)]
        no_setup: bool,
        /// Don't sync the proxy afterwards.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Rename a pod (metadata only — the container is NOT restarted), then re-point the proxy.
    ///
    /// `rename <old> <new>` renames one pod; `<new>` must be a free MACHINE_NAME_LIST name
    /// (bare `apple` or full `arena9-apple`). `rename --from-prefix arena8` renames every
    /// `arena8-<x>` to `{MACHINE_NAME_PREFIX}-<x>` — for a cohort prefix change. Every rename
    /// is validated up front; nothing is touched if any of them is invalid.
    ///
    /// After the provider rename: rewrites ~/.name on each pod over SSH (an unreachable pod
    /// is reported — re-run `pods setup <name>` later), moves the machine's OpenRouter key
    /// mapping (its row in keys/openrouter_api_keys.csv and, with
    /// OPENROUTER_PROVISIONING_KEY, the key's name on OpenRouter), then syncs the proxy. The
    /// pod's MACHINE_NAME env var keeps the old name: env only changes with a reimage.
    Rename {
        /// Current machine name (e.g. arena8-apple) or raw provider id.
        #[arg(required_unless_present = "from_prefix", conflicts_with = "from_prefix")]
        target: Option<String>,
        /// New machine name (must be in MACHINE_NAME_LIST and not taken).
        #[arg(required_unless_present = "from_prefix")]
        new_name: Option<String>,
        /// Rename every `<from_prefix>-<x>` pod to `{MACHINE_NAME_PREFIX}-<x>`.
        #[arg(long)]
        from_prefix: Option<String>,
        /// Don't redeploy the proxy afterwards.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Reimage pods in place: swap the image + re-seed SSH keys, WIPING the container disk.
    ///
    /// Same host/id/name, fresh container from `--image` (default RUNPOD_DOCKER_IMAGE).
    /// PUBLIC_KEY is re-seeded from the current config keys and MACHINE_NAME set to the
    /// pod's name; other env is kept. Nothing is copied over — use `replace` to keep files.
    /// Afterwards waits for SSH endpoints and redeploys the proxy (ports can change).
    ///
    /// Targets: names / ids / ranges, or --all (narrowed by --exclude / --gpus / --on).
    Reimage {
        /// Image to use (default: RUNPOD_DOCKER_IMAGE from config).
        #[arg(long)]
        image: Option<String>,
        /// Don't wait for endpoints / redeploy the proxy afterwards.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
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
    ///
    /// Deliberately one pod at a time (no ranges or lists — irreversible): the target is
    /// matched like every other command's (name, bare name, `@name` or id), and a name two
    /// pods share is refused (pass the id).
    Terminate {
        /// Machine name (e.g. arena8-apple) or raw provider id. Omit with --all.
        target: Option<String>,
        /// Terminate every pod the provider reports (the whole fleet).
        #[arg(long, conflicts_with = "target")]
        all: bool,
        /// Also revoke each terminated machine's OpenRouter key (every key named after it)
        /// and drop its row from keys/openrouter_api_keys.csv. Needs
        /// OPENROUTER_PROVISIONING_KEY. A failed revoke is reported and makes the exit
        /// non-zero, but never stops a terminate.
        #[arg(long)]
        revoke_key: bool,
        /// Don't sync the proxy afterwards (the forward stays until the next
        /// `arena proxy apply`).
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Full backup: git-push each pod's current branch (skips main/master) AND rsync its
    /// home to the local backups folder (`pull`). Targets: names / ids / ranges (none = every
    /// reachable pod). --no-pull = git only.
    Backup {
        /// Only do the git push; skip the rsync file backup.
        #[arg(long)]
        no_pull: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Commit message (default: "arena backup <machine>").
        #[arg(long)]
        message: Option<String>,
        #[command(flatten)]
        sel: Select,
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
        #[command(flatten)]
        sel: SelectByFlag,
    },
    /// Create + push each pod's wNdM autocommit branch (no commit). Targets: names / ids /
    /// ranges (none = every reachable pod).
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
        #[command(flatten)]
        sel: Select,
    },
    /// Switch pods' branch (gentle ff-only; --hard force-resets to origin). Targets after
    /// the branch: names / ids / ranges, or --all (every pod with an SSH endpoint).
    SetBranch {
        /// Branch to check out (e.g. main, or a feature branch).
        branch: String,
        /// DESTRUCTIVE: hard-reset the branch to `origin/<branch>`, discarding local
        /// commits/changes (untracked files are left alone).
        #[arg(long)]
        hard: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Run a shell command on every pod over SSH (concurrent).
    ///
    /// Runs inside an interactive shell with the conda env active (default `arena-env`,
    /// override via `CONDA_ENV`; set it empty to disable), so commands see the
    /// participants' python/packages and the token exports written by `setup`. Flags go
    /// BEFORE the command — everything after it is the command. Scope it with
    /// `-t <targets>` (names / ids / ranges) and the other selection flags; default: every pod.
    Run {
        /// The command to run (everything after `run`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        /// Per-pod budget in seconds (default 1800 = 30 min; at most 86400). A pod that
        /// runs over reports `✗ timed out after Ns` and counts as failed; the others carry on.
        #[arg(long, default_value_t = RUN_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: u64,
        /// Preview only: print the command + target pods, run nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: SelectByFlag,
    },
    /// Health check (read-only). Plain: torch version on every pod (90s budget per pod).
    ///
    /// `--deep`: per-GPU CUDA tensor op, driver vs the configured floor
    /// (`MIN_DRIVER_VERSION`, else derived from `ALLOWED_CUDA_VERSIONS`), torch device
    /// count vs nvidia-smi, GPU↔GPU copy + NCCL all_reduce (only with >1 GPU), Hugging
    /// Face download speed, disk, host load, maintenance window. One pass/warn/fail row
    /// per pod (150s budget each); exits non-zero if any pod FAILs (warnings don't).
    ///
    /// Targets (both checks): names / ids / ranges (none = every pod with an SSH endpoint).
    Test {
        /// Run the deep check instead of the torch-version check.
        #[arg(long)]
        deep: bool,
        /// Emit per-pod `{name, provider, status, checks[], facts}` as JSON on stdout.
        #[arg(long, requires = "deep")]
        json: bool,
        /// Print every check of every pod, not just the table.
        #[arg(short, long, requires = "deep")]
        verbose: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Distribute API keys to pods' shells (per-host CSVs + broadcast HF token). Targets:
    /// names / ids / ranges (none = every reachable pod).
    CopyKeys {
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
        /// Preview only: print what would be set per pod (key values redacted).
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
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
        /// Per-pod scp budget in seconds (default: 10 min plus 1s per MB sent across all
        /// pods — every copy shares your uplink; at most 86400). A pod that runs over
        /// reports `✗ timed out after Ns` and counts as failed; the others carry on.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: Option<u64>,
        /// Preview only: print the scp commands, copy nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: SelectByFlag,
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

/// A create that failed part-way. `created` are the pods made before the failure: they
/// exist (and bill) whatever happened next, so the caller still finishes its job for them
/// — the proxy sync — before reporting `error`.
#[derive(Debug)]
struct CreateFailed {
    created: Vec<arena_core::Pod>,
    error: anyhow::Error,
}

impl CreateFailed {
    fn new(created: Vec<arena_core::Pod>, error: anyhow::Error) -> Self {
        Self { created, error }
    }
}

/// Top up toward the target, retrying for up to `retry_mins` (rounds every
/// `retry_secs`) while capacity is short — Ctrl+C stops the loop early and keeps what
/// was made. With `retry_mins == 0` it's a single attempt (honoring `keep_trying`).
/// Returns every pod created across all rounds — on failure, inside [`CreateFailed`].
async fn create_with_retry(
    provider: &dyn Provider,
    cfg: &Config,
    initial: Vec<String>,
    target: usize,
    ov: &SpecOverrides,
    keep_trying: bool,
    retry_mins: u64,
    retry_secs: u64,
) -> std::result::Result<Vec<arena_core::Pod>, CreateFailed> {
    // Round 1 creates exactly the names that were previewed + confirmed, so the
    // `[created] …` output matches the `[y/N]` prompt. `target` (a provider-scoped total,
    // computed once in plan_create) bounds the whole operation: retries only ever re-plan
    // toward it, so a top-up can never balloon past what the operator agreed to.
    let retry_secs = retry_secs.max(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(retry_mins * 60);
    let mut all: Vec<arena_core::Pod> = Vec::new();
    let mut names = initial;
    let shown = if target == 0 { names.len() } else { target };
    let mut round = 0u32;
    loop {
        round += 1;
        if names.is_empty() {
            break; // target reached (or no free names left)
        }
        // In retry mode the *loop* is the keep-trying, so each round is one-shot.
        let kt = retry_mins == 0 && keep_trying;
        let made = match create_pods(provider, cfg, &names, kt, ov).await {
            Ok(made) => made,
            Err(CreateFailed { created, error }) => {
                all.extend(created);
                return Err(CreateFailed::new(all, error));
            }
        };
        let got = made.len();
        all.extend(made);
        if got == names.len() || retry_mins == 0 {
            break; // filled what this round needed, or no retry requested
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("retry window ({retry_mins}m) elapsed — have {} of {shown}", all.len());
            break;
        }
        eprintln!(
            "round {round}: {} short of target {shown} (capacity); retrying in {retry_secs}s (Ctrl+C to stop)…",
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
        // Explicit names (target == 0) just retry the ones not created yet.
        names = if target == 0 {
            names.into_iter().filter(|n| !all.iter().any(|p| &p.name == n)).collect()
        } else {
            match plan_create(provider, cfg, Want::Total(target)).await {
                Ok(plan) => plan.names,
                Err(e) => return Err(CreateFailed::new(all, e)),
            }
        };
    }
    Ok(all)
}

async fn create_pods(
    provider: &dyn Provider,
    cfg: &Config,
    names: &[String],
    keep_trying: bool,
    ov: &SpecOverrides,
) -> std::result::Result<Vec<arena_core::Pod>, CreateFailed> {
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
                        let error = anyhow::anyhow!("authentication failed creating {name}: {e}");
                        return Err(CreateFailed::new(created, error));
                    }
                    _ => {
                        eprintln!("created {}/{} before failure", created.len(), names.len());
                        return Err(CreateFailed::new(created, anyhow::anyhow!("creating {name}: {e}")));
                    }
                },
            }
        }
    }
    println!("\nCreated {} of {} requested.", created.len(), names.len());
    Ok(created)
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
    let req =
        arena_core::placement::Request::from_flags(ov.gpu.as_deref(), ov.cloud.as_deref(), &base, max_price, order)?;
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
    match tokio::time::timeout(PRICE_TIMEOUT, fetch).await {
        Ok(r) => r,
        Err(_) => Err(arena_core::Error::provider(format!("timed out after {}s", PRICE_TIMEOUT.as_secs()))),
    }
}

/// The prices placement can see (read-only), plus notes on where they came from. RunPod:
/// the live catalog — on `RUNPOD_API=v2` one `/v2/catalog/gpus` per requested tier (its stock
/// is per tier), else / failing that the GraphQL `gpuTypes` (Phase 0.D) — backed by the preset
/// estimates. Vast/Hetzner quote nothing before create.
async fn fetch_price_book(
    cfg: &Config,
    provider_name: &str,
    clouds: &[String],
) -> Result<(arena_core::placement::PriceBook, Vec<String>)> {
    use arena_core::placement::PriceBook;
    use arena_core::provider::{runpod, runpod_v2, RunpodApi};
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
            match bounded_catalog(runpod_v2::fetch_gpu_types(key, cloud)).await {
                Ok(types) if !types.is_empty() => catalogs.push((Some(cloud.clone()), types)),
                Ok(_) => errs.push(format!("v2 catalog ({cloud}): empty")),
                Err(e) => errs.push(format!("v2 catalog ({cloud}): {e}")),
            }
        }
    }
    if catalogs.is_empty() {
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
/// behind `arena offers`, the create/up dry-runs and their confirm prompt.
async fn plan_placement(
    cfg: &Config,
    provider_name: &str,
    req: &arena_core::placement::Request,
) -> Result<arena_core::placement::OptionPlan> {
    req.validate_for(provider_name)?;
    let (book, notes) = fetch_price_book(cfg, provider_name, &req.clouds).await?;
    let mut plan = arena_core::placement::plan_options(req, provider_name, &book);
    plan.notes.splice(0..0, notes);
    Ok(plan)
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
/// goes, then the per-name summary. Same contract as [`create_with_retry`]: every pod made,
/// or — on an auth/other failure — those pods inside [`CreateFailed`], so the caller still
/// syncs the proxy for them.
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
) -> std::result::Result<Vec<arena_core::Pod>, CreateFailed> {
    use arena_core::placement::{self, End, Progress, Rounds, TopUp};
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
    };
    let ctrl_c = || async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let run = placement::place(provider, base, &names, plan, rounds, topup.as_ref(), ctrl_c, &mut show).await;
    print!("\n{}", placement::render_summary(plan, &run));
    let (placed, total) = (run.created.len(), names.len());
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
            return Err(CreateFailed::new(run.created, error));
        }
        End::Failed { name, error } => {
            eprintln!("placed {placed}/{total} before failure");
            let error = match name {
                Some(name) => anyhow::anyhow!("creating {name}: {error}"),
                None => anyhow::Error::new(error),
            };
            return Err(CreateFailed::new(run.created, error));
        }
    }
    Ok(run.created)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;

    // `config check` must work even when a provider key is missing (that's what it's
    // for), so build the provider lazily — only for commands that actually talk to one.
    let provider = match cli.cmd {
        Cmd::Config(_) | Cmd::Cron(_) | Cmd::Tui | Cmd::Gpus { .. } | Cmd::Offers { .. } => None,
        // Fleet-wide: every command spans all configured providers (create still targets
        // --provider). One configured backend behaves like that single provider.
        _ => Some(arena_core::provider::build_fleet(&cli.provider, &cfg, true)?),
    };
    // How every pod-SSH path reaches pods — one for the whole run: real ssh/scp here,
    // `FakeRemote` in tests. Each call carries its own budget (see the `*_TIMEOUT`s).
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::SshRemote);

    match cli.cmd {
        Cmd::Tui => launch_tui(&cli.provider, &cli.config),
        Cmd::Plan(c) => handle_plan(c, provider.unwrap().as_ref(), &cfg).await,
        Cmd::Config(c) => handle_config(c, &cfg, &cli.provider, &cli.config),
        Cmd::Cron(c) => handle_cron(c, &cli.config).await,
        Cmd::Pods(p) => handle_pods(p, provider.unwrap().as_ref(), remote, &cfg, cli.yes).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.unwrap().as_ref(), &cfg, cli.yes).await,
        Cmd::SshConfig { proxy, out } => {
            handle_ssh_config(provider.unwrap().as_ref(), &cfg, proxy, out.as_deref()).await
        }
        Cmd::Keys(k) => handle_keys(k, provider.unwrap().as_ref(), remote, &cfg, cli.yes).await,
        Cmd::Gpus { json } => handle_gpus(&cfg, &cli.provider, json).await,
        Cmd::Offers { gpu, cloud, max_price, gpus, order, json } => {
            handle_offers(&cfg, &cli.provider, gpu, cloud, max_price, gpus, order, json).await
        }
    }
}

/// `arena gpus`: list the GPU types you can pass to `--gpu`. Fetches RunPod's **full,
/// live** catalog with live community/secure prices + stock when on RunPod with a key —
/// via GraphQL on `RUNPOD_API=v1`, via REST `GET /v2/catalog/gpus` (stock for the configured
/// cloud tier) on `v2`, falling back to GraphQL if that fails; otherwise falls back to the
/// local curated presets. `--json` emits the same rows machine-readably
/// (`arena_core::gpu::GpuRow`). Diagnostics go to stderr so the JSON on stdout stays
/// parseable.
async fn handle_gpus(cfg: &Config, provider_name: &str, json: bool) -> Result<()> {
    use arena_core::gpu;
    use arena_core::provider::{runpod, runpod_v2, RunpodApi};

    // (rows, whether the create-API enum was available to flag `creatable`)
    let mut live: Option<(Vec<gpu::GpuRow>, bool)> = None;
    // Which cloud the STOCK column describes (v2's availability is per tier).
    let mut stock_cloud: Option<&str> = None;
    if provider_name == "runpod" {
        if let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) {
            let api = RunpodApi::from_config(cfg)?;
            let fetched = match api {
                RunpodApi::V1 => runpod::fetch_gpu_types(key).await,
                RunpodApi::V2 => {
                    let cloud = runpod_v2::normalize_cloud(&PodSpec::from_config(cfg).cloud_type).unwrap_or("COMMUNITY");
                    match runpod_v2::fetch_gpu_types(key, cloud).await {
                        Ok(types) if !types.is_empty() => {
                            stock_cloud = Some(cloud);
                            Ok(types)
                        }
                        other => {
                            let why = other.err().map_or_else(|| "empty catalog".to_string(), |e| e.to_string());
                            eprintln!("(v2 GPU catalog unavailable: {why} — trying the GraphQL catalog)\n");
                            runpod::fetch_gpu_types(key).await
                        }
                    }
                }
            };
            match fetched {
                Ok(types) if !types.is_empty() => {
                    // v1 only: RunPod's v1 create-validation enum can be NARROWER than the
                    // gpuTypes catalog — listing a GPU that `--gpu` then gets a 400 for. Flag
                    // rows against it (the table hides rejected ones). If the enum can't be
                    // fetched, `creatable` stays unknown and everything is shown. v2's
                    // `gpu.id` has no such enum (and the v1 document is retiring), so on v2
                    // `creatable` is unknown.
                    let creatable = match api {
                        RunpodApi::V1 => runpod::fetch_creatable_gpu_ids(key).await.ok().filter(|v| !v.is_empty()),
                        RunpodApi::V2 => None,
                    };
                    live = Some((gpu::rows_from_live(&types, creatable.as_deref()), creatable.is_some()));
                }
                Ok(_) => {}
                Err(e) => eprintln!("(couldn't fetch the live GPU list: {e} — showing local presets)\n"),
            }
        }
    }

    if json {
        let rows = live.map(|(rows, _)| rows).unwrap_or_else(gpu::rows_from_presets);
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    match live {
        Some((rows, creatable_known)) => {
            let (shown, hidden): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| r.creatable != Some(false));
            print!("{}", gpu::render_gpu_table(&shown));
            println!(
                "\n{} GPU types {}. Pass the API name (or a short alias like \
                 A4000 / 3090) to --gpu. Prices are RunPod's live $/hr per GPU (~ = preset \
                 estimate, - = not offered); STOCK is RunPod's 1-GPU stock hint{}.",
                shown.len(),
                if creatable_known { "(live from RunPod, creatable via the create API)" } else { "(live from RunPod)" },
                stock_cloud.map(|c| format!(" on the {c} cloud")).unwrap_or_default()
            );
            if !hidden.is_empty() {
                println!(
                    "({} more in RunPod's catalog are hidden — listed but rejected by the create API, so --gpu can't use them; see --json.)",
                    hidden.len()
                );
            }
        }
        None => {
            // Fallback: the curated presets (no network / non-RunPod).
            print!("{}", gpu::render_gpu_table(&gpu::rows_from_presets()));
            println!(
                "\n(local presets, ~ = rough estimate — run with --provider runpod + a key for the full live list.)\n\
                 Pass the API name, label, or alias (e.g. `A4000`, `3090`, \"A100 SXM\") to --gpu."
            );
        }
    }
    Ok(())
}

/// `arena offers`: the placement plan for these flags, read-only — the same planner
/// create/up run, so it previews what they would try (minus the names). Unlike `pods`, it
/// needs no fleet: only config (defaults) and, on RunPod, the key for live prices. `--json`
/// prints the plan (`arena_core::placement::OptionPlan`, notes included) and nothing else on
/// stdout.
#[allow(clippy::too_many_arguments)]
async fn handle_offers(
    cfg: &Config,
    provider_name: &str,
    gpu: Option<String>,
    cloud: Option<String>,
    max_price: Option<f64>,
    gpus: Option<u32>,
    order: arena_core::placement::Order,
    json: bool,
) -> Result<()> {
    if !["runpod", "vast", "hetzner"].contains(&provider_name) {
        anyhow::bail!("unknown provider `{provider_name}` (known: runpod, vast, hetzner)");
    }
    let base = spec_with_overrides(cfg, &SpecOverrides { gpus, ..Default::default() });
    let req = arena_core::placement::Request::from_flags(gpu.as_deref(), cloud.as_deref(), &base, max_price, order)?;
    let plan = plan_placement(cfg, provider_name, &req).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    println!("{}", plan_header(&plan));
    print!("{}", arena_core::placement::render_plan(&plan));
    println!(
        "\n`pods create/up` with these flags try the options top to bottom per name, one create at \
         a time; a pool that reports no capacity is skipped for the rest of that round. STOCK is a \
         hint only — creating is the real test."
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

/// One pod's provisioning job for [`provision_fleet`].
struct SetupJob {
    name: String,
    target: arena_core::ssh::SshTarget,
    steps: Vec<arena_core::setup::ProvisionStep>,
}

/// Provision every job concurrently over `remote`, emitting a `[done/total] ✓/✗ name`
/// line as each pod finishes (scp+ssh is slow serially). Each pod runs under its own
/// per-step budgets (see `arena_core::setup::provision`), so a wedged pod reports
/// `timed out at <step>` and never holds up the others — the whole run takes as long as
/// the slowest pod's budget, not forever. The flow is *data* (`provisioning_steps`) run
/// by a generic runner — nothing here names a provider. Returns the names of the pods
/// that were fully provisioned (in finishing order) and the number that failed — the
/// caller only follows up (API keys) on the former, never on a pod that just timed out.
async fn provision_fleet(
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    jobs: Vec<SetupJob>,
    boot: arena_core::setup::BootRetry,
    mut emit: impl FnMut(&str),
) -> (Vec<String>, usize) {
    use arena_core::setup::{progress_line, provision};
    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    // Task id -> pod name, so even a task that panicked is reported by name.
    let mut names = std::collections::HashMap::new();
    for job in jobs {
        let remote = remote.clone();
        let name = job.name.clone();
        let handle = set.spawn(async move {
            let outcome = provision(remote.as_ref(), &job.target, &job.steps, boot).await;
            (job.name, outcome)
        });
        names.insert(handle.id(), name);
    }
    let (mut provisioned, mut failed, mut done) = (Vec::new(), 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        match joined {
            Ok((name, outcome)) => {
                emit(&progress_line(done, total, &name, &outcome));
                if outcome.is_done() {
                    provisioned.push(name);
                } else {
                    failed += 1;
                }
            }
            // A panicked task still counts — as a failure, never silently.
            Err(e) => {
                failed += 1;
                let name = names.get(&e.id()).map(String::as_str).unwrap_or("?");
                emit(&format!("[{done}/{total}] ✗ {name} (its setup task crashed: {e})"));
            }
        }
    }
    (provisioned, failed)
}

/// How long one pod gets for the post-setup API-key + fleet-SSH write (`copy-keys`): a
/// few greps/appends to rc files and `~/.ssh/config` — seconds on a healthy pod, so a
/// minute means a wedged one, which must not hang the command (see `handle_copy_keys`).
const COPY_KEYS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Where `keys gen` writes, and `setup` / `copy-keys` read, the per-host API-key CSVs.
const KEYS_DIR: &str = "./keys";

// Per-call SSH budgets (PLAN 1.B). Every pod-SSH call is bounded so one wedged pod can
// never hang a fleet command: it reports `✗ <name>: timed out after Ns`, counts as a
// failure, and the others carry on. Quick probes share `arena_core::remote::PROBE_TIMEOUT`.

/// `pods test`: a cold `import torch` (CUDA libs off a cold disk) takes 10–30s, so 90s
/// means the pod is wedged — and a read-only health check must not hang on it.
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `pods run`'s default per-pod budget (`--timeout` overrides). Arbitrary commands can be
/// long (a download, a test suite), so it's generous — but never unbounded: a fleet run
/// must end even when one pod wedges. 30 min.
const RUN_TIMEOUT_SECS: u64 = 30 * 60;

/// `pods backup`: git add + commit + push of a participant's tree — seconds normally, but
/// a first push of notebooks/outputs can take minutes. 5 min.
const BACKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `pods set-branch` / `init-branches`: a fetch + checkout (+ ff-pull or push) against
/// GitHub — seconds normally, so 2 min means stuck (a hung fetch, a held lock).
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// `pods cp`'s scp budget floor: connection setup, a small file, a slow start — 10 min.
/// What's copied adds to it ([`cp_timeout`]). Its `mkdir -p` and post-copy size check are
/// quick probes.
const CP_BASE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The slowest operator uplink `pods cp` budgets for: 1 MB/s (≈8 Mbit/s, a tethered or
/// hotel connection). Every pod's scp starts at once from the operator's machine, so the
/// copies share it — the budget is for detecting a wedged copy, not a slow one.
const CP_MIN_BYTES_PER_SEC: u64 = 1_000_000;

/// `pods cp`'s per-pod scp budget, proportional to what's copied: [`CP_BASE_TIMEOUT`] plus
/// the time to send `bytes` to all `pods` at [`CP_MIN_BYTES_PER_SEC`], capped at a day
/// (the `--timeout` maximum). A fixed budget killed big fleet-wide copies part-way: 500 MB
/// to 30 pods over a 20 MB/s uplink needs ~750s, so every pod would fail at 600s.
fn cp_timeout(bytes: u64, pods: usize) -> Duration {
    let sending = bytes.saturating_mul(pods as u64) / CP_MIN_BYTES_PER_SEC;
    Duration::from_secs(
        CP_BASE_TIMEOUT.as_secs().saturating_add(sending).min(arena_core::setup::MAX_STEP_TIMEOUT_SECS),
    )
}

/// What `pods cp` sends for `path`: a file's size, or a tree's total (as `scp -r` sends
/// it). Best-effort, for [`cp_timeout`] only: unreadable entries count 0 and symlinked
/// dirs inside the tree aren't followed (no loops) — the base budget absorbs the slack.
fn local_size(path: &std::path::Path) -> u64 {
    fn tree(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => tree(&e.path()),
                // A file, or a symlink to one (scp copies the target).
                Ok(_) => std::fs::metadata(e.path()).ok().filter(|m| m.is_file()).map_or(0, |m| m.len()),
                Err(_) => 0,
            })
            .sum()
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => tree(path),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// replace/migrate's direct pod-to-pod rsync of a home dir (caches/models excluded): a
/// few GB, minutes pod-to-pod. Two hours means the transfer is wedged, and the pipeline
/// must give the operator their terminal back instead of waiting forever.
const POD_COPY_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// How one pod's SSH call ended, ready for its report line: the command's output (which
/// may still be a non-zero exit), or why there is none — `timed out after Ns`, a spawn
/// error, a crashed task.
type PodCall = std::result::Result<arena_core::ssh::SshOutput, String>;

/// Run one job per pod concurrently and hand each pod's result to `on_done(done, total,
/// key, result)` as it finishes. Finishing order is the point: healthy pods report at
/// once, and a pod whose job is stuck reports when its budget runs out — never holding the
/// others, or the command, hostage (jobs bound their own SSH calls). A job that panicked is
/// still reported, by its key, as `Err` — never silently dropped from the tally.
///
/// `key` is usually the pod's name (all a report line needs). A caller that matches results
/// back to pods uses something unique instead — two pods can share a name (a double
/// create, a leftover), and a name-keyed map would hand one pod the other's result.
async fn each_pod<K, T, F>(
    jobs: Vec<(K, F)>,
    mut on_done: impl FnMut(usize, usize, &K, std::result::Result<T, String>),
) where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    let mut keys = std::collections::HashMap::new();
    for (key, job) in jobs {
        keys.insert(set.spawn(job).id(), key);
    }
    let mut done = 0;
    while let Some(joined) = set.join_next_with_id().await {
        done += 1;
        let (id, result) = match joined {
            Ok((id, out)) => (id, Ok(out)),
            Err(e) => (e.id(), Err(format!("task crashed: {e}"))),
        };
        // The set only yields tasks spawned above, each recorded with its key.
        let key = keys.remove(&id).expect("every spawned task has a key");
        on_done(done, total, &key, result);
    }
}

/// [`each_pod`] for the common case: one command per pod over `remote`, each bounded by
/// `timeout`, reported as a [`PodCall`].
async fn exec_each_pod<K>(
    remote: &Arc<dyn Remote>,
    jobs: Vec<(K, SshTarget, String)>,
    timeout: Duration,
    mut on_done: impl FnMut(usize, usize, &K, PodCall),
) {
    let jobs = jobs
        .into_iter()
        .map(|(name, target, cmd)| {
            let remote = remote.clone();
            (name, async move { remote.exec(&target, &cmd, Some(timeout)).await.map_err(|e| describe_error(&e)) })
        })
        .collect();
    each_pod(jobs, |done, total, name, result| on_done(done, total, name, result.and_then(|call| call))).await;
}

/// The `[done/total] ✗ <name> …` line for a pod whose call failed: the remote command's
/// non-zero exit (with its stderr), or why there was no answer (`timed out after Ns`, …).
fn failure_line(done: usize, total: usize, name: &str, call: &PodCall) -> String {
    match call {
        Ok(out) => format!("[{done}/{total}] ✗ {name} (exit {:?}): {}", out.code, out.stderr.trim()),
        Err(why) => format!("[{done}/{total}] ✗ {name}: {why}"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_setup(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    cfg: &Config,
    apply: bool,
    force: bool,
    hf_token: Option<String>,
    cc_token: Option<String>,
    zsh_install: bool,
    // Per-step budgets, resolved by the caller (`SetupTimeouts::from_config`) *before* it
    // does anything costly: `up` / `replace` / `migrate copy` create (billing) pods before
    // they get here, so a malformed SETUP_TIMEOUT_SECS must fail before the create, not
    // after it — taking the resolved value (not the raw flag) makes that the only option.
    timeouts: arena_core::setup::SetupTimeouts,
    // The pods to provision, resolved by the caller: the operator's selection, or the pods
    // `up` / `replace` / `migrate copy` just created.
    sel: &Selected,
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    use arena_core::setup::{provisioning_steps, BootRetry, ProvisionStep};
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
    // Carry each pod's provider so we can pick bare-VM vs image-based provisioning. A named
    // pod without an endpoint is reported; if none of the named pods is reachable, it fails.
    let targets: Vec<(String, String, SshTarget)> =
        sel.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), p.provider.clone(), t)).collect();
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
            for step in provisioning_steps(provider_name, &display_scfg, name, force, &hetzner_script, &timeouts) {
                println!("  ## {} (timeout {}s)", step.label(), step.timeout().as_secs());
                match step {
                    ProvisionStep::Scp { local, remote, .. } => println!("  {}", target.display_scp(&local, &remote)),
                    ProvisionStep::Run { cmd, .. } => println!("  {}", target.display_command(&cmd)),
                }
            }
            println!();
        }
        let keys_present = arena_core::apikeys::PROVIDERS.iter().any(|(base, _, _)| {
            std::fs::read_to_string(format!("{keys_dir}/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        });
        println!(
            "API keys: {}",
            if keys_present {
                format!("found in {keys_dir} — would distribute to each pod above that provisions successfully")
            } else {
                format!("none in {keys_dir} — would skip")
            }
        );
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Provision concurrently across the fleet, one line per pod as it finishes; a stuck
    // pod times out at its step and never blocks the rest.
    let total = targets.len();
    println!(
        "Provisioning {total} pod(s) over SSH (budgets: copy {}s, config {}s, hetzner script {}s)…",
        timeouts.copy.as_secs(),
        timeouts.config.as_secs(),
        timeouts.bare_vm.as_secs()
    );
    let jobs = targets
        .into_iter()
        .map(|(name, provider_name, target)| SetupJob {
            steps: provisioning_steps(&provider_name, &scfg, &name, force, &hetzner_script, &timeouts),
            name,
            target,
        })
        .collect();
    let (provisioned, failed) =
        provision_fleet(remote.clone(), jobs, BootRetry::default(), |line| println!("{line}")).await;
    println!("\nDone: {} provisioned, {failed} failed.", provisioned.len());

    // Auto-handle API keys: if per-host CSVs have been generated, distribute them and say
    // so; otherwise report they're not set up (rather than silently doing nothing). HF is
    // already handled inline above. Best-effort — never fails the setup.
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
    } else if provisioned.is_empty() {
        println!("API keys: found {} — skipped, no pod provisioned successfully.", csv_sources.join(", "));
    } else {
        // Scope key distribution to exactly the pods that just provisioned OK: never the
        // rest of the fleet (`setup <one-pod>` must not touch the others), and never a pod
        // that just failed or timed out — re-contacting a wedged pod would hang here. Each
        // pod's write is bounded by COPY_KEYS_TIMEOUT anyway.
        let skipped = if failed > 0 { format!(" (skipping the {failed} that failed)") } else { String::new() };
        println!(
            "API keys: found {} — distributing to the {} provisioned pod(s){skipped}…",
            csv_sources.join(", "),
            provisioned.len()
        );
        let done = Selected {
            pods: sel.pods.iter().filter(|p| provisioned.contains(&p.name)).cloned().collect(),
            named: true,
        };
        if let Err(e) = handle_copy_keys(remote, cfg, keys_dir, None, None, &done, false, true).await {
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
        CronCmd::Install { schedule, start_date, pull, proxy } => {
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
            let lines = cron_lines(&CronJob {
                schedule: &schedule,
                env_prefix: &env_prefix,
                exe: &exe.display().to_string(),
                config: &cfg_abs.display().to_string(),
                home: &home,
                pull,
                proxy,
            });
            let new = with_arena_block(&current, &lines);
            write_crontab(&new).await?;
            println!("Installed arena cron job(s):");
            for l in &lines {
                println!("  {l}");
            }
            println!("\n(remove with `arena cron remove`; view with `arena cron show`)");
            return Ok(());
        }
    }
}

/// What `cron install` schedules (the paths already resolved to absolute ones).
struct CronJob<'a> {
    schedule: &'a str,
    /// Inline env for the backup line (e.g. `ARENA_START_DATE=… `), already validated.
    env_prefix: &'a str,
    exe: &'a str,
    config: &'a str,
    home: &'a str,
    pull: bool,
    proxy: bool,
}

/// How often the optional proxy re-sync runs. Each tick is one list call per provider plus
/// a no-op when nothing changed (the config is only rewritten — and nginx only reloaded —
/// on a real change), so 5 minutes is cheap and bounds how long a moved endpoint stays
/// unrouted.
const PROXY_CRON_SCHEDULE: &str = "*/5 * * * *";

/// cron runs jobs with `PATH=/usr/bin:/bin`, but Debian/Ubuntu install nginx in
/// `/usr/sbin`: without this the default reload (`nginx -t && nginx -s reload`) fails
/// with "nginx: not found" on every tick.
const PROXY_CRON_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The arena-managed crontab lines for `cron install` — pure, so the rendering is tested
/// without touching a real crontab. Always the backup job; with `proxy`, also a
/// `proxy apply --yes` every 5 minutes. That's safe unattended because the merge is
/// sticky: a provider that fails to list never drops a forward, and if none answers
/// nothing is written. It has its own log so its every-5-minutes chatter doesn't bury the
/// backup's output. `flock -n` makes a tick that finds the previous one still running
/// exit at once instead of piling up (list calls are bounded by `LIST_TIMEOUT`, but an
/// SSH to a remote proxy isn't). Deliberately no `timeout` kill: one landing between the
/// write and the reload would leave a file nginx never loaded, which later ticks would
/// read as up to date.
fn cron_lines(job: &CronJob) -> Vec<String> {
    let CronJob { schedule, env_prefix, exe, config, home, pull, proxy } = job;
    // `pods backup` now also rsyncs the home (the file backup); default the cron to
    // git-only (`--no-pull`) since it runs frequently, and let `--pull` opt into the
    // full backup each tick (rsync is incremental, so repeats only move deltas).
    let backup = if *pull {
        format!("{env_prefix}{exe} --config {config} pods backup --yes")
    } else {
        format!("{env_prefix}{exe} --config {config} pods backup --no-pull --yes")
    };
    let mut lines = vec![format!("{schedule} {backup} >> {home}/arena-cron.log 2>&1")];
    if *proxy {
        lines.push(format!(
            "{PROXY_CRON_SCHEDULE} {PROXY_CRON_PATH} flock -n {home}/.arena-proxy-cron.lock \
             {exe} --config {config} proxy apply --yes >> {home}/arena-proxy-cron.log 2>&1"
        ));
    }
    lines
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
        ConfigCmd::Which => config_which(cfg, provider_name, config_path),
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

/// `config which`: show the active config file (path, readable/writable), a one-line
/// summary of what parsed, and any keys currently being supplied by the environment
/// (which silently override the file) so it's clear where values are coming from.
fn config_which(cfg: &Config, provider_name: &str, config_path: &std::path::Path) -> Result<()> {
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
    if std::env::var("ARENA_CONFIG").is_ok() {
        println!("  source: ARENA_CONFIG environment variable");
    }
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

/// The `config check` row for `RUNPOD_API`, and whether it's valid. Pure, for testing.
/// v1 gets a warning with the retirement date: past it, every RunPod call fails.
fn runpod_api_row(cfg: &Config) -> (String, bool) {
    use arena_core::provider::{RunpodApi, RUNPOD_V1_RETIREMENT};
    let key = "RUNPOD_API";
    match RunpodApi::from_config(cfg) {
        Ok(RunpodApi::V2) => (format!("  ✓ {key:<24} v2 (api.runpod.io/v2)"), true),
        Ok(RunpodApi::V1) => (
            format!(
                "  ⚠ {key:<24} v1{} (rest.runpod.io/v1) — RunPod retires REST v1 on {RUNPOD_V1_RETIREMENT}; \
                 set RUNPOD_API=v2 once it's been live-tested",
                if cfg.get(key).is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" }
            ),
            true,
        ),
        Err(e) => (format!("  ✗ {key:<24} {e}"), false),
    }
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
    // Which RunPod REST generation `runpod` talks to — shown whenever RunPod is in play,
    // because v1 is retired by RunPod and this is the switch.
    if provider_name == "runpod" || cfg.get("RUNPOD_API_KEY").is_some_and(|v| !v.is_empty()) || cfg.get("RUNPOD_API").is_some() {
        let (line, ok) = runpod_api_row(cfg);
        println!("{line}");
        if !ok {
            missing.push("RUNPOD_API (must be v1 or v2)".into());
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

    println!("\nProxy (`proxy plan` / `proxy apply`):");
    cfg_row(cfg, &mut missing, "SSH_PROXY_HOST", false, false);
    cfg_row(cfg, &mut missing, "SSH_PROXY_STARTING_PORT", false, false);
    // Absent and empty mean different things here (default reload vs never reload), so
    // this row can't use `cfg_row`'s set/missing view.
    let reload = arena_core::proxy::reload_cmd_from(cfg);
    let key = "SSH_PROXY_RELOAD_CMD";
    match cfg.get(key) {
        None => println!("  · {key:<28} (default) {reload}"),
        Some(_) if reload.is_empty() => {
            println!("  ✓ {key:<28} (empty) write-only — the config file is written, nginx is never reloaded")
        }
        Some(_) => println!("  ✓ {key:<28} {reload}"),
    }

    println!("\nBackup (git `backup` + file `pull`):");
    cfg_row(cfg, &mut missing, "ARENA_REPO_NAME", false, false);
    cfg_row(cfg, &mut missing, "GIT_SSH_KEY_REMOTE", false, false);
    cfg_row(cfg, &mut missing, "ARENA_START_DATE", false, false); // wNdM label (init-branches / pull)
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
    println!(
        "  {} iteration start date       {}",
        if cfg.get("ARENA_START_DATE").is_some() { "✓" } else { "✗" },
        cfg.get("ARENA_START_DATE").unwrap_or("unset — backups can't label wNdM")
    );
    // The per-step setup budgets, as `pods setup`/`up` will use them (SETUP_TIMEOUT_SECS).
    match arena_core::setup::SetupTimeouts::from_config(cfg, None) {
        Ok(t) => println!(
            "  · setup step budgets        copy {}s, config {}s, hetzner script {}s",
            t.copy.as_secs(),
            t.config.as_secs(),
            t.bare_vm.as_secs()
        ),
        Err(e) => println!("  ✗ setup step budgets        {e}"),
    }
    // The driver floor `pods test --deep` holds pods to (MIN_DRIVER_VERSION, else derived
    // from ALLOWED_CUDA_VERSIONS).
    match arena_core::health::HealthPolicy::from_config(cfg) {
        Ok(p) => match p.min_driver {
            Some(f) => {
                let v = f.version.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
                println!("  · deep-check driver floor   ≥ {v} ({})", f.why)
            }
            None => println!("  · deep-check driver floor   none (no MIN_DRIVER_VERSION / ALLOWED_CUDA_VERSIONS)"),
        },
        Err(e) => println!("  ✗ deep-check driver floor   {e}"),
    }

    if missing.is_empty() {
        println!("\nOK — required keys for provider `{provider_name}` are present.");
        Ok(())
    } else {
        anyhow::bail!("missing required keys: {}", missing.join(", "))
    }
}

async fn handle_backup(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    apply: bool,
    message: Option<String>,
    // The pods to back up, resolved by the caller (`pods backup` shares it with the pull).
    sel: &Selected,
) -> Result<()> {
    use arena_core::backup;

    // Backup commits the *current* branch (never switches/creates one), so it just needs
    // the repo path + push key — no week/day / autocommit-branch naming.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE").map(String::from);
    let msg_for = |name: &str| message.clone().unwrap_or_else(|| format!("arena backup {name}"));

    // Back up only pods that actually have an SSH endpoint; report the rest.
    let targets = sel.named_targets(cfg)?;
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
    println!(
        "Backing up {total} pod(s) over SSH (current branch; main/master skipped; {}s budget each)…",
        BACKUP_TIMEOUT.as_secs()
    );
    let jobs = targets
        .into_iter()
        .map(|(name, target)| {
            let cmd = backup::backup_command(&repo_path, key.as_deref(), &msg_for(&name));
            (name, target, cmd)
        })
        .collect();
    let t = backup_fleet(&remote, jobs, |line| println!("{line}")).await;
    println!(
        "\nDone: {} pushed, {} unchanged, {} skipped (main/master), {} failed.",
        t.pushed, t.unchanged, t.skipped, t.failed
    );
    if t.failed > 0 {
        anyhow::bail!("{} pod(s) failed to back up", t.failed);
    }
    Ok(())
}

/// What a fleet `backup` did, per outcome.
#[derive(Debug, Default, PartialEq, Eq)]
struct BackupTally {
    pushed: usize,
    unchanged: usize,
    skipped: usize,
    failed: usize,
}

/// Run each pod's backup command (`(name, target, cmd)`) concurrently over `remote`, each
/// within [`BACKUP_TIMEOUT`], classifying its output by the sentinel it printed and
/// emitting one `[done/total]` line per pod as it finishes. A non-zero exit, a timeout or a
/// crashed task is a failure; the others still finish.
async fn backup_fleet(
    remote: &Arc<dyn Remote>,
    jobs: Vec<(String, SshTarget, String)>,
    mut emit: impl FnMut(&str),
) -> BackupTally {
    use arena_core::backup::{self, parse_backup_output};
    let mut t = BackupTally::default();
    exec_each_pod(remote, jobs, BACKUP_TIMEOUT, |done, total, name, call| {
        let line = match &call {
            Ok(out) if out.success => match parse_backup_output(&out.stdout) {
                Some((backup::BACKUP_PUSHED, branch)) => {
                    t.pushed += 1;
                    format!("[{done}/{total}] ✓ {name} -> {branch}")
                }
                Some((backup::BACKUP_NO_CHANGES, branch)) => {
                    t.unchanged += 1;
                    format!("[{done}/{total}] = {name} (no changes, on {branch})")
                }
                Some((backup::BACKUP_SKIPPED, branch)) => {
                    t.skipped += 1;
                    format!("[{done}/{total}] ⊘ {name} (skipped — on protected branch {branch})")
                }
                _ => {
                    t.pushed += 1;
                    format!("[{done}/{total}] ✓ {name} (done)")
                }
            },
            _ => {
                t.failed += 1;
                failure_line(done, total, name, &call)
            }
        };
        emit(&line);
    })
    .await;
    t
}

async fn handle_proxy(cmd: ProxyCmd, provider: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    match cmd {
        ProxyCmd::Plan { out } => {
            let listing = fleet_listing(provider).await;
            emit_proxy_plan(&listing, cfg, out.as_deref())?;
        }
        ProxyCmd::Apply { dry_run } => {
            let listing = fleet_listing(provider).await;
            let prepared = prepare_proxy(cfg, &listing).await?;
            if let Some(why) = &prepared.plan.abort {
                anyhow::bail!("refusing to write the proxy config: {why}");
            }
            print_proxy_review(&prepared, &listing);
            if dry_run {
                print_proxy_dry_run(cfg, &prepared);
                return Ok(());
            }
            if prepared.up_to_date() {
                println!("proxy config already up to date — nothing to write.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {}.", proxy_action(&prepared.pxcfg)))? {
                println!("aborted.");
                return Ok(());
            }
            let written = write_proxy(cfg, &prepared).await?;
            if let Some(line) = written_line(&prepared.pxcfg, prepared.plan.forwards.len(), written) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// The whole fleet's per-provider listing for the proxy merge. Deliberately NOT
/// `list_pods`: that swallows a failing provider, which the merge would read as "every pod
/// on it is gone" and drop their forwards.
async fn fleet_listing(provider: &dyn Provider) -> arena_core::proxy::Listing {
    arena_core::proxy::Listing::from_results(provider.list_by_provider().await)
}

/// The merge planner with this config's prefix + machine list.
fn plan_proxy(
    cfg: &Config,
    pxcfg: &arena_core::proxy::ProxyConfig,
    prev: &[arena_core::proxy::Forward],
    listing: &arena_core::proxy::Listing,
) -> arena_core::proxy::ProxyPlan {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    arena_core::proxy::plan_forwards(pxcfg, prefix, &cfg.machine_names, prev, listing)
}

/// "deploy … and reload nginx" / "write … (no reload)" — for prompts and summaries.
fn proxy_action(pxcfg: &arena_core::proxy::ProxyConfig) -> String {
    let dest = if pxcfg.local {
        format!("this host:{}", expand_tilde(&pxcfg.nginx_path))
    } else {
        format!("{}@{}:{}", pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path)
    };
    if pxcfg.write_only() {
        format!("write the proxy config to {dest} (write-only: SSH_PROXY_RELOAD_CMD is empty, nginx is not reloaded)")
    } else {
        format!("write the proxy config to {dest} and run `{}`", pxcfg.reload_cmd)
    }
}

/// A merge planned against the live config, ready to review and write.
#[derive(Debug)]
struct PreparedProxy {
    pxcfg: arena_core::proxy::ProxyConfig,
    /// The config as it is now; `None` = no file yet.
    current: Option<String>,
    plan: arena_core::proxy::ProxyPlan,
    rendered: String,
}

impl PreparedProxy {
    /// The rendered config is byte-identical to the live one — writing/reloading is a no-op.
    fn up_to_date(&self) -> bool {
        self.current.as_deref() == Some(self.rendered.as_str())
    }
}

/// Read the live config, parse the previous forwards from it, and plan the merge against
/// `listing`. Read-only (a remote proxy is read with `cat` over SSH).
async fn prepare_proxy(cfg: &Config, listing: &arena_core::proxy::Listing) -> Result<PreparedProxy> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let current = read_current_proxy(cfg, &pxcfg).await?;
    let parsed = current.as_deref().map(arena_core::proxy::parse_nginx_detailed).unwrap_or_default();
    if parsed.ignored_blocks > 0 {
        eprintln!(
            "warning: {} server block(s) in the current proxy config aren't in a format arena can read \
             back — they'll be dropped when it's rewritten",
            parsed.ignored_blocks
        );
    }
    let plan = plan_proxy(cfg, &pxcfg, &parsed.forwards, listing);
    let rendered = arena_core::proxy::render_nginx(&plan.forwards);
    Ok(PreparedProxy { pxcfg, current, plan, rendered })
}

/// The live proxy config (`None` = no file yet), which the merge starts from. A read that
/// fails is an error, never "empty": a config rebuilt from an empty `prev` because we
/// couldn't read the real one would drop every forward the merge is meant to keep.
async fn read_current_proxy(cfg: &Config, pxcfg: &arena_core::proxy::ProxyConfig) -> Result<Option<String>> {
    use arena_core::ssh::{self, SshTarget};
    if pxcfg.local {
        let path = expand_tilde(&pxcfg.nginx_path);
        return match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "reading the current proxy config {path}: {e} — refusing to write one built without it"
            )),
        };
    }
    let target = SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let p = &pxcfg.nginx_path;
    let q = sh_path(p);
    // Exit 3 means "no file yet" — distinct from cat's 1 and ssh's own 255 — so a missing
    // file is an empty `prev` while any failure to read it aborts the deploy.
    let out = ssh::run(&target, &format!("test -e {q} || exit 3; cat {q}"))
        .await
        .with_context(|| format!("reading the current proxy config on {}", pxcfg.proxy_host))?;
    match out.code {
        Some(0) => Ok(Some(out.stdout)),
        Some(3) => Ok(None),
        code => anyhow::bail!(
            "couldn't read the current proxy config {p} on {} (exit {code:?}): {} — refusing to write \
             one built without it",
            pxcfg.proxy_host,
            out.stderr.trim()
        ),
    }
}

/// Print a prepared merge for review: provider failures, one `+ ~ - =` line per change
/// (unchanged lines omitted), and the summary.
fn print_proxy_review(p: &PreparedProxy, listing: &arena_core::proxy::Listing) {
    for (prov, e) in listing.errors() {
        eprintln!("warning: {prov} listing failed: {e}");
    }
    for line in p.plan.change_lines(&p.pxcfg.proxy_host, false) {
        println!("  {line}");
    }
    let skipped = if p.plan.skipped.is_empty() {
        String::new()
    } else {
        format!("; {} pod(s) skipped (`arena proxy plan` lists why)", p.plan.skipped.len())
    };
    println!("proxy: {}{skipped}", p.plan.summary());
}

/// `proxy apply --dry-run`: exactly what a real apply would write and run.
fn print_proxy_dry_run(cfg: &Config, p: &PreparedProxy) {
    let px = &p.pxcfg;
    let dest = if px.local { "this host".into() } else { format!("{}@{}", px.proxy_user, px.proxy_host) };
    if p.up_to_date() {
        println!("[dry-run] {dest}:{} already matches — nothing would be written", px.nginx_path);
        return;
    }
    println!("[dry-run] would write {} forward(s) to {dest}:{}", p.plan.forwards.len(), px.nginx_path);
    if px.local {
        if px.write_only() {
            println!(
                "  write {} only — SSH_PROXY_RELOAD_CMD is empty, so nginx is never reloaded",
                expand_tilde(&px.nginx_path)
            );
        } else {
            println!(
                "  write {} + run `{}` locally (the previous config is put back if that fails)",
                expand_tilde(&px.nginx_path),
                px.reload_cmd
            );
        }
    } else {
        let target = arena_core::ssh::SshTarget::for_host(&px.proxy_user, &px.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
        println!("  {}", target.display_scp("<rendered nginx>", "/tmp/.arena-proxy-<unique>.conf"));
        if px.write_only() {
            println!("  then over SSH: install it as {} (no reload — SSH_PROXY_RELOAD_CMD is empty)", px.nginx_path);
        } else {
            println!(
                "  then over SSH: install it as {}, run `{}` (the previous config is put back if that fails)",
                px.nginx_path, px.reload_cmd
            );
        }
    }
    println!(
        "(preview only — run without --dry-run to {})",
        if px.write_only() { "write it" } else { "deploy and reload nginx" }
    );
}

/// Is there a proxy a write can land on? `Err` carries the one-line reason a post-lifecycle
/// sync skips. `pods up` makes the same check to decide whether to deploy as endpoints
/// appear, so the poll loop and the final sync never disagree.
async fn proxy_deployable(cfg: &Config) -> std::result::Result<(), String> {
    use arena_core::ssh::{self, SshTarget};

    let Ok(px) = arena_core::proxy::ProxyConfig::from_config(cfg) else {
        return Err(undeployable_reason(None, false).unwrap_or_default());
    };
    // Write-only mode never runs nginx, so it needs no probe at all.
    let nginx_found = if px.write_only() {
        true
    } else if px.local {
        std::process::Command::new("sh")
            .args(["-c", "command -v nginx >/dev/null 2>&1"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        let target = SshTarget::for_host(&px.proxy_user, &px.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
        matches!(
            ssh::run(&target, "command -v nginx >/dev/null 2>&1 && echo yes").await,
            Ok(out) if out.success && out.stdout.contains("yes")
        )
    };
    match undeployable_reason(Some(&px), nginx_found) {
        None => Ok(()),
        Some(why) => Err(why),
    }
}

/// Why nothing can be deployed (pure, so each case is tested): no proxy configured, or
/// nginx missing where the reload would run. A remote host that doesn't answer reads the
/// same as one without nginx — either way a deploy couldn't land. Write-only never needs
/// nginx.
fn undeployable_reason(px: Option<&arena_core::proxy::ProxyConfig>, nginx_found: bool) -> Option<String> {
    match px {
        None => Some("no proxy configured (SSH_PROXY_HOST unset)".into()),
        Some(px) if px.write_only() || nginx_found => None,
        Some(px) if px.local => Some(
            "nginx not found on this host — install it, then `arena proxy apply` \
             (`arena proxy plan` prints the config)"
                .into(),
        ),
        Some(px) => Some(format!(
            "proxy host {} has no nginx or is unreachable — `arena proxy apply` once it's set up \
             (if this box IS the proxy, unset PROXY_LOCAL)",
            px.proxy_host
        )),
    }
}

/// How a merged config reached the proxy (what [`write_proxy`] did).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// The rendered config already matched the live one byte for byte: nothing written,
    /// nothing reloaded.
    Unchanged,
    /// Written, nginx deliberately not reloaded (`SSH_PROXY_RELOAD_CMD=""`).
    WriteOnly,
    /// Written and the reload command succeeded.
    Reloaded,
}

/// `[proxy] deployed 3 forward(s) locally and reloaded nginx` and friends, for the
/// verbose paths (`proxy apply`, the `up` poll loop); `None` when nothing was written.
fn written_line(px: &arena_core::proxy::ProxyConfig, n: usize, w: Written) -> Option<String> {
    let dest = if px.local { expand_tilde(&px.nginx_path) } else { format!("{}:{}", px.proxy_host, px.nginx_path) };
    match w {
        Written::Unchanged => None,
        Written::WriteOnly => Some(format!("[proxy] wrote {n} forward(s) to {dest} (write-only: nginx not reloaded)")),
        Written::Reloaded if px.local => Some(format!("[proxy] deployed {n} forward(s) locally and reloaded nginx")),
        Written::Reloaded => Some(format!("[proxy] deployed {n} forward(s) to {} and reloaded nginx", px.proxy_host)),
    }
}

/// Merge `listing` into the live proxy config and, if anything changed, write it and run
/// the reload (unless write-only). Idempotent: an unchanged config is neither written nor
/// reloaded; a listing where no provider answered writes nothing at all (error). `review`
/// prints the per-change lines and the write (the `up` poll loop); [`sync_proxy`] passes
/// `false` and reports in one line from the returned plan instead.
///
/// If another arena process rewrote the config between our read and our write
/// ([`ProxyChanged`]), the merge is redone against the file it wrote — a merge built from
/// the stale read would silently undo that writer's forwards.
async fn deploy_proxy(
    cfg: &Config,
    listing: &arena_core::proxy::Listing,
    review: bool,
) -> Result<(PreparedProxy, Written)> {
    const ATTEMPTS: usize = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let prepared = prepare_proxy(cfg, listing).await?;
        if let Some(why) = &prepared.plan.abort {
            anyhow::bail!("not touching the proxy config: {why}");
        }
        if prepared.up_to_date() {
            return Ok((prepared, Written::Unchanged));
        }
        if review {
            print_proxy_review(&prepared, listing);
        }
        let written = match write_proxy(cfg, &prepared).await {
            Ok(w) => w,
            Err(e) if e.is::<ProxyChanged>() && attempt < ATTEMPTS => continue,
            Err(e) => return Err(e),
        };
        if review {
            if let Some(line) = written_line(&prepared.pxcfg, prepared.plan.forwards.len(), written) {
                println!("{line}");
            }
        }
        return Ok((prepared, written));
    }
}

/// What a post-lifecycle proxy sync did. Returned so a caller that *depends* on the proxy
/// (migrate cutover verifies through it, and reverts if it didn't land) can act on it;
/// everyone else ignores it — [`sync_proxy`] has already printed its line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxySync {
    /// Not attempted: no proxy configured, or nowhere to deploy to (why).
    Skipped(String),
    /// Merged against a fresh fleet-wide listing.
    Synced {
        written: Written,
        counts: arena_core::proxy::ChangeCounts,
        /// Machines whose forward this sync dropped — the lockout-relevant change, so
        /// it's named rather than just counted.
        removed: Vec<String>,
        /// On-list pods listed with no SSH endpoint yet (not forwarded until a later sync).
        pending: usize,
        /// Providers whose listing failed: their forwards were kept as they were.
        failed_providers: Vec<String>,
        /// Forwards this sync routed to an endpoint the listing confirmed (not kept-stale
        /// ones) — what `pods replace` checks before terminating the pod it replaced.
        routed: Vec<arena_core::proxy::Forward>,
    },
    /// Listing, reading the live config, writing or reloading failed.
    Failed(String),
}

/// The single line a sync prints, e.g. `[proxy] after terminate: +0 ~0 -1 =0 (deployed;
/// removed arena8-apple)`. Pure, so every variant is tested. Error text is folded onto the
/// line: it often embeds a command's multi-line stderr (`nginx -t`'s `[emerg]` + "test
/// failed", ssh's host-key notice).
fn sync_line(why: &str, s: &ProxySync) -> String {
    let one_line = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    match s {
        ProxySync::Skipped(reason) => format!("[proxy] after {why}: skipped — {}", one_line(reason)),
        ProxySync::Failed(e) => {
            format!("[proxy] after {why}: NOT synced — {} (retry: `arena proxy apply`)", one_line(e))
        }
        ProxySync::Synced { written, counts, removed, pending, failed_providers, .. } => {
            let mut notes = vec![match written {
                Written::Unchanged => "unchanged".to_string(),
                Written::WriteOnly => "written; write-only, nginx not reloaded".to_string(),
                Written::Reloaded => "deployed".to_string(),
            }];
            if !removed.is_empty() {
                const SHOWN: usize = 3;
                let mut names = removed.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
                if removed.len() > SHOWN {
                    names.push_str(&format!(" +{} more", removed.len() - SHOWN));
                }
                notes.push(format!("removed {names}"));
            }
            if *pending > 0 {
                notes.push(format!("{pending} pod(s) not forwarded yet (no SSH endpoint)"));
            }
            if !failed_providers.is_empty() {
                notes.push(format!("{} failed to list — forwards kept", failed_providers.join(", ")));
            }
            format!("[proxy] after {why}: {} ({})", counts.compact(), notes.join("; "))
        }
    }
}

/// Best-effort proxy sync at the end of a lifecycle command (create/up/rename/reimage/
/// terminate/replace/migrate): skip with a note if there's no deployable proxy, else list
/// the fleet per provider, merge (the sticky 0.A rules), write + reload only if the config
/// changed, and print exactly one line. It never fails the command — the pods are already
/// created/renamed/terminated, and an error exit would invite re-running the mutation
/// itself; a failure is a warning naming `arena proxy apply`.
///
/// `fleet` must span every configured provider (the `build_fleet` provider the pods
/// commands get); a flow holding one backend uses [`sync_proxy_via`].
///
/// A just-terminated pod can still be listed for a while (RunPod/Vast report it exited or
/// terminating), so the sync right after `terminate` may keep its forward (R1/R2). That's
/// harmless — its port belongs to that machine name alone — and the next sync (any
/// lifecycle command, `proxy apply`, or the `cron install --proxy` tick) removes it once
/// the provider stops listing the pod.
async fn sync_proxy(cfg: &Config, fleet: &dyn Provider, why: &str) -> ProxySync {
    let outcome = match proxy_deployable(cfg).await {
        Err(reason) => ProxySync::Skipped(reason),
        Ok(()) => {
            let listing = fleet_listing(fleet).await;
            match deploy_proxy(cfg, &listing, false).await {
                Ok((prepared, written)) => ProxySync::Synced {
                    written,
                    counts: prepared.plan.counts(),
                    removed: prepared
                        .plan
                        .changes
                        .iter()
                        .filter(|c| matches!(c.kind, arena_core::proxy::ChangeKind::Removed { .. }))
                        .map(|c| c.forward.name.clone())
                        .collect(),
                    pending: prepared.plan.pending.len(),
                    failed_providers: listing.errors().iter().map(|(p, _)| p.to_string()).collect(),
                    routed: prepared.plan.routed(),
                },
                Err(e) => ProxySync::Failed(format!("{e:#}")),
            }
        }
    };
    report_sync(why, outcome)
}

/// Print a sync's line (failures to stderr) and hand the outcome back.
fn report_sync(why: &str, outcome: ProxySync) -> ProxySync {
    let line = sync_line(why, &outcome);
    if matches!(outcome, ProxySync::Failed(_)) {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    outcome
}

/// [`sync_proxy`] for flows that hold a single backend (replace/migrate get the pod's
/// `owner`): rebuild the fleet from its name first, so every configured provider is
/// listed. A single-provider listing would leave the others "not queried" and — for legacy
/// entries with no recorded owner — could even read as "every provider listed OK without
/// them", dropping their forwards.
async fn sync_proxy_via(cfg: &Config, owner: &dyn Provider, why: &str) -> ProxySync {
    match arena_core::provider::build_fleet(owner.name(), cfg, true) {
        Ok(fleet) => sync_proxy(cfg, fleet.as_ref(), why).await,
        Err(e) => report_sync(why, ProxySync::Failed(format!("building the fleet provider: {e}"))),
    }
}

/// Another arena process (the proxy cron, a lifecycle sync, an interactive apply) changed
/// the proxy config between our read and our write. Typed so [`deploy_proxy`] can redo
/// the merge against the new file instead of overwriting it with one built from a stale
/// `prev` — which would silently drop whatever that writer had kept or added.
#[derive(Debug)]
struct ProxyChanged;

impl std::fmt::Display for ProxyChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the proxy config changed since it was read (another arena run wrote it) — not \
             overwriting it; re-run to merge against the new one",
        )
    }
}

impl std::error::Error for ProxyChanged {}

/// How long a proxy write waits for another arena process's write to finish. A writer
/// holds the lock only for re-read + write + reload (seconds), so this is generous.
const PROXY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Serialize proxy writes between arena processes on this box — the `cron install
/// --proxy` tick, post-lifecycle syncs, an interactive `proxy apply` — with an exclusive
/// `flock` held across re-read, write and reload. Local proxy: on the config's
/// *directory*, which needs no write access and is the same inode for root's cron and a
/// user's shell. Remote proxy: on a lock file in the temp dir keyed by host (only
/// serializes writers on *this* control box). Best-effort: when no lock can be opened we
/// go on — [`write_proxy`]'s re-read-and-compare still refuses to overwrite a change.
async fn proxy_lock(pxcfg: &arena_core::proxy::ProxyConfig) -> Result<Option<std::fs::File>> {
    use std::path::Path;
    let file = if pxcfg.local {
        let path = PathBuf::from(expand_tilde(&pxcfg.nginx_path));
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::File::open(dir).ok()
    } else {
        let host: String =
            pxcfg.proxy_host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        let path = std::env::temp_dir().join(format!("arena-proxy-{host}.lock"));
        // Open an existing lock read-only (flock doesn't need write access, and another
        // user may own it); create it world-readable otherwise so the next user can too.
        std::fs::File::open(&path).ok().or_else(|| {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&path).ok()?;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
            Some(f)
        })
    };
    let Some(file) = file else { return Ok(None) };
    let deadline = std::time::Instant::now() + PROXY_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another arena process has been writing the proxy config for over {}s — not writing",
                PROXY_LOCK_WAIT.as_secs()
            ),
            // A filesystem without flock: go on, the compare-before-write still guards.
            Err(std::fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// Replace `path`'s contents so a concurrent reader (another arena's prepare, someone's
/// `nginx -t`) sees the old file or the new one, never a truncated one: write a hidden temp
/// sibling (the leading `.` keeps it out of an nginx `include …/*` glob), give it the old
/// file's mode and owner, and rename it over. A symlink is followed — its target is
/// replaced, the link kept. Where that isn't permitted (a directory we can't create files
/// in, a sticky one we can't rename within — the old in-place write only needed the file
/// itself to be writable) fall back to that in-place write rather than fail a deploy that
/// used to work; the lock + compare in [`write_proxy`] still keep arena writers apart.
fn replace_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(dir), Some(name)) = (real.parent(), real.file_name()) else {
        return std::fs::write(&real, text);
    };
    let tmp = dir.join(format!(".{}.arena-tmp-{}", name.to_string_lossy(), std::process::id()));
    let swapped = std::fs::write(&tmp, text).and_then(|()| {
        if let Ok(meta) = std::fs::metadata(&real) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
            // Root's cron rewriting a user-owned file must leave it user-owned, or the
            // user's next interactive sync can't write it. (Fails harmlessly as non-root.)
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
        std::fs::rename(&tmp, &real)
    });
    match swapped {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                std::fs::write(&real, text)
            } else {
                Err(e)
            }
        }
    }
}

/// A config path as a shell word: `~/x` → `"$HOME"/'x'` (still expands on the far side),
/// anything else single-quoted.
fn sh_path(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(p),
    }
}

/// `<pid>-<nanos>`: unique enough for a temp file name no other process will pick.
fn unique_tag() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// Exit codes of [`remote_install_script`].
const INSTALL_FAILED: i32 = 90;
const RELOAD_FAILED_RESTORED: i32 = 91;
const RELOAD_FAILED_NOT_RESTORED: i32 = 92;

/// The single remote command that installs an uploaded config: back up the live file,
/// copy the upload over it *in place* (keeps its owner and mode, and needs only write
/// access to the file, like the old direct scp), run the reload, and if the reload fails
/// put the backup back (or remove the file if there was none) — so the live file always
/// matches what nginx is running, and the next sync sees a difference and retries.
/// One SSH session after the upload, so a connection dropped between upload and install
/// leaves the live config untouched. `reload_cmd: None` = write-only. Pure (and plain
/// POSIX sh), so its behaviour is tested by running it locally.
fn remote_install_script(path: &str, upload: &str, reload_cmd: Option<&str>) -> String {
    let (p, n) = (sh_path(path), shell_quote(upload));
    let mut script = format!(
        "p={p}; n={n}; b=\"$n.prev\"; had=0\n\
         restore() {{ if [ \"$had\" = 1 ]; then cat \"$b\" > \"$p\"; else rm -f \"$p\"; fi; }}\n\
         if [ -e \"$p\" ]; then cat \"$p\" > \"$b\" || {{ rm -f \"$n\" \"$b\"; exit {INSTALL_FAILED}; }}; had=1; fi\n\
         if ! cat \"$n\" > \"$p\"; then restore; rm -f \"$n\" \"$b\"; exit {INSTALL_FAILED}; fi\n\
         rm -f \"$n\"\n"
    );
    match reload_cmd {
        None => script.push_str("rm -f \"$b\"\nexit 0\n"),
        Some(cmd) => script.push_str(&format!(
            // On its own lines, so a reload command ending in a `# comment` can't swallow
            // the rest of the script.
            "if (\n{cmd}\n); then rm -f \"$b\"; exit 0; fi\n\
             if restore; then rm -f \"$b\"; exit {RELOAD_FAILED_RESTORED}; fi\n\
             exit {RELOAD_FAILED_NOT_RESTORED}\n"
        )),
    }
    script
}

/// Removes a local temp file when dropped (the scp source), whatever path we leave by.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write a prepared config to the proxy (locally, or scp + one SSH install over SSH) and
/// run the reload command unless write-only. This is the one place the tool touches the
/// proxy host. Silent: callers report (see [`written_line`], [`sync_line`]).
///
/// - Under [`proxy_lock`], the live config is re-read and must still be what `p` was
///   planned against, else [`ProxyChanged`] — never a blind overwrite.
/// - A failed reload puts the previous config back. Leaving the new file in place would
///   make every later sync see "already up to date" and never retry the reload, while
///   nginx kept serving the old routes.
async fn write_proxy(cfg: &Config, p: &PreparedProxy) -> Result<Written> {
    use arena_core::ssh::{self, SshTarget};

    // The planner's abort is also enforced here, so no caller can write around it.
    if let Some(why) = &p.plan.abort {
        anyhow::bail!("refusing to write the proxy config: {why}");
    }
    if p.up_to_date() {
        return Ok(Written::Unchanged);
    }
    let pxcfg = &p.pxcfg;
    let _lock = proxy_lock(pxcfg).await?;
    if read_current_proxy(cfg, pxcfg).await? != p.current {
        return Err(ProxyChanged.into());
    }

    // Local: this box IS the proxy — write the config + reload directly, no SSH.
    if pxcfg.local {
        let path = PathBuf::from(expand_tilde(&pxcfg.nginx_path));
        replace_file(&path, &p.rendered).with_context(|| format!("writing nginx config to {}", path.display()))?;
        if pxcfg.write_only() {
            return Ok(Written::WriteOnly);
        }
        let failure = match std::process::Command::new("sh").args(["-c", &pxcfg.reload_cmd]).output() {
            Ok(out) if out.status.success() => return Ok(Written::Reloaded),
            Ok(out) => format!("local nginx reload failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => format!("running the local reload command: {e}"),
        };
        let restored = match &p.current {
            Some(prev) => replace_file(&path, prev),
            None => std::fs::remove_file(&path).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }),
        };
        match restored {
            Ok(()) => anyhow::bail!("{failure} — previous config put back, so the next sync retries"),
            Err(e) => anyhow::bail!(
                "{failure} — and putting the previous config back failed ({e}): {} now holds a config \
                 nginx hasn't loaded, which a re-run will see as up to date; fix the cause, then run \
                 `{}` by hand",
                path.display(),
                pxcfg.reload_cmd
            ),
        }
    }

    // Remote: upload to a unique temp file on the proxy host, then install + reload there
    // in one SSH session. (A fixed local temp name broke once another user had created it.)
    let target =
        SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let tag = unique_tag();
    let local_tmp = RemoveOnDrop(std::env::temp_dir().join(format!("arena-proxy-{tag}.conf")));
    std::fs::write(&local_tmp.0, &p.rendered).context("writing rendered nginx config to a temp file")?;
    let upload = format!("/tmp/.arena-proxy-{tag}.conf");
    let scp = ssh::scp(&target, &local_tmp.0.to_string_lossy(), &upload).await?;
    if !scp.success {
        anyhow::bail!("scp to proxy {} failed: {}", pxcfg.proxy_host, scp.stderr.trim());
    }
    let reload = (!pxcfg.write_only()).then_some(pxcfg.reload_cmd.as_str());
    let out = ssh::run(&target, &remote_install_script(&pxcfg.nginx_path, &upload, reload)).await?;
    let (host, err) = (&pxcfg.proxy_host, out.stderr.trim());
    match out.code {
        Some(0) if reload.is_none() => Ok(Written::WriteOnly),
        Some(0) => Ok(Written::Reloaded),
        Some(INSTALL_FAILED) => anyhow::bail!("installing the config on {host} failed (live config left as it was): {err}"),
        Some(RELOAD_FAILED_RESTORED) => {
            anyhow::bail!("nginx reload on {host} failed: {err} — previous config put back, so the next sync retries")
        }
        Some(RELOAD_FAILED_NOT_RESTORED) => anyhow::bail!(
            "nginx reload on {host} failed: {err} — and putting the previous config back failed: {} \
             holds a config nginx hasn't loaded, which a re-run will see as up to date; fix the cause, \
             then run `{}` there by hand",
            pxcfg.nginx_path,
            pxcfg.reload_cmd
        ),
        code => anyhow::bail!(
            "installing/reloading on {host} failed (exit {code:?}): {err} — the live config may not \
             have changed; `arena proxy apply --dry-run` shows where it stands"
        ),
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

/// `proxy plan`: show the merge against the current config (`+ ~ - =` per machine, the
/// summary, skipped pods), then the rendered nginx config (optionally also written to
/// `out`, locally). Never connects to the proxy: for a local proxy the current config is a
/// local file read; for a remote one the diff isn't shown (`proxy apply --dry-run` reads it).
///
/// Without the current config (remote proxy, or the local file unreadable) the plan starts
/// from nothing, so it lacks every forward the merge would *keep* (owner's provider
/// failed, pod listed without an endpoint). That output is labelled a listing-only preview
/// and `--out` is refused: deployed by hand, it would drop those participants' forwards.
fn emit_proxy_plan(listing: &arena_core::proxy::Listing, cfg: &Config, out: Option<&std::path::Path>) -> Result<()> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;

    println!(
        "\n# proxy host: {}@{}  (nginx config path: {})",
        pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path
    );
    // `None` = the current config is unknown, so this can't be a merge.
    let prev: Option<Vec<arena_core::proxy::Forward>> = if pxcfg.local {
        let path = expand_tilde(&pxcfg.nginx_path);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let parsed = arena_core::proxy::parse_nginx_detailed(&text);
                println!("# merge vs current config {path} ({} forward(s))", parsed.forwards.len());
                if parsed.ignored_blocks > 0 {
                    eprintln!(
                        "warning: {} server block(s) in {path} aren't in a format arena can read back — \
                         they'd be dropped on apply",
                        parsed.ignored_blocks
                    );
                }
                Some(parsed.forwards)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("# no current config at {path} yet — every forward is new");
                Some(Vec::new())
            }
            Err(e) => {
                println!("# couldn't read {path} ({e}) — diff vs live config not shown");
                None
            }
        }
    } else {
        println!("# diff vs live config not shown (remote proxy) — `arena proxy apply --dry-run` reads it");
        None
    };
    if prev.is_none() && out.is_some() {
        anyhow::bail!(
            "--out needs the current proxy config to merge against, and it isn't readable here — a \
             config built from the listing alone would drop every forward the merge keeps. Use \
             `arena proxy apply` (it reads the live config; --dry-run to preview)."
        );
    }

    for (prov, e) in listing.errors() {
        eprintln!("warning: {prov} listing failed: {e}");
    }
    let plan = plan_proxy(cfg, &pxcfg, prev.as_deref().unwrap_or_default(), listing);
    if let Some(why) = &plan.abort {
        anyhow::bail!("{why} — `proxy apply` would refuse to write anything");
    }
    if plan.changes.is_empty() {
        println!("(no forwardable pods — nothing with an SSH endpoint in the name list)");
    } else {
        for line in plan.change_lines(&pxcfg.proxy_host, true) {
            println!("{line}");
        }
    }
    println!("{}", plan.summary());
    for s in &plan.skipped {
        eprintln!("warning: skipped {} — {}", s.name, s.reason);
    }

    let nginx = arena_core::proxy::render_nginx(&plan.forwards);
    println!("\n{}", plan_config_header(&pxcfg.nginx_path, prev.is_some()));
    println!("{nginx}");

    if let Some(path) = out {
        std::fs::write(path, &nginx)
            .with_context(|| format!("writing nginx config to {}", path.display()))?;
        eprintln!("wrote nginx config to {} (local only — not deployed)", path.display());
    } else {
        println!(
            "# Review, then `arena proxy apply` to {}. (--out <file> saves it locally.)",
            proxy_action(&pxcfg)
        );
    }
    Ok(())
}

/// The header above the config `proxy plan` prints. Only a real merge (the current config
/// was read) may say "write to … on the proxy"; a plan from the listing alone is labelled
/// as the preview it is, so nobody deploys it by hand.
fn plan_config_header(nginx_path: &str, merged: bool) -> String {
    if merged {
        format!("# ----- nginx config (write to {nginx_path} on the proxy) -----")
    } else {
        "# ----- nginx config: LISTING-ONLY PREVIEW, not a merge — it omits every forward \
         `proxy apply` would keep; do NOT deploy it by hand (`arena proxy apply --dry-run`) -----"
            .to_string()
    }
}

/// How long `pods list` waits for the best-effort pod details before rendering without them.
const ENRICH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `pods list`'s best-effort [`Provider::enrich`]: never fatal and never hangs the listing
/// (the HTTP client has no timeout of its own). Returns the one warning line to print when
/// details are incomplete; the pods keep whatever was filled before the failure.
async fn enrich_best_effort(
    provider: &dyn Provider,
    pods: &mut [arena_core::Pod],
    limit: std::time::Duration,
) -> Option<String> {
    let what = "warning: pod details incomplete (GPU/$/h/maintenance)";
    match tokio::time::timeout(limit, provider.enrich(pods)).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{what}: {e}")),
        Err(_) => Some(format!("{what}: timed out after {limit:?}")),
    }
}

/// `pods list`'s GPU probe: `nvidia-smi` on every pod with an endpoint, concurrently over
/// `remote` (5s connect timeout, the whole probe within `PROBE_TIMEOUT` — see
/// `metrics::fetch_with`), so a wedged pod costs the listing at most that budget. Where a
/// pod answers, what the machine itself sees overrides the provider-reported GPU (type and
/// count); one that doesn't keeps what the provider said.
async fn probe_gpus(remote: &Arc<dyn Remote>, cfg: &Config, pods: &mut [arena_core::Pod]) {
    use arena_core::metrics::{self, ProbeOpts};
    let mut jobs = Vec::new();
    for pod in pods.iter() {
        if let Ok(mut t) = SshTarget::from_pod(pod, cfg) {
            t.connect_timeout_secs = 5;
            let remote = remote.clone();
            let probe = async move { metrics::fetch_with(remote.as_ref(), &t, &ProbeOpts::default()).await };
            jobs.push((pod.name.clone(), probe));
        }
    }
    let mut gpus = std::collections::HashMap::new();
    each_pod(jobs, |_, _, name, probed| {
        if let Some((g, n)) = probed.ok().and_then(|m| m.gpu_summary().map(|g| (g, m.gpus.len() as u32))) {
            gpus.insert(name.to_string(), (g, n));
        }
    })
    .await;
    for p in pods.iter_mut() {
        if let Some((g, n)) = gpus.get(&p.name) {
            p.gpu_type = Some(g.clone());
            p.gpu_count = Some(*n);
        }
    }
}

async fn handle_pods(
    cmd: PodCmd,
    provider: &dyn Provider,
    // How we reach pods (one for the whole command): `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
) -> Result<()> {
    match cmd {
        PodCmd::List { json, probe, no_probe } => {
            // Fleet view across every configured provider (the aggregate provider), so
            // e.g. hetzner CPU pods show up alongside the GPU fleet. Grouped by provider.
            let mut pods = provider.list_pods().await?;
            pods.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
            // Best-effort details the list API omits (RunPod: GPU, $/h, host maintenance
            // window) — one extra read-only query. Never fatal: on failure or a hang the
            // list still renders, just with fewer columns filled, after one warning line.
            if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
                eprintln!("{warning}");
            }
            // Probe GPU by default for the human table; JSON stays fast/scriptable unless
            // asked. `--no-probe` always wins.
            let probe = !no_probe && (probe || !json);
            if probe {
                // nvidia-smi over SSH (same source as the TUI), concurrently, bounded per pod.
                probe_gpus(&remote, cfg, &mut pods).await;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&pods)?);
                return Ok(());
            }
            if pods.is_empty() {
                println!("(no pods)");
                return Ok(());
            }
            // Columns + footer come from arena_core::fleet so the TUI/snapshot reuse them.
            use arena_core::fleet;
            print!("{}", fleet::render_pods_table(&pods));
            println!("{}", fleet::fleet_footer(&fleet::fleet_cost(&pods)));
        }

        PodCmd::Create { names, count, add, gpu, gpus, cloud, max_price, order, disk, volume, image, bootstrap, skip_proxy, dry_run, keep_trying, retry_mins, retry_secs } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
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
            let (spec, options) = match placing {
                Some((req, base)) => (base, Some(plan_placement(cfg, provider.name(), &req).await?)),
                None => (spec_with_overrides(cfg, &ov), None),
            };
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = provider.describe(&spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
                }
                warn_no_volume(provider, &spec);
                println!("\nDry-run only — no pods created (this is a preview).");
                return Ok(());
            }
            let then = if skip_proxy { "" } else { ", then sync the proxy" };
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), provider.describe(&spec), names.join(", ")
                ),
            };
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            // Explicit names retry just the names still missing; -n/-a re-plan toward the
            // total (topup_target == 0 marks the explicit case).
            let outcome = match &options {
                Some(plan) => place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs).await,
                None => create_with_retry(provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs).await,
            };
            let (created, error) = match outcome {
                Ok(created) => (created, None),
                Err(CreateFailed { created, error }) => (created, Some(error)),
            };
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

        PodCmd::Up { names, count, add, gpu, gpus, cloud, max_price, order, disk, volume, image, bootstrap, dry_run, no_wait, keep_trying, retry_mins, retry_secs, no_setup, timeout, interval } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
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

            // Setup runs after create: reject a malformed SETUP_TIMEOUT_SECS *before*
            // creating (billing) pods, not after.
            let setup_timeouts = if no_setup {
                arena_core::setup::SetupTimeouts::default()
            } else {
                arena_core::setup::SetupTimeouts::from_config(cfg, None)?
            };
            let (spec, options) = match placing {
                Some((req, base)) => (base, Some(plan_placement(cfg, provider.name(), &req).await?)),
                None => (spec_with_overrides(cfg, &ov), None),
            };
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = provider.describe(&spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
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

            let then = format!(
                "{}, wait for endpoints{}, then update the proxy if nginx is set up",
                if retry_mins > 0 { format!(", retrying up to {retry_mins}m") } else { String::new() },
                if no_setup { "" } else { ", provision them" },
            );
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, &then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), provider.describe(&spec), names.join(", ")
                ),
            };
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            // Create as many as capacity allows (retrying if requested); only wait on
            // the ones we got. Proxy is deployed *after* this returns — i.e. once the
            // retry loop has finished topping up. Explicit names create directly. A create
            // that failed part-way still syncs the proxy for the pods it made, then fails.
            let outcome = match &options {
                Some(plan) => place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs).await,
                None => create_with_retry(provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs).await,
            };
            let created = match outcome {
                Ok(created) => created,
                Err(CreateFailed { created, error }) => {
                    if !created.is_empty() {
                        sync_proxy(cfg, provider, "up").await;
                    }
                    return Err(error);
                }
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
                println!("\n--no-wait: not polling. Run `arena proxy apply` once endpoints are assigned.");
                sync_proxy(cfg, provider, "up").await;
                return Ok(());
            }

            // Is there a proxy to deploy to? (Decides deploy-as-they-come; checked once up
            // front, with the same check the final sync makes.)
            let deployable = proxy_deployable(cfg).await.is_ok();

            // Poll until our pods have SSH endpoints or we hit the timeout, updating
            // nginx as endpoints appear (idempotent — reloads only on a real change).
            // Ctrl+C stops the wait early. Stateless: each tick re-reads truth. Each tick
            // lists per provider, so the proxy merge never mistakes a provider that
            // didn't answer for one whose pods are all gone.
            let interval = interval.max(1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            let is_ready = |p: &arena_core::Pod| p.ssh_ip.is_some() && p.ssh_port.is_some();
            println!(
                "\nWaiting up to {timeout}s for SSH endpoints{} (Ctrl+C to stop)…",
                if deployable { ", updating nginx as they come up" } else { "" }
            );
            let pods = loop {
                let listing = fleet_listing(provider).await;
                if !listing.any_ok() {
                    let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
                    eprintln!("  poll failed ({}); retrying", errs.join("; "));
                }
                let pods = listing.pods();
                let ready = pods.iter().filter(|p| want_ids.contains(&p.id) && is_ready(p)).count();
                println!("  {ready}/{} ready", want_ids.len());
                if deployable && listing.any_ok() {
                    if let Err(e) = deploy_proxy(cfg, &listing, true).await {
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
            let setup = if no_setup {
                Ok(())
            } else {
                println!("\nProvisioning the new pod(s) over SSH…");
                // By id, not name: a provider's reported name can differ from the one asked
                // for (Vast's `vast-<id>` fallback), as for the readiness wait above.
                let new: Vec<String> = created.iter().map(|p| p.id.clone()).collect();
                match just_these(provider, &new).await {
                    Ok(sel) => {
                        handle_setup(remote.clone(), cfg, true, false, None, None, false, setup_timeouts, &sel, KEYS_DIR)
                            .await
                    }
                    Err(e) => Err(e),
                }
            };
            // Final state: one more merge from a fresh listing (an endpoint can move while
            // setup runs), and the one line saying where the proxy stands — including why
            // it was skipped when there's nothing to deploy to. Runs even if setup failed:
            // the pods exist either way.
            sync_proxy(cfg, provider, "up").await;
            setup?;
        }

        PodCmd::Stop { sel, wipe_ok, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("stop")).await?;
            // Only running pods can be stopped; a selected pod that isn't is said, not hidden.
            let running = |p: &arena_core::Pod| p.status.to_uppercase().contains("RUNNING");
            for p in sel.pods.iter().filter(|p| !running(p)) {
                eprintln!("skip {} — not running ({})", p.name, p.status);
            }
            let pods: Vec<&arena_core::Pod> = sel.pods.iter().filter(|p| running(p)).collect();
            if pods.is_empty() {
                println!("(no running pods to stop)");
                return Ok(());
            }
            // What each stop costs (a RunPod pod keeps no data across stop → start), and
            // which pods would lose everything — those need --wipe-ok, even with --yes.
            let mut fates = Vec::with_capacity(pods.len());
            for p in &pods {
                fates.push(disk_fate(provider, p).await);
            }
            let lines: Vec<String> = pods
                .iter()
                .zip(&fates)
                .map(|(p, f)| format!("  {} (id={}, {}): {}", p.name, p.id, p.provider, f.describe(DiskOp::Stop)))
                .collect();
            let blocked: Vec<&str> =
                pods.iter().zip(&fates).filter(|(_, f)| f.needs_wipe_ok()).map(|(p, _)| p.name.as_str()).collect();
            if dry_run {
                println!("[dry-run] would stop {} pod(s):\n{}", pods.len(), lines.join("\n"));
                if !blocked.is_empty() && !wipe_ok {
                    println!("(would be refused without --wipe-ok: {})", blocked.join(", "));
                }
                println!("\nDry-run only — would stop {} pod(s).", pods.len());
                return Ok(());
            }
            if !blocked.is_empty() && !wipe_ok {
                anyhow::bail!("{}", wipe_refusal(DiskOp::Stop, &blocked));
            }
            if !confirm(yes, &format!("Will stop {} running pod(s):\n{}", pods.len(), lines.join("\n")))? {
                println!("aborted.");
                return Ok(());
            }
            // The fleet provider routes each stop to the backend that owns the pod id.
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


        PodCmd::InitBranches { sel, week, day, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_init_branches(remote, cfg, week, day, &sel, dry_run, yes).await?;
        }

        PodCmd::Pull { label, dir, max_size, remote_path, no_git, no_big, sel, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_pull(cfg, label, &dir, max_size, remote_path, no_git, no_big, &sel, dry_run, yes).await?;
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

        PodCmd::Reimage { sel, image, skip_proxy, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("reimage")).await?;
            handle_reimage(provider, cfg, &sel, image, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Replace { target, gpu, gpus, cloud, disk, volume, image, keep_old, skip_proxy, dry_run } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap: false };
            handle_replace(remote, cfg, &target, &ov, keep_old, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Migrate { cmd } => match cmd {
            MigrateCmd::Copy { target, gpu, gpus, cloud, disk, volume, image, bootstrap, dry_run } => {
                let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap };
                handle_migrate_copy(remote, cfg, &target, &ov, dry_run, yes).await?;
            }
            MigrateCmd::Cutover { target, yes: y, skip_proxy, dry_run } => {
                handle_migrate_cutover(remote.as_ref(), cfg, &target, skip_proxy, dry_run, yes || y).await?;
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

        PodCmd::Terminate { target, all, revoke_key, skip_proxy, dry_run } => {
            let or = openrouter_client(cfg);
            let book = KeyBook::new(OPENROUTER_KEYS_CSV, or.as_ref());
            let target = match (all, target) {
                (true, _) => None,
                (false, Some(t)) => Some(t),
                (false, None) => anyhow::bail!("specify a pod (name or id) to terminate, or pass --all"),
            };
            handle_terminate(provider, cfg, &book, target.as_deref(), revoke_key, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Backup { sel, no_pull, dry_run, message } => {
            // Resolved once, before the prompt (a typo fails before asking), and shared by the
            // git push and the pull.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            if sel.pods.is_empty() {
                println!("(no pods to back up)");
                return Ok(());
            }
            let scope = format!("the ARENA tree of {} pod(s) ({})", sel.pods.len(), sel.names());
            let what = if no_pull {
                format!("commit + push {scope}, each on its current branch (main/master skipped)")
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
            let git_result = handle_backup(remote, cfg, !dry_run, message, &sel).await;
            if !no_pull {
                println!();
                let dir = local_backup_dir(cfg);
                handle_pull(cfg, None, &dir, None, None, false, false, &sel, dry_run, true).await?;
            }
            git_result?;
        }
        PodCmd::Setup { sel, dry_run, force, hf_token, cc_token, zsh_install, timeout } => {
            // Budgets first: a malformed SETUP_TIMEOUT_SECS fails before anything else.
            let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, timeout)?;
            // Resolved before the prompt, so a typo fails before asking and the prompt names
            // exactly the pods that will be provisioned.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            if !dry_run
                && !sel.pods.is_empty()
                && !confirm(
                    yes,
                    &format!(
                        "Will provision {} pod(s) over SSH (deploy key, ~/.name, repo): {}",
                        sel.pods.len(),
                        sel.names()
                    ),
                )?
            {
                println!("aborted.");
                return Ok(());
            }
            handle_setup(remote, cfg, !dry_run, force, hf_token, cc_token, zsh_install, timeouts, &sel, KEYS_DIR).await?;
        }
        PodCmd::SetBranch { branch, sel, hard, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("switch")).await?;
            handle_set_branch(remote, cfg, &branch, &sel, hard, dry_run, yes).await?;
        }
        PodCmd::Run { sel, command, timeout, dry_run } => {
            let cmd = command.join(" ");
            let budget = Duration::from_secs(timeout);
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_run(remote, cfg, &sel, &cmd, budget, dry_run, yes, /*confirm*/ true, /*compact*/ false).await?;
        }
        PodCmd::Test { deep: true, sel, json, verbose } => {
            // Read-only: no confirm. Config first: a malformed MIN_DRIVER_VERSION fails before
            // the fleet is even listed.
            arena_core::health::HealthPolicy::from_config(cfg)?;
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_deep_test(provider, remote, cfg, &sel, json, verbose).await?;
        }
        PodCmd::Test { deep: false, sel, .. } => {
            // Read-only: no confirm, compact one-line-per-pod output.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
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

/// Run `cmd` on every selected pod with an SSH endpoint, concurrently, each pod within
/// `timeout` (a pod that runs over is a failure; the others carry on). `compact` prints one
/// line per pod (last stdout line); otherwise a per-pod block. `confirm_needed` gates it
/// behind the y/N prompt (arbitrary exec); read-only checks pass false.
#[allow(clippy::too_many_arguments)]
async fn handle_run(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    // The pods to run on, resolved by the caller.
    sel: &Selected,
    cmd: &str,
    timeout: Duration,
    dry_run: bool,
    yes: bool,
    confirm_needed: bool,
    compact: bool,
) -> Result<()> {
    // Pods we can't reach yet are reported (not silently dropped), so a "run on every pod"
    // can't quietly skip part of the fleet while reporting success.
    let mut targets = sel.named_targets(cfg)?;
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint)");
        return Ok(());
    }

    // Source the participants' ~/.zshrc and activate the conda env, so commands see the
    // participants' `arena-env` (python/packages) and the token exports from setup.
    // `CONDA_ENV=""` in config disables activation (still sources the rc for tokens).
    let conda_env = cfg.get("CONDA_ENV").unwrap_or("arena-env");
    let remote_cmd = arena_core::ssh::login_shell_wrap(cmd, Some(conda_env));

    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if dry_run {
        println!(
            "[dry-run] would run on {} pod(s) ({}s budget each): {names}\n  {remote_cmd}",
            targets.len(),
            timeout.as_secs()
        );
        return Ok(());
    }
    if confirm_needed && !confirm(yes, &format!("Run `{cmd}` on {} pod(s) over SSH: {names}", targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let results = run_fleet(&remote, targets, &remote_cmd, timeout).await;
    let (lines, bad) = render_run(&results, compact);
    for line in lines {
        println!("{line}");
    }
    // Propagate partial failure to the exit code, like the mutating sibling handlers — so
    // a scripted `pods test` / `pods run` can't pass while the command failed on pods.
    if bad > 0 {
        anyhow::bail!("{bad} pod(s) failed");
    }
    Ok(())
}

/// One pod's `pods run` / `pods test` outcome: its output (or why it failed).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunResult {
    name: String,
    text: String,
    ok: bool,
}

/// Run `remote_cmd` on every target concurrently over `remote`, each within `timeout`;
/// results sorted by pod name. A non-zero exit, a timeout (`timed out after Ns`) or a
/// crashed task is a failed result — never dropped from the tally.
async fn run_fleet(
    remote: &Arc<dyn Remote>,
    targets: Vec<(String, SshTarget)>,
    remote_cmd: &str,
    timeout: Duration,
) -> Vec<RunResult> {
    use arena_core::ssh::strip_interactive_noise;
    let jobs = targets.into_iter().map(|(name, t)| (name, t, remote_cmd.to_string())).collect();
    let mut results = Vec::new();
    exec_each_pod(remote, jobs, timeout, |_, _, name, call| {
        // a no-PTY shell can emit harmless job-control chatter; strip it.
        let (text, ok) = match call {
            Ok(out) if out.success => (strip_interactive_noise(out.stdout.trim()).trim().to_string(), true),
            Ok(out) => {
                (format!("exit {:?}: {}", out.code, strip_interactive_noise(out.stderr.trim()).trim()), false)
            }
            Err(why) => (why, false),
        };
        results.push(RunResult { name: name.to_string(), text, ok });
    })
    .await;
    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

/// The `pods run` / `pods test` report: `compact` = one `<name>  <last line>` row per pod
/// (`✗ <why>` on failure, e.g. `✗ timed out after 90s`), else a `── <name>` block with the
/// full output; then the `N ok, M failed` tally. Returns the lines and the failure count.
/// Pure, so per-pod reporting is tested without a terminal.
fn render_run(results: &[RunResult], compact: bool) -> (Vec<String>, usize) {
    let (mut lines, mut ok, mut bad) = (Vec::new(), 0, 0);
    for r in results {
        if r.ok {
            ok += 1
        } else {
            bad += 1
        }
        if compact {
            let line = r.text.lines().last().unwrap_or("").trim();
            let shown = if r.ok { line.to_string() } else { format!("✗ {line}") };
            lines.push(format!("{:<22} {shown}", r.name));
        } else {
            lines.push(format!("\n── {} {}", r.name, if r.ok { "" } else { "(FAILED)" }));
            lines.push(r.text.clone());
        }
    }
    lines.push(format!("\n{ok} ok, {bad} failed"));
    (lines, bad)
}

/// `pods test --deep`: run the deep check on the pods (see [`deep_check_fleet`]), print
/// the pass/warn/fail table (or JSON), and fail the command if any pod FAILed — so a
/// script or `up --check` can gate on it. Warnings (slow network, maintenance, …) are
/// reported but don't change the exit status.
async fn handle_deep_test(
    provider: &dyn Provider,
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    json: bool,
    verbose: bool,
) -> Result<()> {
    use arena_core::health::Status;
    let results = deep_check_fleet(provider, &remote, cfg, sel).await?;
    let (stdout, stderr) = render_deep_test(&results, json, verbose)?;
    print!("{stdout}");
    for line in stderr {
        eprintln!("{line}");
    }
    let failed = results.iter().filter(|h| h.status == Status::Fail).count();
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed the deep check");
    }
    Ok(())
}

/// What `pods test --deep` prints: (stdout, stderr lines). With `json`, stdout is always
/// the JSON array — `[]` when no pod could be checked (an empty or still-booting fleet is
/// exactly when a `| jq` or `up --check` consumer must not get prose) — and the human
/// summary goes to stderr. Pure, so that contract is tested without capturing stdout.
fn render_deep_test(
    results: &[arena_core::health::PodHealth],
    json: bool,
    verbose: bool,
) -> Result<(String, Vec<String>)> {
    use arena_core::health::{render_report, render_summary};
    const NONE: &str = "(no pods with an SSH endpoint)";
    Ok(match (json, results.is_empty()) {
        (true, true) => ("[]\n".into(), vec![NONE.into()]),
        (true, false) => (format!("{}\n", serde_json::to_string_pretty(results)?), render_summary(results)),
        (false, true) => (format!("{NONE}\n"), vec![]),
        (false, false) => (render_report(results, verbose), vec![]),
    })
}

/// Deep-check the selected pods concurrently over `remote`, one exec each bounded by
/// [`arena_core::health::DEEP_CHECK_TIMEOUT`]; results sorted by name. A pod the operator
/// named that has no SSH endpoint is a FAIL (it was asked about and couldn't be checked);
/// one swept in by the default/`--all` is skipped with a note, like `pods test`. An
/// unreachable or timed-out pod is a FAIL with the reason. The provider's maintenance
/// windows are fetched alongside the SSH runs (best-effort, bounded) — it's an API-side
/// fact the pod can't report.
///
/// Results are matched back to pods by position, not name: two pods can share a name (a
/// double create, a leftover), and each must get its own row and verdict — keyed by name,
/// one would take the other's result and the other would vanish from the report (and from
/// the exit status). A pod that was probed but has no result is a FAIL, never dropped.
async fn deep_check_fleet(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
) -> Result<Vec<arena_core::health::PodHealth>> {
    use arena_core::health::{deep_check_command, parse_deep, HealthPolicy, PodHealth, DEEP_CHECK_TIMEOUT};
    // Config first: a malformed MIN_DRIVER_VERSION fails before any pod is touched.
    let policy = HealthPolicy::from_config(cfg)?;
    let mut pods = sel.pods.clone();
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    for (name, ids) in duplicate_names(&pods) {
        eprintln!(
            "warning: {} pods are named {name} (ids {}) — each is checked and reported separately",
            ids.len(),
            ids.join(", ")
        );
    }

    /// What happens to each pod (by its index in `pods`).
    enum Slot {
        /// No endpoint and not asked for by name: left out, with a note.
        Skipped,
        /// Asked for by name but has no endpoint: a FAIL.
        NoEndpoint,
        Probed,
    }
    // `CONDA_ENV=""` disables activation (like `pods run`); python is then whatever the
    // rc puts on PATH.
    let cmd = deep_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env")));
    let mut jobs = Vec::new();
    let mut slots = Vec::with_capacity(pods.len());
    for (i, pod) in pods.iter().enumerate() {
        slots.push(match SshTarget::from_pod(pod, cfg) {
            Ok(t) => {
                jobs.push(((i, pod.name.clone()), t, cmd.clone()));
                Slot::Probed
            }
            Err(_) if sel.named => Slot::NoEndpoint,
            Err(_) => {
                eprintln!("skip {} — no SSH endpoint yet", pod.name);
                Slot::Skipped
            }
        });
    }
    if !jobs.is_empty() {
        eprintln!(
            "Deep-checking {} pod(s) ({}s budget each)…",
            jobs.len(),
            DEEP_CHECK_TIMEOUT.as_secs()
        );
    }
    let mut calls: std::collections::HashMap<usize, PodCall> = std::collections::HashMap::new();
    let probe = exec_each_pod(remote, jobs, DEEP_CHECK_TIMEOUT, |done, total, (i, name), call| {
        match &call {
            Ok(out) if out.success => eprintln!("[{done}/{total}] {name}: checked"),
            failed => eprintln!("{}", failure_line(done, total, name, failed)),
        }
        calls.insert(*i, call);
    });
    // `enrich` fills fields in place (a slice: it can't add, drop or move pods), so the
    // indices the jobs carry still point at the same pods afterwards.
    let (warning, ()) = tokio::join!(enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT), probe);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }

    let mut results = Vec::new();
    for (i, pod) in pods.iter().enumerate() {
        match slots[i] {
            Slot::Skipped => continue,
            Slot::NoEndpoint => {
                results.push(PodHealth::unreachable(pod, format!("no SSH endpoint yet (status {})", pod.status)));
                continue;
            }
            Slot::Probed => {}
        }
        let Some(call) = calls.remove(&i) else {
            results.push(PodHealth::unreachable(pod, "no result from the check"));
            continue;
        };
        results.push(match call {
            Err(why) => PodHealth::unreachable(pod, why),
            Ok(out) => {
                let facts = parse_deep(&out.stdout);
                let stderr = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                if !facts.started && !out.success {
                    // ssh itself failed (255 = refused/auth/…): say that, not "no output".
                    PodHealth::unreachable(pod, format!("exit {:?}: {stderr}", out.code))
                } else {
                    let started = facts.started;
                    let mut health = PodHealth::checked(pod, facts, &policy);
                    // The script never started (e.g. no `base64` on the pod): stderr says why.
                    if !started && !stderr.is_empty() {
                        for c in health.checks.iter_mut().filter(|c| c.name == "script") {
                            c.detail = format!("{} ({stderr})", c.detail);
                        }
                    }
                    health
                }
            }
        });
    }
    Ok(results)
}

/// Names held by more than one pod, with those pods' ids (in listing order) — for the
/// warning that the report will show the name twice. Sorted by name.
fn duplicate_names(pods: &[arena_core::Pod]) -> Vec<(String, Vec<String>)> {
    let mut by_name: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    for p in pods {
        by_name.entry(&p.name).or_default().push(p.id.clone());
    }
    by_name.into_iter().filter(|(_, ids)| ids.len() > 1).map(|(n, ids)| (n.to_string(), ids)).collect()
}

/// Switch the selected pods (those with an SSH endpoint) to `branch` over SSH.
/// Gentle (fetch+checkout+ff-pull) by default; `hard` force-resets to `origin/<branch>`,
/// discarding local commits/changes. Dry-run prints the exact command per pod. Pods run
/// concurrently, each within [`BRANCH_TIMEOUT`] (it used to be serial, so one wedged pod
/// held up every pod after it).
#[allow(clippy::too_many_arguments)]
async fn handle_set_branch(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    branch: &str,
    // The pods to switch, resolved by the caller (which demands names or --all).
    sel: &Selected,
    hard: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let key = cfg.get("GIT_SSH_KEY_REMOTE");
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, key, hard);

    // Unreachable pods are reported — a silently-skipped pod left on a stale/diverged branch
    // (especially under --hard) is exactly what we don't want.
    let targets = sel.named_targets(cfg)?;
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
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will {how}: {names}"))? {
        println!("aborted.");
        return Ok(());
    }

    let total = targets.len();
    println!("Switching {total} pod(s) to '{branch}' over SSH ({}s budget each)…", BRANCH_TIMEOUT.as_secs());
    let jobs = targets.into_iter().map(|(name, t)| (name, t, cmd.clone())).collect();
    let (mut ok, mut failed) = (0, 0);
    exec_each_pod(&remote, jobs, BRANCH_TIMEOUT, |done, total, name, call| match &call {
        Ok(out) if out.success => {
            println!("[{done}/{total}] ✓ {name} (on {branch})");
            ok += 1;
        }
        _ => {
            println!("{}", failure_line(done, total, name, &call));
            failed += 1;
        }
    })
    .await;
    println!("\nswitched {ok}/{total}");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to switch branch");
    }
    Ok(())
}

/// `pods reimage`: swap image + re-seed keys in place (disk wiped), then re-point the proxy.
#[allow(clippy::too_many_arguments)]
async fn handle_reimage(
    provider: &dyn Provider,
    cfg: &Config,
    // The pods to reimage, resolved by the caller (which demands names or --all).
    sel: &Selected,
    image: Option<String>,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let pods = &sel.pods;
    if pods.is_empty() {
        println!("(no pods to reimage)");
        return Ok(());
    }
    let image = image.unwrap_or_else(|| PodSpec::from_config(cfg).image);
    if image.is_empty() {
        anyhow::bail!("no image: pass --image or set RUNPOD_DOCKER_IMAGE");
    }
    let pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    if pubkeys.is_empty() {
        anyhow::bail!("no readable public keys from SHARED_SSH_KEY_PATH/GIT_SSH_KEY_LOCAL — refusing to lock pods out");
    }
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    println!("image: {image}\nkeys:  {}", pubkeys.iter().map(|k| k.split_whitespace().last().unwrap_or("?")).collect::<Vec<_>>().join(", "));
    if dry_run {
        println!("[dry-run] would reimage {} pod(s): {}", pods.len(), names.join(" "));
        return Ok(());
    }
    if !confirm(yes, &format!("Will reimage {} pod(s), WIPING their disks: {}", pods.len(), names.join(" ")))? {
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
                eprintln!("[failed] {}: {e}", pod.name);
                failed.push(pod.name.clone());
            }
        }
    }
    // A reimaged pod comes back on a new SSH port, so wait for it to settle before the
    // sync (nothing to sync if every reimage failed).
    if !skip_proxy && failed.len() < pods.len() {
        println!("waiting for SSH endpoints…");
        for pod in pods.iter().filter(|p| !failed.contains(&p.name)) {
            if let Err(e) = wait_for_stable_endpoint(provider, &pod.id, 30, 600).await {
                eprintln!("warning: {} has no endpoint yet ({e})", pod.name);
            }
        }
        sync_proxy(cfg, provider, "reimage").await;
    }
    if !failed.is_empty() {
        anyhow::bail!("{} pod(s) failed to reimage: {}", failed.len(), failed.join(" "));
    }
    Ok(())
}

/// Where a machine's OpenRouter key lives, for the lifecycle commands that move or drop it
/// (`rename`, `terminate --revoke-key`): the local CSV `copy-keys` distributes from, and —
/// when `OPENROUTER_PROVISIONING_KEY` is set — the provisioning API, where keys are found by
/// name (`keys gen` names each after its machine, i.e. the pod's name). A struct so tests
/// point it at a temp file and a fake API.
struct KeyBook<'a> {
    /// `keys/openrouter_api_keys.csv` — a symlink to the per-cohort file
    /// (`ensure_cohort_keys_file`); reads and writes go through it to that file.
    csv: PathBuf,
    api: Option<&'a dyn arena_core::openrouter::KeyApi>,
}

impl<'a> KeyBook<'a> {
    fn new<K: arena_core::openrouter::KeyApi + 'a>(csv: impl Into<PathBuf>, api: Option<&'a K>) -> Self {
        Self { csv: csv.into(), api: api.map(|k| k as &dyn arena_core::openrouter::KeyApi) }
    }

    /// The CSV's text, or `None` when there is no keys file (nothing to move or drop).
    fn read_csv(&self) -> Option<String> {
        std::fs::read_to_string(&self.csv).ok()
    }

    /// Replace the CSV atomically, through the symlink (the per-cohort file it points at is
    /// rewritten, the link kept), keeping its mode — it holds secrets.
    fn write_csv(&self, text: &str) -> std::io::Result<()> {
        replace_file(&self.csv, text)
    }
}

/// The provisioning-API client, when `OPENROUTER_PROVISIONING_KEY` is set (else `None`:
/// key names on OpenRouter are left alone, and `--revoke-key` is refused).
fn openrouter_client(cfg: &Config) -> Option<arena_core::openrouter::OpenRouter> {
    cfg.get("OPENROUTER_PROVISIONING_KEY").filter(|s| !s.is_empty()).map(arena_core::openrouter::OpenRouter::new)
}

/// `pods terminate <target>` / `--all`, optionally `--revoke-key`. Revocation runs only for
/// pods that actually terminated (a pod still running keeps a working key), after the
/// terminates and the proxy sync — a failed revoke is reported per machine and fails the
/// exit, but never stands in the way of a terminate.
#[allow(clippy::too_many_arguments)]
async fn handle_terminate(
    provider: &dyn Provider,
    cfg: &Config,
    book: &KeyBook<'_>,
    // One pod (the shared single-pod matcher), or `None` = the whole fleet (`--all`).
    target: Option<&str>,
    revoke_key: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // Before anything is listed or touched: revoking needs the provisioning API.
    if revoke_key && book.api.is_none() {
        anyhow::bail!(
            "--revoke-key needs OPENROUTER_PROVISIONING_KEY (keys are found and deleted through the \
             provisioning API) — set it, or drop --revoke-key and `arena keys revoke` later"
        );
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut listed = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pods: Vec<arena_core::Pod> = match target {
        None => {
            listed.sort_by(|a, b| a.name.cmp(&b.name));
            listed
        }
        // One pod, matched like every other command's targets (the shared selector's
        // single-pod rule): a typo names the closest pods, a range/`all` is refused, and a
        // name two pods share is refused — pass the id.
        Some(t) => {
            let i = arena_core::selector::resolve_one(&Naming::from_config(cfg), &listed, t)?;
            vec![listed.swap_remove(i)]
        }
    };
    if pods.is_empty() {
        println!("(no pods to terminate)");
        return Ok(());
    }
    let label = |p: &arena_core::Pod| format!("{} (id={}, {})", p.name, p.id, p.provider);
    let then_keys = if revoke_key {
        format!(", revoke the OpenRouter key(s) named after each and drop their rows from {}", book.csv.display())
    } else {
        String::new()
    };
    let then_sync = if skip_proxy { "" } else { ", then sync the proxy" };
    if dry_run {
        for p in &pods {
            println!("[dry-run] would terminate {}", label(p));
            if revoke_key {
                println!("[dry-run]   then revoke OpenRouter key(s) named {} and drop its row from {}", p.name, book.csv.display());
            }
        }
        println!("\nDry-run only — would terminate {} pod(s){then_keys}{then_sync} (preview).", pods.len());
        return Ok(());
    }
    let what = match target {
        None => format!("Will TERMINATE ALL {} pod(s) — irreversible{then_keys}{then_sync}.", pods.len()),
        Some(_) => format!("Will TERMINATE {} — irreversible{then_keys}{then_sync}.", label(&pods[0])),
    };
    if !confirm(yes, &what)? {
        println!("aborted.");
        return Ok(());
    }
    // The fleet provider routes each terminate to the backend that owns the pod id.
    let mut gone: Vec<String> = Vec::new();
    for p in &pods {
        match provider.terminate_pod(&p.id).await {
            Ok(()) => {
                println!("[terminated] {}", if target.is_some() { label(p) } else { p.name.clone() });
                gone.push(p.name.clone());
            }
            // One named pod: its failure is the command's error (nothing else to do).
            Err(e) if target.is_some() => return Err(e.into()),
            Err(e) => eprintln!("[FAILED] {}: {e}", p.name),
        }
    }
    if target.is_none() {
        println!("\nterminated {}/{}", gone.len(), pods.len());
    }
    // Sync whatever did go (the merge only drops what the providers confirm gone).
    if !gone.is_empty() && !skip_proxy {
        sync_proxy(cfg, provider, "terminate").await;
    }
    let revoke_failed = if revoke_key && !gone.is_empty() { revoke_machine_keys(book, &gone).await } else { Vec::new() };
    let mut problems = Vec::new();
    if gone.len() < pods.len() {
        problems.push(format!("{} pod(s) failed to terminate", pods.len() - gone.len()));
    }
    if !revoke_failed.is_empty() {
        problems.push(format!(
            "revoking the OpenRouter key failed for {} — the pod(s) ARE terminated; retry with `arena keys revoke {}`",
            revoke_failed.join(", "),
            revoke_failed.join(" ")
        ));
    }
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("; "));
    }
    Ok(())
}

/// Revoke every OpenRouter key named after each of `machines` (pods just terminated) and
/// drop their rows from the keys CSV. One key listing for the batch. A machine with no key
/// is said, not failed. Its CSV row goes only once nothing is left under its name on
/// OpenRouter: a row whose key couldn't be deleted stays, as the record that it still works.
/// Returns the machines whose revoke failed.
async fn revoke_machine_keys(book: &KeyBook<'_>, machines: &[String]) -> Vec<String> {
    let Some(api) = book.api else { return machines.to_vec() }; // the caller checked up front
    let keys = match api.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            for m in machines {
                eprintln!("[keys] ✗ {m}: listing OpenRouter keys failed: {e}");
            }
            return machines.to_vec();
        }
    };
    let mut csv = book.read_csv();
    let (mut failed, mut dropped) = (Vec::new(), Vec::new());
    for m in machines {
        let mine: Vec<&arena_core::openrouter::KeyInfo> = keys.iter().filter(|k| k.name.as_deref() == Some(m)).collect();
        let mut errs = Vec::new();
        for k in &mine {
            if let Err(e) = api.delete_key(&k.hash).await {
                errs.push(e.to_string());
            }
        }
        if !errs.is_empty() {
            eprintln!("[keys] ✗ {m}: revoke failed ({}) — its CSV row is kept", errs.join("; "));
            failed.push(m.clone());
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
            0 if row.is_empty() => println!("[keys] = {m}: no OpenRouter key, no CSV row"),
            0 => println!("[keys] ✓ {m}: no OpenRouter key{row}"),
            n => println!("[keys] ✓ {m}: revoked {n} OpenRouter key(s){row}"),
        }
    }
    if let (Some(text), false) = (&csv, dropped.is_empty()) {
        if let Err(e) = book.write_csv(text) {
            eprintln!("[keys] ✗ couldn't update {}: {e} — rows kept for {}", book.csv.display(), dropped.join(", "));
            failed.extend(dropped);
        }
    }
    failed
}

/// What a restart (or a stop, then a start) does to one pod's files: what its confirm text
/// says, and whether it needs `--wipe-ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskFate {
    /// The backend keeps the disk (a Hetzner VM).
    Kept,
    /// The container is reset to its image. `volume_gb`: the persistent volume (at
    /// /workspace) that survives — `Some(0)` = none, `None` = couldn't be read.
    Wiped { volume_gb: Option<u32> },
}

/// The two operations that reset a container disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskOp {
    Restart,
    Stop,
}

impl DiskFate {
    /// A wipe with nothing to keep the work — no volume, or none confirmed (an unreadable
    /// spec counts as none: guessing "it has one" is the data-loss direction).
    fn needs_wipe_ok(self) -> bool {
        matches!(self, DiskFate::Wiped { volume_gb: None | Some(0) })
    }

    /// One line for the prompt / dry run: what's lost and what survives. Pure, so tested.
    fn describe(self, op: DiskOp) -> String {
        use arena_core::provider::runpod_v2::VOLUME_MOUNT_PATH as VOL;
        let wipe = match op {
            DiskOp::Restart => "⚠ WIPES the container disk (reset to the image)",
            DiskOp::Stop => "⚠ discards the container disk (it starts again as a fresh image)",
        };
        let lost = "participants' files, ~/.name, setup's git remote, the distributed keys";
        match self {
            DiskFate::Kept => match op {
                DiskOp::Restart => "disk kept — a hard reset (power-cycle): running processes die, files stay".into(),
                DiskOp::Stop => "disk kept — powered off, files stay".into(),
            },
            DiskFate::Wiped { volume_gb: Some(v) } if v > 0 => {
                format!("{wipe}: everything outside {VOL} is lost ({lost}); the {v} GB volume at {VOL} survives")
            }
            DiskFate::Wiped { volume_gb: Some(_) } => {
                format!("{wipe} and it has NO persistent volume: nothing survives — {lost}")
            }
            DiskFate::Wiped { volume_gb: None } => {
                format!("{wipe}; couldn't read whether it has a volume, so assume nothing survives — {lost}")
            }
        }
    }
}

/// A pod's [`DiskFate`]: the owning backend says whether a restart wipes; if it does, the
/// pod's spec says whether a volume survives (read-only, bounded like a listing — an error
/// or a stall reads as "unknown", which the gate treats like no volume).
async fn disk_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    if !provider.restart_wipes_container_disk(pod) {
        return DiskFate::Kept;
    }
    let spec = tokio::time::timeout(arena_core::provider::LIST_TIMEOUT, provider.pod_spec(&pod.id)).await;
    DiskFate::Wiped { volume_gb: match spec {
        Ok(Ok(s)) => Some(s.volume_gb),
        _ => None,
    } }
}

/// Why a restart/stop is refused without `--wipe-ok` (pure, so tested): it would destroy
/// everything on these pods, and `--yes` is not an answer to that — the operator has to say
/// so explicitly, after a backup.
fn wipe_refusal(op: DiskOp, names: &[&str]) -> String {
    let (verb, done) = match op {
        DiskOp::Restart => ("restart", "resets the container disk to the image"),
        DiskOp::Stop => ("stop", "discards the container disk (a stopped RunPod pod keeps no data)"),
    };
    let alt = match op {
        DiskOp::Restart => "",
        DiskOp::Stop => " If the group is done, terminate instead.",
    };
    format!(
        "refusing to {verb} {} without --wipe-ok: this {done} and there's no persistent volume (or \
         none could be confirmed), so nothing on it survives — participants' work included. Back up \
         first (`arena pods backup {}`), then re-run with --wipe-ok (--yes alone isn't enough).{alt}",
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

/// `pods restart <target>`. On a backend whose restart wipes the container disk (RunPod;
/// Vast assumed) the prompt says plainly what's lost and what survives, a pod with no
/// volume (or none confirmed) is refused without `--wipe-ok` — even with `--yes` — and
/// afterwards the pod is brought back usable: wait for its endpoint to settle, re-run setup
/// on it (`~/.name`, deploy key + git remote, API keys; `--no-setup` skips), sync the
/// proxy. A Hetzner reset keeps the disk: no gate, no re-setup, just the sync.
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
    let fate = disk_fate(provider, &pod).await;
    let wiped = matches!(fate, DiskFate::Wiped { .. });
    let setup = wiped && !opts.no_setup;
    if setup {
        // Setup is what brings a wiped pod back: make sure it can run before wiping one.
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("re-running setup after the restart needs the setup config — fix it, or pass --no-setup")?;
    }
    let then = match (wiped, setup, opts.skip_proxy) {
        (true, true, false) => "then: wait for its SSH endpoint, re-run setup on it (~/.name, deploy key + git remote, API keys), sync the proxy",
        (true, true, true) => "then: wait for its SSH endpoint, re-run setup on it (~/.name, deploy key + git remote, API keys)",
        (true, false, false) => "then: wait for its SSH endpoint and sync the proxy (no setup: it comes back as the bare image)",
        (true, false, true) => "then: nothing (no setup, no proxy sync: it comes back as the bare image)",
        (false, _, false) => "then: sync the proxy",
        (false, _, true) => "then: nothing",
    };
    let what = format!("restart {label}:\n  {}\n  {then}", fate.describe(DiskOp::Restart));
    if opts.dry_run {
        println!("[dry-run] would {what}");
        if fate.needs_wipe_ok() && !opts.wipe_ok {
            println!("(would be refused without --wipe-ok)");
        }
        return Ok(());
    }
    if fate.needs_wipe_ok() && !opts.wipe_ok {
        anyhow::bail!("{}", wipe_refusal(DiskOp::Restart, &[pod.name.as_str()]));
    }
    if !confirm(opts.yes, &format!("Will {what}"))? {
        println!("aborted.");
        return Ok(());
    }
    // The fleet provider routes it to the backend that owns the pod id.
    provider.restart_pod(&pod.id).await?;
    println!("[restarted] {label}");

    let mut outcome: Result<()> = Ok(());
    if wiped && (setup || !opts.skip_proxy) {
        println!("waiting for {}'s SSH endpoint to settle…", pod.name);
        match wait_for_stable_endpoint_every(provider, &pod.id, settle.stable_secs, settle.timeout_secs, settle.poll).await {
            Err(e) => outcome = Err(anyhow::anyhow!("no SSH endpoint ({e})")),
            Ok(_) if setup => {
                println!("\nRe-provisioning {} (the restart reset it to the image)…", pod.name);
                outcome = match just_these(provider, std::slice::from_ref(&pod.id)).await {
                    Ok(sel) => handle_setup(remote, cfg, true, false, None, None, false, timeouts, &sel, keys_dir).await,
                    Err(e) => Err(e),
                };
            }
            Ok(_) => {}
        }
    }
    // Runs whatever happened to setup: the restart happened either way.
    if !opts.skip_proxy {
        sync_proxy(cfg, provider, "restart").await;
    }
    if wiped && !setup {
        println!(
            "(--no-setup: {0} is back as the bare image — `arena pods setup {0}` restores ~/.name, the git remote and keys)",
            pod.name
        );
    }
    outcome.map_err(|e| {
        anyhow::anyhow!(
            "{0} restarted, but it is NOT set up again: {e:#}\n  → run `arena pods setup {0}` once it answers \
             (not another restart — that would wipe it again)",
            pod.name
        )
    })
}

/// Printed by every rename: the one thing that can't follow a name.
const MACHINE_NAME_NOTE: &str = "note: each renamed pod's MACHINE_NAME env var keeps the old name — env only \
     changes with a reimage (which wipes the disk); ~/.name carries the new one.";

/// `pods rename`: validate every (old → new) pair, confirm, rename via the provider's
/// metadata-only rename, then bring along everything keyed by the machine name (PLAN 1.D):
/// `~/.name` on the pod, the OpenRouter key mapping, and the proxy (stable ports follow
/// names). The pod's `MACHINE_NAME` env var can't follow — env is fixed at create/reimage —
/// so the summary says so. Nothing after the provider rename is fatal: the pods are renamed
/// either way, so a pod that can't be reached or a key that can't be moved is reported.
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
    let pods = provider.list_pods().await.context("listing pods")?;
    let plan = plan_renames(prefix, &cfg.machine_names, &pods, &request)?;
    if plan.is_empty() {
        println!("nothing to rename.");
        return Ok(());
    }
    let summary: Vec<String> = plan.iter().map(|r| format!("  {} → {} (id={})", r.old, r.new, r.id)).collect();
    let steps = rename_followup_steps(prefix, &plan, &pods, book, skip_proxy);
    let what = format!("rename {} pod(s):\n{}\nthen:\n{}", plan.len(), summary.join("\n"), steps.join("\n"));
    if dry_run {
        println!("[dry-run] would {what}");
        println!("{MACHINE_NAME_NOTE}");
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
                "renaming {} → {} failed: {e}\n{}",
                r.old,
                r.new,
                rename_failure_hint(&request, i, plan.len())
            ));
            break;
        }
        println!("[renamed] {} → {}", r.old, r.new);
        done.push(r);
    }
    // The pods renamed so far get their ~/.name and key mapping even when the batch stopped:
    // a re-run renames only the rest, so these would otherwise keep the old ones for good.
    let missed = rewrite_name_files(remote, cfg, prefix, &done, &pods).await;
    let key_warnings = move_key_mappings(book, &done).await;
    if let Some(failure) = failure {
        // Deliberately no sync on a partial batch: in a `--from-prefix` rename the pods not
        // renamed yet still carry the old prefix, which has no slot in the current list — a
        // sync would drop their forwards. Left alone, every old forward keeps routing (a
        // renamed pod keeps its port: same list index), until the re-run.
        for line in rename_report(&missed, &key_warnings) {
            eprintln!("{line}");
        }
        anyhow::bail!("{failure}");
    }
    if skip_proxy {
        println!("(proxy not synced — run `arena proxy apply`)");
    } else {
        sync_proxy(cfg, provider, "rename").await;
    }
    for line in rename_report(&missed, &key_warnings) {
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
    use arena_core::apikeys::{rename_csv_host, CsvRename};
    use arena_core::setup::short_name;
    let mut out = Vec::new();
    let shorts: Vec<String> = plan.iter().map(|r| format!("{} ← '{}'", r.new, short_name(prefix, &r.new))).collect();
    out.push(format!("  1. rewrite ~/.name (export MACHINE_NAME='<short>') over SSH: {}", shorts.join(", ")));
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
            let (mut moves, mut taken) = (Vec::new(), Vec::new());
            for r in plan {
                match rename_csv_host(&text, &r.old, &r.new) {
                    CsvRename::Renamed(_) => moves.push(format!("{} → {}", r.old, r.new)),
                    CsvRename::Taken => taken.push(r.new.as_str()),
                    CsvRename::NoRow => {}
                }
            }
            out.push(if moves.is_empty() {
                format!("  2. OpenRouter key mapping: no rows for these machines in {csv}")
            } else {
                format!("  2. OpenRouter key mapping: move rows in {csv}: {}", moves.join(", "))
            });
            if !taken.is_empty() {
                out.push(format!("     ⚠ already have their own row (left alone): {}", taken.join(", ")));
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

/// Rewrite `~/.name` on each renamed pod to its new short name — exactly the line setup
/// writes ([`arena_core::setup::name_file_command`]) — concurrently, each within
/// `PROBE_TIMEOUT`. The endpoint comes from the pre-rename listing (a metadata rename doesn't
/// move it). Never fails the rename: the pods it couldn't update come back as
/// `(name, why)` for the summary.
async fn rewrite_name_files(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    prefix: &str,
    done: &[&PlannedRename],
    pods: &[arena_core::Pod],
) -> Vec<(String, String)> {
    use arena_core::setup::{name_file_command, short_name};
    let mut missed = Vec::new();
    let mut set = tokio::task::JoinSet::new();
    for r in done {
        let target = pods.iter().find(|p| p.id == r.id).and_then(|p| SshTarget::from_pod(p, cfg).ok());
        let Some(target) = target else {
            eprintln!("[~/.name] ✗ {}: no SSH endpoint", r.new);
            missed.push((r.new.clone(), "no SSH endpoint".to_string()));
            continue;
        };
        let cmd = name_file_command(short_name(prefix, &r.new));
        let (remote, name) = (remote.clone(), r.new.clone());
        set.spawn(async move {
            let res = remote.exec(&target, &cmd, Some(PROBE_TIMEOUT)).await;
            (name, res)
        });
    }
    while let Some(joined) = set.join_next().await {
        let (name, why) = match joined {
            Ok((name, Ok(o))) if o.success => {
                println!("[~/.name] ✓ {name}");
                continue;
            }
            Ok((name, Ok(o))) => (name, format!("exit {}: {}", o.code.unwrap_or(-1), o.stderr.trim())),
            Ok((name, Err(e))) => (name, describe_error(&e)),
            Err(e) => ("?".to_string(), format!("task crashed: {e}")),
        };
        eprintln!("[~/.name] ✗ {name}: {why}");
        missed.push((name, why));
    }
    missed
}

/// Move each renamed machine's OpenRouter key mapping to its new name: its row(s) in the
/// keys CSV and its key's `name` on OpenRouter (with a provisioning key), so `copy-keys`,
/// `keys rotate|revoke` and `terminate --revoke-key` — which all go by machine name — keep
/// finding it. Quiet when there's nothing to move (no CSV or no row; no provisioning key or
/// no key under the old name). A new name that already holds its own row or key is left
/// alone and warned about — silently merging two machines' keys is worse than a manual fix —
/// and then neither side moves, so the CSV and OpenRouter stay consistent. Never fails the
/// rename: problems come back as warning lines.
async fn move_key_mappings(book: &KeyBook<'_>, done: &[&PlannedRename]) -> Vec<String> {
    use arena_core::apikeys::{rename_csv_host, CsvRename};
    let mut warnings = Vec::new();
    if done.is_empty() {
        return warnings;
    }
    let csv_text = book.read_csv();
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
    let named = |name: &str| -> Vec<&arena_core::openrouter::KeyInfo> {
        keys.iter().flatten().filter(|k| k.name.as_deref() == Some(name)).collect()
    };
    // Decide per machine first, so a conflict on either side moves neither.
    let mut csv_next = csv_text.clone();
    let (mut csv_moved, mut renames) = (Vec::new(), Vec::new());
    for r in done {
        let csv_step = csv_text.as_deref().map(|t| rename_csv_host(t, &r.old, &r.new));
        let mine = named(&r.old);
        let key_taken = !mine.is_empty() && !named(&r.new).is_empty();
        if matches!(csv_step, Some(CsvRename::Taken)) || key_taken {
            warnings.push(format!(
                "{} already has its own OpenRouter key{} — left {}'s key mapping as it was; keep one \
                 (`arena keys revoke`/`rotate`) and fix the other by hand",
                r.new,
                if key_taken { " on OpenRouter" } else { " row in the CSV" },
                r.old
            ));
            continue;
        }
        if let (Some(CsvRename::Renamed(_)), Some(text)) = (&csv_step, &csv_next) {
            if let CsvRename::Renamed(t) = rename_csv_host(text, &r.old, &r.new) {
                csv_next = Some(t);
                csv_moved.push(format!("{} → {}", r.old, r.new));
            }
        }
        renames.extend(mine.into_iter().map(|k| (k.hash.clone(), r.old.clone(), r.new.clone())));
    }
    if let (Some(text), false) = (&csv_next, csv_moved.is_empty()) {
        match book.write_csv(text) {
            Ok(()) => println!("[keys] {}: {}", book.csv.display(), csv_moved.join(", ")),
            Err(e) => warnings.push(format!(
                "couldn't update {} ({e}) — its rows still use the old names: {}",
                book.csv.display(),
                csv_moved.join(", ")
            )),
        }
    }
    if let Some(api) = book.api {
        for (hash, old, new) in renames {
            match api.rename_key(&hash, &new).await {
                Ok(()) => println!("[keys] OpenRouter key {old} → {new}"),
                Err(e) => warnings.push(format!("renaming the OpenRouter key {old} → {new} failed: {e}")),
            }
        }
    }
    warnings
}

/// The closing lines of a rename (pure, so tested): the MACHINE_NAME note, the pods whose
/// `~/.name` still has the old name (with the command that fixes them), and any key-mapping
/// warnings.
fn rename_report(missed: &[(String, String)], key_warnings: &[String]) -> Vec<String> {
    let mut out = vec![MACHINE_NAME_NOTE.to_string()];
    if !missed.is_empty() {
        let names: Vec<&str> = missed.iter().map(|(n, _)| n.as_str()).collect();
        let why: Vec<String> = missed.iter().map(|(n, w)| format!("{n}: {w}")).collect();
        out.push(format!(
            "⚠ ~/.name still has the old name on {} ({}) — re-run `arena pods setup {}` once it answers",
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
async fn handle_migrate_copy(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // Setup runs after the create below: a malformed SETUP_TIMEOUT_SECS must fail now,
    // before anything is created (and billed) — not after a ~15 min wait for the new pod,
    // stranding it. Checked even on a dry-run (so the preview surfaces it) and on a
    // re-sync that won't run setup (it's a config error either way; fix it once).
    let setup_timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None)?;
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
        // By id (a fresh listing, for the settled endpoint): the pod we just made.
        let new = just_these(owner.as_ref(), std::slice::from_ref(&created.id)).await?;
        handle_setup(remote.clone(), cfg, true, false, None, None, false, setup_timeouts, &new, KEYS_DIR)
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        created.id
    };

    println!("[3/3] syncing {canonical} → {new_name}…");
    copy_pod_files(cfg, owner.as_ref(), remote.as_ref(), &src_id, &new_id)
        .await
        .with_context(|| format!("syncing {canonical} -> {new_name}"))?;
    clean_marker(owner.as_ref(), remote.as_ref(), &new_id, cfg).await;

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
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: &dyn Remote,
    cfg: &Config,
    target: &str,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
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
    copy_pod_files(cfg, owner.as_ref(), remote, &src_id, &new_id)
        .await
        .with_context(|| format!("final sync {canonical} -> {new_name}"))?;
    clean_marker(owner.as_ref(), remote, &new_id, cfg).await;
    println!("[2/4] verifying {new_name} health…");
    verify_replacement(owner.as_ref(), remote, &new_id, cfg).await?;

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
        let name_cmd =
            format!("printf %s {} > \"$HOME/.name\"", shell_quote(&format!("export MACHINE_NAME='{short}'")));
        let _ = remote.exec(&t, &name_cmd, Some(PROBE_TIMEOUT)).await;
    }

    // 4. proxy apply + verify through the proxy; auto-revert on failure.
    if skip_proxy {
        println!("[4/4] proxy skipped — run `arena proxy apply` to route {canonical} to the new pod.");
    } else {
        println!("[4/4] re-pointing proxy and verifying {canonical} through it…");
        // Unlike the other lifecycle commands, the cutover *depends* on the proxy: a sync
        // that was skipped or failed means nobody can reach the new pod, so revert now.
        if !matches!(sync_proxy_via(cfg, owner.as_ref(), "migrate cutover").await, ProxySync::Synced { .. }) {
            eprintln!("      the proxy wasn't re-pointed — auto-reverting.");
            cutover_revert(cfg, owner.as_ref(), &canonical, &new_id, &src_id, true).await;
            anyhow::bail!("cutover reverted: proxy sync failed. {canonical} is back on the original.");
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
            if proxy_reaches_pod(remote, cfg, &canonical, &new_id).await.unwrap_or(false) {
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
    if apply_px && !matches!(sync_proxy_via(cfg, owner, "migrate revert").await, ProxySync::Synced { .. }) {
        eprintln!("  REVERT WARNING: the proxy wasn't re-pointed — run `arena proxy apply`.");
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
#[allow(clippy::too_many_arguments)]
async fn handle_replace(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    keep_old: bool,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    // Setup runs after the create (step 3/7): a malformed SETUP_TIMEOUT_SECS must fail
    // now, before anything is created (and billed) — not after the new pod has come up,
    // which would strand a `-new` pod that also blocks a re-run. Dry-runs check it too.
    let setup_timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None)?;
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
        // By id (a fresh listing each attempt, for the current endpoint): the pod we made.
        let new = just_these(owner.as_ref(), std::slice::from_ref(&created.id)).await?;
        handle_setup(remote.clone(), cfg, true, false, None, None, false, setup_timeouts, &new, KEYS_DIR)
            .await
            .with_context(|| format!("provisioning {new_name}"))?;
        println!("[4/7] copying {canonical} → {new_name} (excludes caches, HF models, .claude, .ssh, shell-rc keys)…");
        copy_pod_files(cfg, owner.as_ref(), remote.as_ref(), &src_id, &created.id)
            .await
            .with_context(|| replace_copy_failed(&canonical, &new_name))?;
        println!("[5/7] confirming the copy persists (~90s — pods can reset while still settling)…");
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        if marker_present(owner.as_ref(), remote.as_ref(), &created.id, cfg).await {
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
    clean_marker(owner.as_ref(), remote.as_ref(), &created.id, cfg).await;

    // Health check before the swap (re-resolves the endpoint fresh and retries).
    println!("      verifying health of {new_name}…");
    verify_replacement(owner.as_ref(), remote.as_ref(), &created.id, cfg)
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
        Ok(p) => match SshTarget::from_pod(&p, cfg) {
            Ok(t) => remote.exec(&t, &name_cmd, Some(PROBE_TIMEOUT)).await.map(|o| o.success).unwrap_or(false),
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
    let blocker = if skip_proxy {
        println!("[7/7] proxy skipped (--skip-proxy) — run `arena proxy apply` to repoint nginx.");
        None
    } else {
        println!("[7/7] re-pointing proxy…");
        // Fleet-wide (not just `owner`'s pods): a single-provider list here used to drop
        // every other provider's forwards, and a failed list (`unwrap_or_default`) all of them.
        let sync = sync_proxy_via(cfg, owner.as_ref(), "replace").await;
        let proxied = arena_core::proxy::ProxyConfig::from_config(cfg).is_ok()
            && cfg.machine_names.iter().any(|m| arena_core::naming::qualify(prefix, m) == canonical);
        replace_cleanup_blocker(&sync, proxied, &canonical, owner.name(), &created.id)
    };
    if keep_old {
        println!(
            "done — {canonical} is the fresh pod. {old_name} kept (--keep-old); terminate it with \
             `arena pods terminate {old_name}` once you're happy."
        );
    } else if let Some(why) = blocker {
        eprintln!(
            "{canonical} is the fresh pod, but {old_name} is NOT terminated: {why}. Its stable port \
             would point at a dead pod. Once `arena proxy apply` routes {canonical} to the new pod, \
             remove it with `arena pods terminate {old_name}`."
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
///
/// The probes and the direct copy go through `remote`, each with a budget (the copy gets
/// [`POD_COPY_TIMEOUT`]). The via-local fallback spawns `rsync` itself ([`run_rsync`]):
/// rsync runs its own ssh transport, which is neither a `Remote` exec nor a copy.
async fn copy_pod_files(
    cfg: &Config,
    provider: &dyn Provider,
    remote: &dyn Remote,
    src_id: &str,
    dest_id: &str,
) -> Result<()> {
    use arena_core::pull::{self, PullConfig};
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
    if !target_is_pod(remote, &src_target, src_id, provider).await {
        anyhow::bail!(
            "source {src_id}'s SSH endpoint doesn't resolve to that pod (its ip:port was likely \
             reassigned to a different pod) — refusing to copy from the wrong pod. Re-run."
        );
    }
    let unmark = format!("rm -f \"{marker}\"");
    remote
        .exec(&src_target, &format!("printf %s {} > \"{marker}\"", shell_quote(&token)), Some(PROBE_TIMEOUT))
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
    let direct_ok = match remote.exec(&src_target, &direct, Some(POD_COPY_TIMEOUT)).await {
        Ok(o) => o.success,
        // A copy that ran out of its budget was under way (a missing pod-to-pod key fails at
        // once), so the via-local fallback would only repeat it, slower: stop here instead.
        // Nothing has been swapped, and rsync is incremental — copying onto the SAME dest
        // again continues from here. How to do that depends on the caller (`migrate copy`
        // re-uses `-new`; `replace` refuses while it exists), so the caller says it.
        Err(e @ arena_core::Error::Timeout { .. }) => {
            let _ = remote.exec(&src_target, &unmark, Some(PROBE_TIMEOUT)).await;
            anyhow::bail!(
                "direct pod-to-pod copy {} — NOT swapping; the original is untouched. What was \
                 copied stays on the new pod: copying onto it again continues from there.",
                describe_error(&e)
            );
        }
        Err(_) => false,
    };
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
        if !target_is_pod(remote, &src_target, src_id, provider).await {
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
    let landed = marker_present(provider, remote, dest_id, cfg).await;
    // Clean the SOURCE marker now; leave the DEST marker for the caller's persistence re-check.
    let _ = remote.exec(&src_target, &unmark, Some(PROBE_TIMEOUT)).await;
    if !landed {
        anyhow::bail!(
            "copy verification failed — the data didn't reach the intended new pod ({dest_id}) \
             (a churning endpoint likely got reassigned mid-transfer). NOT swapping; the \
             original is untouched. Re-run to retry."
        );
    }
    Ok(())
}

/// Spawn `rsync` with the given argv; error (with stderr) on a non-zero exit. Deliberately
/// not a [`Remote`] call (like `pods pull`'s rsyncs): rsync drives its own ssh transport
/// (`-e`), so it is neither an exec nor a single-file copy — and it has no budget here.
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

/// Resolve a user-supplied target (machine name, bare short name, `@name` OR raw provider
/// id) by searching EVERY configured provider, returning the owning provider too — so
/// `restart`/`replace`/`migrate <name>` work whatever backend the pod lives on, without
/// passing `--provider`. The mutation must go to the owning provider's API. Matched with
/// the shared selector's single-pod rule (`arena_core::selector::resolve_one`): a typo
/// fails with the closest names, and a name two pods share is refused rather than taking
/// whichever listed first. Best-effort listing; a provider that errors is warned about and
/// skipped.
async fn resolve_target_any(
    cfg: &Config,
    target: &str,
) -> Result<(Box<dyn Provider>, String, String)> {
    let policy = arena_core::retry::RetryPolicy::default();
    let mut backends: Vec<(&str, Box<dyn Provider>)> = Vec::new();
    let mut pods: Vec<arena_core::Pod> = Vec::new();
    let mut owner_of: Vec<usize> = Vec::new(); // pods[i] belongs to backends[owner_of[i]]
    for name in ["runpod", "vast", "hetzner"] {
        let Ok(p) = arena_core::provider::build(name, cfg) else { continue };
        match arena_core::retry::retrying(&policy, || p.list_pods()).await {
            Ok(listed) => {
                owner_of.extend(std::iter::repeat(backends.len()).take(listed.len()));
                pods.extend(listed);
                backends.push((name, p));
            }
            Err(e) => eprintln!("warning: couldn't list {name} pods ({e})"),
        }
    }
    let i = arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?;
    let (name, _) = &backends[owner_of[i]];
    let label = format!("{} (id={}, {name})", pods[i].name, pods[i].id);
    let id = pods[i].id.clone();
    let (_, owner) = backends.swap_remove(owner_of[i]);
    Ok((owner, id, label))
}

/// What a fleet command does when the operator names no pods (no targets, no `--all`).
#[derive(Debug, Clone, Copy)]
enum Unscoped {
    /// The whole fleet: read-only checks, and the commands that always defaulted to it
    /// (`run`, `test`, `setup`, `copy-keys`, `cp`, `backup`, `pull`, `init-branches`).
    All,
    /// Refuse: a command that changes pods (`stop`, `reimage`, `set-branch`, `keys`) must
    /// be told which — names or `--all` (the verb goes in the message).
    Refuse(&'static str),
}

/// The pods a fleet command acts on, resolved once at the command layer, and whether the
/// operator *named* them. Named matters downstream: a named pod that can't be reached is a
/// failure to report, while one swept in by `--all` (or the default) is skipped with a note.
#[derive(Debug, Clone, Default)]
struct Selected {
    pods: Vec<arena_core::Pod>,
    named: bool,
}

impl Selected {
    /// The whole given list, as if nothing was named (what an unscoped selection of that
    /// fleet resolves to) — for tests that drive a handler directly.
    #[cfg(test)]
    fn all(pods: Vec<arena_core::Pod>) -> Selected {
        Selected { pods, named: false }
    }

    /// `name, name, …` — for prompts and dry-runs, so the operator sees what will run.
    fn names(&self) -> String {
        self.pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Every selected pod with an SSH endpoint, with its target, in selection order. A pod
    /// without one is reported (`skip <name> — no SSH endpoint yet`), never silently dropped;
    /// when the operator named pods and *none* is reachable, that's an error — not a quiet
    /// "nothing to do" that reads like success.
    fn ssh_targets(&self, cfg: &Config) -> Result<Vec<(&arena_core::Pod, SshTarget)>> {
        let mut targets = Vec::new();
        for pod in &self.pods {
            match SshTarget::from_pod(pod, cfg) {
                Ok(t) => targets.push((pod, t)),
                Err(_) => eprintln!("skip {} — no SSH endpoint yet", pod.name),
            }
        }
        if targets.is_empty() && self.named {
            anyhow::bail!("none of the selected pods ({}) has an SSH endpoint yet — check `arena pods list`", self.names());
        }
        Ok(targets)
    }

    /// [`ssh_targets`](Self::ssh_targets) keyed by pod name — what the per-pod runners take.
    fn named_targets(&self, cfg: &Config) -> Result<Vec<(String, SshTarget)>> {
        Ok(self.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), t)).collect())
    }
}

/// Resolve the operator's selection (PLAN 1.C): validate it (`arena_core::selector`: token
/// syntax, ranges against MACHINE_NAME_LIST, `--on`) *before* listing, refuse an unscoped
/// selection where the command needs one, list the fleet (retrying transient failures),
/// then resolve — a typo or a narrowed-to-nothing selection is an error before anything is
/// touched. With `--gpus`, the provider's details are fetched first (best-effort, bounded):
/// a listing may omit GPU counts, and an unknown count never matches.
async fn select(provider: &dyn Provider, cfg: &Config, sel: &SelectArgs, unscoped: Unscoped) -> Result<Selected> {
    let naming = Naming::from_config(cfg);
    let selector = Selector::parse(sel, &naming)?;
    if let (true, Unscoped::Refuse(verb)) = (selector.is_unscoped(), unscoped) {
        anyhow::bail!("name the pods to {verb} (names, ids or a range like apple..mayor), or pass --all");
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    if selector.gpus.is_some() {
        if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
            eprintln!("{warning}");
        }
    }
    let picked = selector.resolve(&naming, &pods)?;
    Ok(Selected { pods: picked.into_iter().map(|i| pods[i].clone()).collect(), named: sel.names_pods() })
}

/// A fresh listing narrowed to the pods with these `ids` — ones the program itself just
/// made (`up`, `replace`, `migrate copy`) and is about to set up. Not operator input, so no
/// typo check: a pod missing from the listing is warned about and left out (the old
/// `setup --only` behaviour); it's an error only if none is left.
async fn just_these(provider: &dyn Provider, ids: &[String]) -> Result<Selected> {
    let listed = provider.list_pods().await.context("listing pods")?;
    let pods: Vec<arena_core::Pod> = listed.into_iter().filter(|p| ids.contains(&p.id)).collect();
    let missing: Vec<&str> = ids.iter().map(String::as_str).filter(|id| !pods.iter().any(|p| p.id == *id)).collect();
    if !missing.is_empty() {
        eprintln!("warning: not in the listing (gone?): id {}", missing.join(", "));
    }
    if pods.is_empty() {
        anyhow::bail!("none of the new pods (id {}) is in the listing — check `arena pods list`", ids.join(", "));
    }
    Ok(Selected { pods, named: true })
}

/// `pods init-branches`: create each pod's `autocommit-…-wNdM-…` branch and push it
/// upstream, without committing — so a new day's branch exists before backups run.
#[allow(clippy::too_many_arguments)]
async fn handle_init_branches(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    week: Option<u32>,
    day: Option<u32>,
    // The pods to init, resolved by the caller.
    sel: &Selected,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let (week, day) = resolve_week_day(cfg, week, day)?;
    let bcfg = arena_core::backup::BackupConfig::from_config(cfg, week, day);
    println!("Iteration: w{week}d{day}\n");

    let targets = sel.named_targets(cfg)?;
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
        "Will create + push the w{week}d{day} autocommit branch on {} pod(s): {}",
        targets.len(),
        targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
    ))? {
        println!("aborted.");
        return Ok(());
    }

    let total = targets.len();
    println!("Initializing branches on {total} pod(s) over SSH ({}s budget each)…", BRANCH_TIMEOUT.as_secs());
    let jobs = targets
        .into_iter()
        .map(|(name, target)| {
            let cmd = arena_core::backup::init_branch_command(&bcfg, &name);
            (name, target, cmd)
        })
        .collect();
    let (mut ok, mut failed) = (0, 0);
    exec_each_pod(&remote, jobs, BRANCH_TIMEOUT, |done, total, name, call| match &call {
        Ok(out) if out.success => {
            println!("[{done}/{total}] ✓ {name} -> {}", bcfg.branch_for(name));
            ok += 1;
        }
        _ => {
            println!("{}", failure_line(done, total, name, &call));
            failed += 1;
        }
    })
    .await;
    println!("\nDone: {ok} initialized, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to init branch");
    }
    Ok(())
}

/// `pods pull`: rsync each pod's home directory to `<dir>/<label>/<pod-name>/`. The file
/// backup (legacy `backup.sh`), complementing the git autocommit `backup`. It spawns
/// `rsync` itself rather than going through a [`Remote`]: rsync drives its own ssh
/// transport (`-e`), which is neither an exec nor a single-file copy (and has no budget).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
async fn handle_pull(
    cfg: &Config,
    label: Option<String>,
    dir: &str,
    max_size: Option<String>,
    remote_path: Option<String>,
    no_git: bool,
    no_big: bool,
    // The pods to pull, resolved by the caller.
    sel: &Selected,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::pull::{self, PullConfig};

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

    let targets = sel.named_targets(cfg)?;
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
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will rsync {} pod home(s) into {dest_msg}: {names}", targets.len()))? {
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

/// `pods copy`: scp a local file to every selected pod, creating the remote parent
/// dir first. Destination per [`resolve_remote_dest`]. Pods run concurrently, each step
/// bounded (see [`copy_to_pod`]); the scp gets `timeout` (`--timeout`), else a budget
/// proportional to what's sent ([`cp_timeout`]).
#[allow(clippy::too_many_arguments)]
async fn handle_copy(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote_ssh: Arc<dyn Remote>,
    cfg: &Config,
    file: &std::path::Path,
    dest: Option<&str>,
    recursive: bool,
    // The pods to copy to, resolved by the caller.
    sel: &Selected,
    timeout: Option<Duration>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if recursive {
        if !file.exists() {
            anyhow::bail!("not found: {}", file.display());
        }
    } else if !file.is_file() {
        anyhow::bail!("not a file: {} (pass -r to copy a directory)", file.display());
    }
    let local = file.to_string_lossy().into_owned();

    // Resolve the remote destination (same for every pod).
    let repo_name = cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials");
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

    let targets = sel.named_targets(cfg)?;
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to copy to)");
        return Ok(());
    }

    let rflag = if recursive { "-r " } else { "" };
    let budget = timeout.unwrap_or_else(|| cp_timeout(local_size(file), targets.len()));
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
    println!(
        "  budget: {}s per pod{}",
        budget.as_secs(),
        if timeout.is_some() { " (--timeout)" } else { " (scales with size × pods; --timeout to change)" }
    );
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
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will copy {local} to {} on {} pod(s): {names}", remote, targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    // For a single file we verify it actually landed afterward (scp can exit 0 without
    // writing what you intended — e.g. the dest already exists as a directory, so the
    // file lands *inside* it). Skipped for -r (the dest is a tree, not one file).
    let plan = Arc::new(CopyPlan {
        expect_size: if recursive { None } else { std::fs::metadata(file).ok().map(|m| m.len()) },
        basename: file.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string(),
        local,
        remote,
        remote_parent,
        recursive,
        timeout: budget,
    });
    let jobs = targets
        .into_iter()
        .map(|(name, t)| {
            let (remote_ssh, plan) = (remote_ssh.clone(), plan.clone());
            (name, async move { copy_to_pod(remote_ssh.as_ref(), &t, &plan).await })
        })
        .collect();
    let (mut ok, mut failed) = (0, 0);
    each_pod(jobs, |done, total, name, result| match result.and_then(|copied| copied) {
        Ok(()) => {
            println!("[{done}/{total}] ✓ {name}");
            ok += 1;
        }
        Err(e) => {
            println!("[{done}/{total}] ✗ {name}: {e}");
            failed += 1;
        }
    })
    .await;
    println!("\nDone: {ok} copied, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to receive the file");
    }
    Ok(())
}

/// What `pods cp` copies where, and how it checks the copy landed — the same on every pod.
struct CopyPlan {
    local: String,
    remote: String,
    /// The remote dir to `mkdir -p` first.
    remote_parent: String,
    /// `-r`: copy a tree (`scp -r`); no size check.
    recursive: bool,
    /// A single file's local size, verified on the pod after the copy (`None` with `-r`).
    expect_size: Option<u64>,
    basename: String,
    /// The scp's budget (see [`cp_timeout`]).
    timeout: Duration,
}

/// One pod's `pods cp`: `mkdir -p` the parent → scp (`-r` for a tree) → for a single file,
/// verify it landed at the expected size. Stops at the first failure: no copy into a
/// parent that couldn't be made, no check of a copy that failed. The mkdir and the check
/// are quick probes (`PROBE_TIMEOUT`); the scp gets the plan's `timeout`.
async fn copy_to_pod(remote: &dyn Remote, t: &SshTarget, plan: &CopyPlan) -> std::result::Result<(), String> {
    let mkdir = format!("mkdir -p {}", shell_quote(&plan.remote_parent));
    match remote.exec(t, &mkdir, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {}
        Ok(o) => return Err(format!("mkdir failed: {}", o.stderr.trim())),
        Err(e) => return Err(describe_error(&e)),
    }
    let copied = if plan.recursive {
        remote.copy_recursive(t, &plan.local, &plan.remote, Some(plan.timeout)).await
    } else {
        remote.copy(t, &plan.local, &plan.remote, Some(plan.timeout)).await
    };
    match copied {
        Ok(o) if o.success => {}
        Ok(o) => return Err(o.stderr.trim().to_string()),
        Err(e) => return Err(describe_error(&e)),
    }
    // Verify the single-file copy actually landed at the expected size. A `remote` ending
    // in `/` means "into this dir" (intended → check <remote><basename>); otherwise
    // `remote` should BE the file, and finding a directory there is a silent misplacement
    // (scp dropped the file inside it) we flag rather than pass.
    let Some(expected) = plan.expect_size else { return Ok(()) };
    let remote_path = &plan.remote;
    let check = if remote_path.ends_with('/') {
        let final_path = format!("{remote_path}{}", plan.basename);
        format!(
            "f={f}; [ -f \"$f\" ] && echo \"OK $(wc -c < \"$f\" | tr -d ' ')\" || echo MISSING",
            f = shell_quote(&final_path),
        )
    } else {
        format!(
            "f={r}; if [ -d \"$f\" ]; then echo MISPLACED; elif [ -f \"$f\" ]; then echo \"OK $(wc -c < \"$f\" | tr -d ' ')\"; else echo MISSING; fi",
            r = shell_quote(remote_path),
        )
    };
    match remote.exec(t, &check, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {
            let line = o.stdout.trim();
            if let Some(n) = line.strip_prefix("OK ") {
                match n.trim().parse::<u64>() {
                    Ok(sz) if sz == expected => Ok(()),
                    Ok(sz) => Err(format!("size mismatch after copy: {sz}B on pod vs {expected}B local (partial / clobbered)")),
                    Err(_) => Ok(()), // couldn't parse size; don't false-fail
                }
            } else if line == "MISPLACED" {
                Err(format!("{remote_path} is a directory on the pod — the file landed *inside* it; pass an explicit file DEST or remove that dir"))
            } else {
                Err(format!("nothing at {remote_path} after scp (silent non-write)"))
            }
        }
        // If the verify probe itself can't run (or times out), don't override a successful scp.
        _ => Ok(()),
    }
}

/// `pods copy-keys`: distribute API keys to each pod's shell. Per-host keys come from
/// `<keys_dir>/<provider>_api_keys.csv`; a Hugging Face token (from `--hf-token` or
/// config `HF_TOKEN`) is broadcast to every pod (for gated repos like Llama 3).
#[allow(clippy::too_many_arguments)]
async fn handle_copy_keys(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests. Each pod's write is
    // bounded by COPY_KEYS_TIMEOUT, so one wedged pod can't hang the command.
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    cfg: &Config,
    keys_dir: &str,
    hf_token: Option<String>,
    cc_token: Option<String>,
    // The pods to write to, resolved by the caller (the operator's selection, or the pods
    // `setup` / `keys --copy` just handled).
    sel: &Selected,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::apikeys;
    use arena_core::ssh::SshTarget;
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
    // "broadcast" = same value on every *selected* pod (vs per-host CSV keys) — not "all
    // pods", which misleadingly implies the whole fleet when the selection is narrower.
    for (key, display, _) in apikeys::BROADCAST_TOKENS {
        if token_value(key).is_some() {
            sources.push(format!("{display} (broadcast to every selected pod)"));
        }
    }

    if per_host.is_empty() && broadcast.is_empty() {
        anyhow::bail!(
            "no keys to copy: put `<provider>_api_keys.csv` in {keys_dir}/ \
             (openai/anthropic/openrouter), set HF_TOKEN / CLAUDE_CODE_OAUTH_TOKEN, or pass --hf-token"
        );
    }
    println!("Key sources: {}\n", sources.join(", "));

    // Build the per-pod var set (broadcast HF merged into every reachable selected pod).
    let pods = &sel.pods;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let mut jobs: Vec<(String, SshTarget, Vec<(String, String)>)> = Vec::new();
    // Reachable pods that matched NO per-host key — they'd get broadcast tokens only.
    // Worth flagging loudly: a stale/misnamed CSV (e.g. last cohort's hosts) otherwise
    // hides behind a per-pod ✓, so nobody notices the per-host keys never landed.
    let mut broadcast_only: Vec<String> = Vec::new();
    let mut unreachable = 0;
    for pod in pods {
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
            Err(_) => {
                eprintln!("skip {} — no SSH endpoint yet", pod.name);
                unreachable += 1;
            }
        }
    }
    if jobs.is_empty() {
        // Pods the operator named, all without an endpoint: a failure, not "nothing to do".
        if sel.named && unreachable > 0 {
            anyhow::bail!("none of the selected pods ({}) has an SSH endpoint yet — check `arena pods list`", sel.names());
        }
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
        _ => arena_core::sshconfig::render_manual(prefix, ssh_user, pod_identity, pods),
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
    let names = jobs.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will export API keys into ~/.bashrc & ~/.zshrc, authorize the fleet keys, and write ~/.ssh/config on {} pod(s): {names}", jobs.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    for (name, t, vars) in jobs {
        // Tokens (export lines) + fleet SSH (authorized_keys + ~/.ssh/config) in one round-trip.
        let cmd = format!("{}; {}", apikeys::remote_export_command(&vars), fleet_ssh);
        let remote = remote.clone();
        set.spawn(async move { (name, remote.exec(&t, &cmd, Some(COPY_KEYS_TIMEOUT)).await) });
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

/// Resolve which machines a `keys` action targets, with the shared selector. A key belongs
/// to a *machine name*, which can exist before its pod (mint keys, then `up`) or after it
/// (revoke a terminated pod's key) — so names and ranges resolve against the current pods
/// **plus** every MACHINE_NAME_LIST name without a pod (a name in neither is still a typo
/// error). `--all` keeps its meaning: every *current* pod. Returns the full machine names,
/// and the selection of those that are pods (what `--copy` can reach).
async fn keys_targets(provider: &dyn Provider, cfg: &Config, sel: &SelectArgs) -> Result<(Vec<String>, Selected)> {
    let naming = Naming::from_config(cfg);
    let selector = Selector::parse(sel, &naming)?;
    if selector.is_unscoped() {
        anyhow::bail!("name the machines (names or a range like apple..mayor), or pass --all");
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let mut universe = pods.clone();
    if !selector.all {
        // Spare list names, as pod-less stand-ins (no id, no provider: `--on`/`--gpus` skip them).
        for entry in &cfg.machine_names {
            let name = arena_core::naming::qualify(naming.prefix, entry);
            if !pods.iter().any(|p| p.name == name) {
                universe.push(arena_core::Pod { name, ..Default::default() });
            }
        }
    }
    let picked = selector.resolve(&naming, &universe)?;
    let names = picked.iter().map(|&i| universe[i].name.clone()).collect();
    let live = picked.into_iter().filter(|&i| i < pods.len()).map(|i| pods[i].clone()).collect();
    Ok((names, Selected { pods: live, named: true }))
}

/// `keys gen|rotate --copy`: push the fresh keys to the targeted machines that are pods
/// (a name without a pod has nowhere to go yet — `setup`/`copy-keys` deliver it later).
async fn copy_new_keys(remote: Arc<dyn Remote>, cfg: &Config, live: &Selected, yes: bool) -> Result<()> {
    if live.pods.is_empty() {
        println!("(--copy: none of these machines has a pod yet — run `arena pods copy-keys` once they're up)");
        return Ok(());
    }
    handle_copy_keys(remote, cfg, KEYS_DIR, None, None, live, false, yes).await
}

/// Persist one machine's freshly minted OpenRouter key into the per-host CSV (upsert).
/// A new file is seeded with a header naming the arena iteration (`prefix`) so the CSV
/// is self-documenting about which cohort the keys belong to.
fn write_openrouter_key(host: &str, secret: &str, prefix: &str) -> Result<()> {
    std::fs::create_dir_all("./keys").context("creating ./keys")?;
    let target = ensure_cohort_keys_file(prefix)?;
    let mut existing = std::fs::read_to_string(&target).unwrap_or_default();
    if existing.trim().is_empty() {
        existing = format!(
            "# OpenRouter API keys — arena iteration: {prefix}\n# host,key (one runtime key per machine; managed by `arena keys`)\n"
        );
    }
    let updated = arena_core::apikeys::upsert_csv(&existing, host, secret);
    std::fs::write(&target, updated).with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}

/// Keys live per cohort in `keys/<prefix>_openrouter_keys.csv`; the canonical
/// `keys/openrouter_api_keys.csv` (what copy-keys reads) is a symlink to the current one.
/// Repoints the symlink when the prefix changes, so a new cohort's keys never get written
/// into the previous cohort's file. A legacy regular file is moved aside to `.bak`.
fn ensure_cohort_keys_file(prefix: &str) -> Result<PathBuf> {
    let file = format!("{prefix}_openrouter_keys.csv");
    let target = PathBuf::from("./keys").join(&file);
    let link = std::path::Path::new(OPENROUTER_KEYS_CSV);
    match std::fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(link)?.file_name() != Some(std::ffi::OsStr::new(&file)) {
                std::fs::remove_file(link)?;
                std::os::unix::fs::symlink(&file, link)?;
                println!("(keys) {OPENROUTER_KEYS_CSV} now → {file}");
            }
        }
        Ok(_) => {
            let bak = format!("{OPENROUTER_KEYS_CSV}.pre-{prefix}.bak");
            anyhow::ensure!(!std::path::Path::new(&bak).exists(), "{bak} already exists — sort out {OPENROUTER_KEYS_CSV} by hand");
            std::fs::rename(link, &bak)?;
            println!("(keys) moved old {OPENROUTER_KEYS_CSV} aside to {bak}");
            std::os::unix::fs::symlink(&file, link)?;
        }
        Err(_) => std::os::unix::fs::symlink(&file, link)?,
    }
    Ok(target)
}

/// `arena keys`: manage OpenRouter runtime keys (generate / list / rotate / revoke) via
/// the provisioning API. Secrets are saved to `keys/openrouter_api_keys.csv` (which
/// `pods copy-keys` then distributes); rotation/revocation find a key by its name
/// (`<prefix>-<machine>`), so no local hash bookkeeping is needed.
async fn handle_keys(
    cmd: KeysCmd,
    provider: &dyn Provider,
    // How `--copy` reaches pods (via `copy-keys`): `SshRemote` for real.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
) -> Result<()> {
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

        KeysCmd::Gen { sel, limit, copy, dry_run } => {
            let (names, live) = keys_targets(provider, cfg, &sel.args()).await?;
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
                "Will mint {} OpenRouter key(s) (cap ${limit:.2} each) and write {OPENROUTER_KEYS_CSV}: {}",
                names.len(),
                names.join(", ")
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
                copy_new_keys(remote.clone(), cfg, &live, yes).await?;
            }
        }

        KeysCmd::Rotate { sel, limit, copy, dry_run } => {
            let (names, live) = keys_targets(provider, cfg, &sel.args()).await?;
            let limit = limit.unwrap_or(default_limit);
            if dry_run {
                println!("Dry-run — would delete + re-mint a key for {} machine(s):", names.len());
                for h in &names {
                    println!("  {}", key_name(&prefix, &cfg.machine_names, h));
                }
                return Ok(());
            }
            if !confirm(yes, &format!("Will DELETE + re-mint {} OpenRouter key(s) (cap ${limit:.2}): {}", names.len(), names.join(", ")))? {
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
                copy_new_keys(remote.clone(), cfg, &live, yes).await?;
            }
        }

        KeysCmd::Revoke { sel, dry_run } => {
            let (names, _) = keys_targets(provider, cfg, &sel.args()).await?;
            if dry_run {
                println!("Dry-run — would revoke the key for {} machine(s):", names.len());
                for h in &names {
                    println!("  {}", key_name(&prefix, &cfg.machine_names, h));
                }
                return Ok(());
            }
            if !confirm(yes, &format!("Will DELETE {} OpenRouter key(s) — no regenerate: {}", names.len(), names.join(", ")))? {
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

/// Scenario tests for target selection (PLAN 1.C) at the command layer, driven by a fake
/// `Provider` that records every mutation (no real API/SSH): the shared selector picks
/// exactly the pods each command family acts on, typos fail before anything is touched,
/// commands that change pods demand names or `--all`, and every old spelling still parses.
#[cfg(test)]
mod selection_tests {
    use super::{handle_keys, handle_pods, keys_targets, select, Cli, Cmd, KeysCmd, PodCmd, Unscoped};
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
            (&["reimage", "bloom", "zebra", "--skip-proxy"], &["bloom", "zebra"]),
            (&["reimage", "--all", "--exclude", "apple..cloud", "--skip-proxy"], &["vm", "zebra"]),
            (&["reimage", "apple..bloom", "--dry-run"], &[]),
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
        let keys = |a: SelectArgs| {
            let f = &f;
            async move { keys_targets(f, &cfg(), &a).await.map(|(n, live)| (n, live.pods.len())).map_err(|e| e.to_string()) }
        };
        // A range spans list names without a pod (mayor): a key can precede its pod.
        assert_eq!(
            keys(args(&["cloud..vm"])).await.unwrap(),
            (vec!["arena8-cloud".to_string(), "arena8-mayor".into(), "arena8-vm".into()], 2)
        );
        // A spare list name alone (revoke a terminated pod's key) is fine; a typo is not.
        assert_eq!(keys(args(&["mayor"])).await.unwrap(), (vec!["arena8-mayor".to_string()], 0));
        assert!(keys(args(&["mayr"])).await.unwrap_err().contains("did you mean arena8-mayor?"));
        // --all = every current pod (not the spare names), narrowed as usual.
        let all = SelectArgs { all: true, exclude: vec!["vm".into()], ..Default::default() };
        assert_eq!(keys(all).await.unwrap().0, ["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-zebra"]);
        assert!(keys(SelectArgs::default()).await.unwrap_err().contains("name the machines"));
        // Through the command (a dry run never reaches OpenRouter).
        let cfg = Config::parse("MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(apple bloom)\nOPENROUTER_PROVISIONING_KEY=test\n");
        for argv in [&["keys", "gen", "apple..bloom", "--dry-run"][..], &["keys", "revoke", "--all", "--dry-run"]] {
            let Cmd::Keys(k) = parse(argv).unwrap() else { unreachable!() };
            handle_keys(k, &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        let Cmd::Keys(k) = parse(&["keys", "rotate", "aple", "--dry-run"]).unwrap() else { unreachable!() };
        assert!(handle_keys(k, &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap_err().to_string().contains("did you mean"));
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

/// `pods list` enrichment must degrade, not fail: a provider error or a hang yields one
/// warning and the pods (as listed) still render.
#[cfg(test)]
mod list_tests {
    use super::enrich_best_effort;
    use arena_core::{Error, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use std::time::Duration;

    enum Mode {
        Fill,
        Fail,
        Hang,
    }

    struct EnrichFake(Mode);

    #[async_trait]
    impl Provider for EnrichFake {
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
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            match self.0 {
                Mode::Fill => {
                    pods.iter_mut().for_each(|p| p.cost_per_hr = Some(0.17));
                    Ok(())
                }
                Mode::Fail => Err(Error::provider("pod details HTTP 429 Too Many Requests")),
                Mode::Hang => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(())
                }
            }
        }
    }

    fn pods() -> Vec<Pod> {
        vec![Pod { id: "a".into(), name: "devtest-apple".into(), provider: "runpod".into(), ..Default::default() }]
    }

    #[tokio::test]
    async fn success_fills_and_stays_quiet() {
        let mut p = pods();
        assert_eq!(enrich_best_effort(&EnrichFake(Mode::Fill), &mut p, Duration::from_secs(5)).await, None);
        assert_eq!(p[0].cost_per_hr, Some(0.17));
    }

    #[tokio::test]
    async fn failure_is_one_warning_and_pods_survive() {
        let mut p = pods();
        let w = enrich_best_effort(&EnrichFake(Mode::Fail), &mut p, Duration::from_secs(5)).await.unwrap();
        assert!(w.starts_with("warning: pod details incomplete") && w.contains("429"), "{w}");
        assert!(!w.contains('\n'), "one line: {w}");
        assert_eq!(p, pods()); // listing untouched, still renders
    }

    #[tokio::test]
    async fn hang_times_out_instead_of_blocking_the_list() {
        let mut p = pods();
        let w = enrich_best_effort(&EnrichFake(Mode::Hang), &mut p, Duration::from_millis(50)).await.unwrap();
        assert!(w.contains("timed out after 50ms"), "{w}");
        assert_eq!(p, pods());
    }
}

/// `pods setup` over a scripted `FakeRemote` on a paused clock: one wedged pod must time
/// out at its step while the rest finish, and the whole run must end at that pod's
/// budget — not at the hang.
#[cfg(test)]
mod setup_tests {
    use super::{handle_copy_keys, handle_pods, handle_setup, provision_fleet, Cli, Cmd, Selected, SetupJob, COPY_KEYS_TIMEOUT};
    use arena_core::remote::{FakeRemote, FakeReply, Remote, RemoteCall};
    use arena_core::setup::{provisioning_steps, BootRetry, SetupConfig, SetupTimeouts};
    use arena_core::ssh::{SshOutput, SshTarget};
    use arena_core::{Config, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::Instant;

    fn target(port: u16) -> SshTarget {
        SshTarget { user: "root".into(), host: "10.0.0.1".into(), port, key_paths: vec![], connect_timeout_secs: 10 }
    }

    fn scfg() -> SetupConfig {
        SetupConfig {
            key_local: "/local/key".into(),
            key_remote: "/root/.ssh/id_ed25519".into(),
            repo_path: "/root/ARENA_materials".into(),
            repo_url: "git@github.com:o/r.git".into(),
            branch: "main".into(),
            prefix: "devtest".into(),
            authorized_pubkeys: vec![],
            broadcast_exports: vec![],
            zsh_install: false,
        }
    }

    fn job(name: &str, port: u16) -> SetupJob {
        SetupJob {
            name: name.into(),
            target: target(port),
            steps: provisioning_steps("runpod", &scfg(), name, false, "", &SetupTimeouts::default()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_hung_pod_times_out_while_the_others_finish() {
        // (a) bloom's config step hangs "forever"; apple and cloud are healthy.
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang()]);
        let jobs = vec![job("devtest-apple", 22001), job("devtest-bloom", 22002), job("devtest-cloud", 22003)];
        let mut lines = Vec::new();
        let start = Instant::now();
        let (mut provisioned, failed) =
            provision_fleet(fake.clone(), jobs, BootRetry::default(), |l| lines.push(l.to_string())).await;

        provisioned.sort();
        assert_eq!((provisioned, failed), (vec!["devtest-apple".to_string(), "devtest-cloud".to_string()], 1));
        assert_eq!(start.elapsed(), Duration::from_secs(300), "ends at the stuck step's budget, not the hang");
        // Healthy pods are reported first (they never waited on bloom), in either order.
        assert!(lines[0].starts_with("[1/3] ✓ ") && lines[1].starts_with("[2/3] ✓ "), "{lines:?}");
        let mut healthy: Vec<&str> = lines[..2].iter().filter_map(|l| l.split_once(" ✓ ").map(|(_, n)| n)).collect();
        healthy.sort();
        assert_eq!(healthy, ["devtest-apple", "devtest-cloud"], "{lines:?}");
        assert_eq!(lines[2], "[3/3] ✗ devtest-bloom (timed out at repo + keys config after 300s)");
        // Every pod got both steps; nothing was retried.
        for port in [22001, 22002, 22003] {
            assert_eq!(fake.calls_to(&format!("10.0.0.1:{port}")).len(), 2, "port {port}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_counted_and_described() {
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::exit(1, "scp: /root/.ssh: Permission denied")]);
        let mut lines = Vec::new();
        let (provisioned, failed) =
            provision_fleet(fake, vec![job("devtest-apple", 22001)], BootRetry::default(), |l| lines.push(l.to_string())).await;
        assert_eq!((provisioned.len(), failed), (0, 1));
        assert_eq!(lines, ["[1/1] ✗ devtest-apple (failed at copy deploy key, exit 1): scp: /root/.ssh: Permission denied"]);
    }

    struct Fleet(Vec<Pod>);

    #[async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(self.0.clone())
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

    fn fleet() -> Fleet {
        let pod = |name: &str, port: u16| Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(port),
            ..Default::default()
        };
        Fleet(vec![pod("devtest-apple", 22001), pod("devtest-bloom", 22002), pod("devtest-cloud", 22003)])
    }

    /// The whole test fleet, nothing named.
    fn everyone() -> Selected {
        Selected::all(fleet().0)
    }

    fn setup_cfg(extra: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n\
             GIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n{extra}"
        ))
    }

    /// A Remote whose calls panic — to check a crashed setup task is still reported.
    struct PanickyRemote;

    #[async_trait]
    impl Remote for PanickyRemote {
        async fn exec(&self, _: &SshTarget, _: &str, _: Option<Duration>) -> Result<SshOutput> {
            panic!("boom")
        }
        async fn copy(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<SshOutput> {
            panic!("boom")
        }
        async fn copy_recursive(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<SshOutput> {
            panic!("boom")
        }
    }

    #[tokio::test]
    async fn a_crashed_setup_task_is_a_named_failure() {
        let mut lines = Vec::new();
        let (provisioned, failed) = provision_fleet(
            Arc::new(PanickyRemote),
            vec![job("devtest-apple", 22001)],
            BootRetry::default(),
            |l| lines.push(l.to_string()),
        )
        .await;
        assert_eq!((provisioned.len(), failed), (0, 1));
        assert!(lines[0].starts_with("[1/1] ✗ devtest-apple (its setup task crashed: "), "{lines:?}");
    }

    /// The resolved budgets for a test config (`--timeout` = `flag`).
    fn budgets(cfg: &Config, flag: Option<u64>) -> SetupTimeouts {
        SetupTimeouts::from_config(cfg, flag).unwrap()
    }

    /// No API-key CSVs: setup skips key distribution.
    const NO_KEYS: &str = "/nonexistent/arena-test-keys";

    #[tokio::test(start_paused = true)]
    async fn handle_setup_runs_over_the_given_remote_with_configured_budget() {
        // End to end through the command: SETUP_TIMEOUT_SECS sets the config-step budget,
        // the hung pod fails the command (non-zero exit) after that budget, the others
        // were fully provisioned with the narrow fetch.
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang()]);
        let start = Instant::now();
        let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
        let err = handle_setup(fake.clone(), &cfg, true, false, None, None, false, budgets(&cfg, None), &everyone(), NO_KEYS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1 pod(s) failed to set up"), "{err}");
        assert_eq!(start.elapsed(), Duration::from_secs(120));
        for port in [22001, 22002, 22003] {
            let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
            assert!(
                matches!(&calls[..], [RemoteCall::Copy { local, .. }, RemoteCall::Exec { cmd, timeout, .. }]
                    if local.ends_with("devtest_deploy_key")
                        && cmd.contains("git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'")
                        && *timeout == Some(Duration::from_secs(120))),
                "port {port}: {calls:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handle_setup_timeout_flag_wins_and_names_scope_the_run() {
        let fake = Arc::new(FakeRemote::new());
        let only = Selected { pods: vec![fleet().0.remove(0)], named: true };
        let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
        handle_setup(fake.clone(), &cfg, true, false, None, None, false, budgets(&cfg, Some(45)), &only, NO_KEYS)
            .await
            .unwrap();
        let calls = fake.calls();
        assert!(calls.iter().all(|c| c.host() == "10.0.0.1:22001"), "only the named pod: {calls:?}");
        assert!(matches!(calls.last(), Some(RemoteCall::Exec { timeout, .. }) if *timeout == Some(Duration::from_secs(45))));
    }

    /// A temp keys dir holding an OpenAI per-host CSV for the whole test fleet.
    fn keys_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arena-setup-keys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("openai_api_keys.csv"),
            "host,key\ndevtest-apple,sk-a\ndevtest-bloom,sk-b\ndevtest-cloud,sk-c\n",
        )
        .unwrap();
        d
    }

    #[tokio::test(start_paused = true)]
    async fn keys_go_only_to_provisioned_pods_so_a_wedged_pod_cannot_hang_setup() {
        // Review finding: once `keys gen` has run, a whole-fleet setup hands off to
        // copy-keys — which used to target every reachable pod (incl. the one that just
        // timed out) over an unbounded ssh, hanging the command forever. Now: bloom
        // (wedged at its config step) is never contacted again, the others get their keys,
        // and the command ends at bloom's step budget.
        let dir = keys_dir("wedged");
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang(), FakeReply::hang()]);
        let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
        let start = Instant::now();
        let keys = dir.to_string_lossy();
        let err = handle_setup(fake.clone(), &cfg, true, false, None, None, false, budgets(&cfg, None), &everyone(), &keys)
            .await
            .unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.to_string().contains("1 pod(s) failed to set up"), "{err}");
        assert_eq!(start.elapsed(), Duration::from_secs(120), "ends at bloom's budget, not the hang");
        assert_eq!(fake.calls_to("10.0.0.1:22002").len(), 2, "the timed-out pod is not re-contacted for keys");
        for port in [22001, 22003] {
            let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
            assert_eq!(calls.len(), 3, "copy, config, keys: {calls:?}");
            assert!(
                matches!(&calls[2], RemoteCall::Exec { cmd, timeout, .. }
                    if cmd.contains("OPENAI_API_KEY") && *timeout == Some(COPY_KEYS_TIMEOUT)),
                "{calls:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pod_wedging_during_key_distribution_times_out() {
        // Every pod provisions, then apple hangs on the key write: copy-keys reports it
        // after COPY_KEYS_TIMEOUT (best-effort: setup itself still succeeds).
        let dir = keys_dir("keyhang");
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
        let cfg = setup_cfg("");
        let start = Instant::now();
        let keys = dir.to_string_lossy().into_owned();
        handle_setup(fake.clone(), &cfg, true, false, None, None, false, budgets(&cfg, None), &everyone(), &keys)
            .await
            .unwrap();
        assert_eq!(start.elapsed(), COPY_KEYS_TIMEOUT);
        for port in [22001, 22002, 22003] {
            assert_eq!(fake.calls_to(&format!("10.0.0.1:{port}")).len(), 3, "port {port}");
        }

        // copy-keys on its own (`pods copy-keys`) is bounded the same way.
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22003", [FakeReply::hang()]);
        let start = Instant::now();
        let err = handle_copy_keys(fake.clone(), &cfg, &keys, None, None, &everyone(), false, true)
            .await
            .unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.to_string().contains("1 pod(s) failed"), "{err}");
        assert_eq!(start.elapsed(), COPY_KEYS_TIMEOUT);
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test]
    async fn create_then_setup_commands_reject_a_bad_budget_before_doing_anything() {
        // Review finding: `replace` / `migrate copy` create (and bill) a pod, wait up to
        // ~15 min for it, and only then ran setup — which is where a malformed
        // SETUP_TIMEOUT_SECS used to be caught, stranding the new pod. Now they fail first.
        // (The config has no provider keys, so getting past the check would fail
        // differently — and never reach a provider.)
        let cfg = setup_cfg("SETUP_TIMEOUT_SECS=5m");
        let ov = super::SpecOverrides::default();
        for dry_run in [true, false] {
            let fake = Arc::new(FakeRemote::new());
            let err =
                super::handle_replace(fake.clone(), &cfg, "apple", &ov, false, true, dry_run, true).await.unwrap_err();
            assert!(err.to_string().contains("SETUP_TIMEOUT_SECS"), "replace (dry_run={dry_run}): {err}");
            let err = super::handle_migrate_copy(fake.clone(), &cfg, "apple", &ov, dry_run, true).await.unwrap_err();
            assert!(fake.calls().is_empty(), "nothing reached a pod");
            assert!(err.to_string().contains("SETUP_TIMEOUT_SECS"), "migrate copy (dry_run={dry_run}): {err}");
        }
    }

    /// `pods <args…>` parsed as the CLI would.
    fn pods(args: &[&str]) -> super::PodCmd {
        use clap::Parser;
        let argv = ["arena", "pods"].iter().chain(args).copied();
        match Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd {
            Cmd::Pods(p) => p,
            _ => unreachable!(),
        }
    }

    /// The hosts (`ip:port`) that got any call, sorted.
    fn hosts(fake: &FakeRemote) -> Vec<String> {
        let mut h: Vec<String> = fake.calls().iter().map(|c| c.host().to_string()).collect();
        h.sort();
        h.dedup();
        h
    }

    #[tokio::test(start_paused = true)]
    async fn setup_and_copy_keys_run_on_exactly_the_selected_pods() {
        // PLAN 1.C through the commands: the shared selector decides who is provisioned /
        // gets keys — names, ranges, --exclude — and a typo touches nobody.
        let dir = keys_dir("select");
        let keys = dir.to_string_lossy().into_owned();
        let cfg = setup_cfg("MACHINE_NAME_LIST=(apple bloom cloud)\n");
        let cases: &[(&[&str], &[u16])] = &[
            (&["setup", "apple", "cloud"], &[22001, 22003]),
            (&["setup", "bloom..cloud", "--exclude", "cloud"], &[22002]),
            (&["setup", "--include", "apple"], &[22001]), // the old spelling
        ];
        for (args, ports) in cases {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(args), &fleet(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let want: Vec<String> = ports.iter().map(|p| format!("10.0.0.1:{p}")).collect();
            assert_eq!(hosts(&fake), want, "{args:?}");
        }
        for (args, ports) in [
            (vec!["copy-keys", "--keys-dir", keys.as_str(), "--all", "--exclude", "bloom"], [22001, 22003]),
            (vec!["copy-keys", "--keys-dir", keys.as_str(), "apple", "--include", "cloud"], [22001, 22003]),
        ] {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&args), &fleet(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let want: Vec<String> = ports.iter().map(|p| format!("10.0.0.1:{p}")).collect();
            assert_eq!(hosts(&fake), want, "{args:?}");
        }
        for args in [&["setup", "apple", "clod"][..], &["copy-keys", "--exclude", "bloon"]] {
            let fake = Arc::new(FakeRemote::new());
            let err = handle_pods(pods(args), &fleet(), fake.clone(), &cfg, true).await.unwrap_err().to_string();
            assert!(err.contains("matched no pod") && err.contains("did you mean devtest-"), "{args:?}: {err}");
            assert!(fake.calls().is_empty(), "{args:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_takes_a_positive_timeout() {
        use super::{Cli, Cmd, PodCmd};
        use clap::Parser;
        let parsed = Cli::try_parse_from(["arena", "pods", "setup", "apple", "--timeout", "900"]).unwrap().cmd;
        assert!(matches!(parsed, Cmd::Pods(PodCmd::Setup { timeout: Some(900), .. })));
        let parsed = Cli::try_parse_from(["arena", "pods", "setup"]).unwrap().cmd;
        assert!(matches!(parsed, Cmd::Pods(PodCmd::Setup { timeout: None, .. })));
        assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", "0"]).is_err());
        // Bounded above (no overflow from "no limit" spelled as a huge number).
        assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", "86400"]).is_ok());
        for big in ["86401", "18446744073709551615"] {
            assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", big]).is_err(), "{big}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_arena_block, with_arena_block, CRON_BEGIN, CRON_END};

    /// `config check` says which RunPod API is selected, warns on v1 with the retirement
    /// date, and fails on a value that's neither.
    #[test]
    fn config_check_runpod_api_row() {
        use super::{runpod_api_row, Config};
        let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API_KEY=k"));
        assert!(ok && line.contains("⚠") && line.contains("v1 (default)") && line.contains("2026-11-15"), "{line}");
        let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=v1"));
        assert!(ok && line.contains("v1 (rest.runpod.io/v1)") && line.contains("2026-11-15"), "{line}");
        let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=v2"));
        assert!(ok && line.contains("✓") && line.contains("v2") && !line.contains("retires"), "{line}");
        let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=v3"));
        assert!(!ok && line.contains("✗") && line.contains("must be `v1` or `v2`"), "{line}");
        // `config check` never shows secret values — not even a key pasted into RUNPOD_API.
        let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=rpa_SECRETKEY123"));
        assert!(!ok && line.contains("✗") && !line.contains("SECRETKEY"), "{line}");
    }

    /// `--json` on `gpus` and `pods list` is a scripting contract — pin the flag parsing
    /// (and let clap validate the whole command tree while we're at it).
    #[test]
    fn gpus_and_pods_list_take_json() {
        use super::{Cli, Cmd, PodCmd};
        use clap::{CommandFactory, Parser};
        Cli::command().debug_assert();
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().cmd;
        assert!(matches!(parse(&["arena", "gpus", "--json"]), Cmd::Gpus { json: true }));
        assert!(matches!(parse(&["arena", "gpus"]), Cmd::Gpus { json: false }));
        assert!(matches!(parse(&["arena", "pods", "list", "--json"]), Cmd::Pods(PodCmd::List { json: true, .. })));
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
            ..Default::default()
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

    fn cron_job(pull: bool, proxy: bool) -> Vec<String> {
        super::cron_lines(&super::CronJob {
            schedule: "*/15 * * * *",
            env_prefix: "",
            exe: "/opt/arena",
            config: "/srv/config.env",
            home: "/home/u",
            pull,
            proxy,
        })
    }

    #[test]
    fn cron_lines_add_the_proxy_resync_only_when_asked() {
        assert_eq!(
            cron_job(false, false),
            ["*/15 * * * * /opt/arena --config /srv/config.env pods backup --no-pull --yes >> /home/u/arena-cron.log 2>&1"]
        );
        // The proxy line carries a PATH with /usr/sbin (cron's default lacks it, and that's
        // where nginx lives on Ubuntu) and a `flock -n` so a slow tick can't pile up.
        let both = cron_job(true, true);
        assert_eq!(
            both,
            [
                "*/15 * * * * /opt/arena --config /srv/config.env pods backup --yes >> /home/u/arena-cron.log 2>&1",
                "*/5 * * * * PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
                 flock -n /home/u/.arena-proxy-cron.lock /opt/arena --config /srv/config.env proxy apply --yes \
                 >> /home/u/arena-proxy-cron.log 2>&1",
            ]
        );
        // Both live inside the one arena block; re-installing without --proxy drops the
        // proxy line again and leaves other entries alone.
        let tab = with_arena_block("0 9 * * * keep-me\n", &both);
        assert_eq!(tab.matches(CRON_BEGIN).count(), 1);
        let block: Vec<&str> = tab.lines().skip_while(|l| *l != CRON_BEGIN).collect();
        assert!(block.iter().any(|l| l.contains("proxy apply --yes")), "{tab}");
        let tab = with_arena_block(&tab, &cron_job(false, false));
        assert!(tab.contains("keep-me") && tab.contains("pods backup") && !tab.contains("proxy apply"), "{tab}");
    }

    #[test]
    fn sync_line_reports_each_outcome_on_one_line() {
        use super::{sync_line, ProxySync, Written};
        use arena_core::proxy::ChangeCounts;
        let synced = |written, counts, removed: &[&str], pending, failed: &[&str]| ProxySync::Synced {
            written,
            counts,
            removed: removed.iter().map(|s| s.to_string()).collect(),
            pending,
            failed_providers: failed.iter().map(|s| s.to_string()).collect(),
            routed: vec![],
        };
        let removed_one = ChangeCounts { removed: 1, unchanged: 4, ..Default::default() };
        let cases: Vec<(ProxySync, &str)> = vec![
            (
                synced(Written::Reloaded, removed_one, &["arena8-apple"], 0, &[]),
                "[proxy] after terminate: +0 ~0 -1 =0 (deployed; removed arena8-apple)",
            ),
            (
                synced(Written::Unchanged, ChangeCounts { unchanged: 5, ..Default::default() }, &[], 0, &[]),
                "[proxy] after terminate: +0 ~0 -0 =0 (unchanged)",
            ),
            (
                synced(Written::WriteOnly, ChangeCounts { added: 1, ..Default::default() }, &[], 2, &[]),
                "[proxy] after terminate: +1 ~0 -0 =0 (written; write-only, nginx not reloaded; \
                 2 pod(s) not forwarded yet (no SSH endpoint))",
            ),
            (
                synced(
                    Written::Reloaded,
                    ChangeCounts { removed: 5, kept: 2, ..Default::default() },
                    &["a", "b", "c", "d", "e"],
                    0,
                    &["vast"],
                ),
                "[proxy] after terminate: +0 ~0 -5 =2 (deployed; removed a, b, c +2 more; \
                 vast failed to list — forwards kept)",
            ),
            (
                ProxySync::Skipped("no proxy configured (SSH_PROXY_HOST unset)".into()),
                "[proxy] after terminate: skipped — no proxy configured (SSH_PROXY_HOST unset)",
            ),
            (
                ProxySync::Failed("not touching the proxy config: every provider failed".into()),
                "[proxy] after terminate: NOT synced — not touching the proxy config: every provider \
                 failed (retry: `arena proxy apply`)",
            ),
            // What a failing `nginx -t` really prints: several stderr lines, folded into one.
            (
                ProxySync::Failed(
                    "local nginx reload failed: nginx: [emerg] unexpected \"}\" in /p.conf:9\n\
                     nginx: configuration file /etc/nginx/nginx.conf test failed — previous config put back"
                        .into(),
                ),
                "[proxy] after terminate: NOT synced — local nginx reload failed: nginx: [emerg] \
                 unexpected \"}\" in /p.conf:9 nginx: configuration file /etc/nginx/nginx.conf test \
                 failed — previous config put back (retry: `arena proxy apply`)",
            ),
            (
                ProxySync::Failed(
                    "nginx reload on p.example failed: Warning: Permanently added 'p.example' (ED25519) \
                     to the list of known hosts.\r\nnginx: [emerg] bind() failed"
                        .into(),
                ),
                "[proxy] after terminate: NOT synced — nginx reload on p.example failed: Warning: \
                 Permanently added 'p.example' (ED25519) to the list of known hosts. nginx: [emerg] \
                 bind() failed (retry: `arena proxy apply`)",
            ),
        ];
        for (outcome, want) in cases {
            let got = sync_line("terminate", &outcome);
            assert_eq!(got, want);
            assert!(!got.contains('\n'), "one line: {got}");
        }
    }

    #[test]
    fn undeployable_only_without_a_proxy_or_without_nginx_unless_write_only() {
        use super::undeployable_reason;
        use arena_core::proxy::ProxyConfig;
        let px = |extra: &str| {
            ProxyConfig::from_config(&arena_core::Config::parse(&format!("SSH_PROXY_HOST=proxy.example.com\n{extra}")))
                .unwrap()
        };
        let local = px("");
        let remote = px("PROXY_LOCAL=false\n");
        let write_only = px("PROXY_LOCAL=false\nSSH_PROXY_RELOAD_CMD=\"\"\n");

        assert!(undeployable_reason(None, true).unwrap().contains("SSH_PROXY_HOST"));
        assert_eq!(undeployable_reason(Some(&local), true), None);
        assert!(undeployable_reason(Some(&local), false).unwrap().contains("not found on this host"));
        assert_eq!(undeployable_reason(Some(&remote), true), None);
        let why = undeployable_reason(Some(&remote), false).unwrap();
        assert!(why.contains("proxy.example.com") && why.contains("unreachable"), "{why}");
        // Write-only never runs nginx, so a missing nginx never blocks it.
        assert_eq!(undeployable_reason(Some(&write_only), false), None);
    }

    #[test]
    fn written_line_says_what_reached_the_proxy() {
        use super::{written_line, Written};
        use arena_core::proxy::ProxyConfig;
        let px = |extra: &str| {
            ProxyConfig::from_config(&arena_core::Config::parse(&format!(
                "SSH_PROXY_HOST=proxy.example.com\nSSH_PROXY_NGINX_CONFIG_PATH=/srv/p.conf\n{extra}"
            )))
            .unwrap()
        };
        let (local, remote) = (px(""), px("PROXY_LOCAL=false\n"));
        assert_eq!(written_line(&local, 3, Written::Unchanged), None);
        assert_eq!(
            written_line(&local, 3, Written::WriteOnly).as_deref(),
            Some("[proxy] wrote 3 forward(s) to /srv/p.conf (write-only: nginx not reloaded)")
        );
        assert_eq!(
            written_line(&local, 3, Written::Reloaded).as_deref(),
            Some("[proxy] deployed 3 forward(s) locally and reloaded nginx")
        );
        assert_eq!(
            written_line(&remote, 2, Written::WriteOnly).as_deref(),
            Some("[proxy] wrote 2 forward(s) to proxy.example.com:/srv/p.conf (write-only: nginx not reloaded)")
        );
        assert_eq!(
            written_line(&remote, 2, Written::Reloaded).as_deref(),
            Some("[proxy] deployed 2 forward(s) to proxy.example.com and reloaded nginx")
        );
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
    fn plan_config_header_only_says_write_for_a_real_merge() {
        use super::plan_config_header;
        assert_eq!(
            plan_config_header("/srv/p.conf", true),
            "# ----- nginx config (write to /srv/p.conf on the proxy) -----"
        );
        let preview = plan_config_header("/srv/p.conf", false);
        assert!(preview.contains("LISTING-ONLY PREVIEW") && preview.contains("do NOT deploy"), "{preview}");
        assert!(!preview.contains("write to"), "{preview}");
    }

    #[test]
    fn sh_path_keeps_tilde_expansion_and_quotes_the_rest() {
        use super::sh_path;
        assert_eq!(sh_path("~/proxy.conf"), "\"$HOME\"/'proxy.conf'");
        assert_eq!(sh_path("/etc/nginx/streams-enabled/arena.conf"), "'/etc/nginx/streams-enabled/arena.conf'");
        assert_eq!(sh_path("/a b/it's"), "'/a b/it'\\''s'");
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

/// End-to-end tests of the proxy deploy path (read current file → merge → write), against
/// a temp file in **write-only** mode (`SSH_PROXY_RELOAD_CMD=""`, loopback host = local)
/// — so they can never run nginx or SSH anywhere.
#[cfg(test)]
mod proxy_deploy_tests {
    use super::{
        deploy_proxy, emit_proxy_plan, fleet_listing, handle_pods, prepare_proxy, read_current_proxy,
        remote_install_script, replace_file, sync_proxy, write_proxy, PodCmd, ProxyChanged, ProxySync, Written,
        INSTALL_FAILED, RELOAD_FAILED_NOT_RESTORED, RELOAD_FAILED_RESTORED,
    };
    use arena_core::proxy::{Listing, ProviderListing, ProxyConfig};
    use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    fn cfg(path: &Path) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=arena8\nSSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\n\
             SSH_PROXY_RELOAD_CMD=\"\"\nMACHINE_NAME_LIST=(\n  \"apple\"\n  \"autumn\"\n)\n",
            path.display()
        ))
    }

    /// A fresh temp path (per process + test name), removed again when dropped.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    impl std::ops::Deref for Tmp {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl AsRef<Path> for Tmp {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }
    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("arena-proxy-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Tmp(p)
    }

    fn pod(provider: &str, name: &str, ip: &str, port: u16) -> Pod {
        Pod {
            id: format!("{provider}-{name}"),
            name: name.into(),
            provider: provider.into(),
            ssh_ip: Some(ip.into()),
            ssh_port: Some(port),
            ..Default::default()
        }
    }

    fn listing(v: Vec<(&str, std::result::Result<Vec<Pod>, &str>)>) -> Listing {
        Listing {
            providers: v
                .into_iter()
                .map(|(p, r)| ProviderListing { provider: p.into(), pods: r.map_err(String::from) })
                .collect(),
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[tokio::test]
    async fn deploy_merges_with_the_file_it_wrote_and_refuses_on_total_failure() {
        let path = tmp("merge.conf");
        let cfg = cfg(&path);
        assert!(ProxyConfig::from_config(&cfg).unwrap().write_only(), "tests must never reload nginx");
        let apple = pod("runpod", "arena8-apple", "1.1.1.1", 22000);
        let autumn = pod("vast", "arena8-autumn", "ssh4.vast.ai", 31000);

        // 1. both providers answer → both forwards written (no file before = empty prev).
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![apple.clone()])), ("vast", Ok(vec![autumn]))]), false)
            .await
            .unwrap();
        let first = read(&path);
        assert!(first.contains("proxy_pass 1.1.1.1:22000;") && first.contains("proxy_pass ssh4.vast.ai:31000;"));

        // 2. vast 429s while runpod moved apple: apple follows, autumn is kept.
        let moved = pod("runpod", "arena8-apple", "2.2.2.2", 22001);
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![moved])), ("vast", Err("vast list HTTP 429"))]), false)
            .await
            .unwrap();
        let second = read(&path);
        assert!(second.contains("proxy_pass 2.2.2.2:22001;") && second.contains("proxy_pass ssh4.vast.ai:31000;"));

        // 3. nobody answers → refuse, file untouched.
        let err = deploy_proxy(&cfg, &listing(vec![("runpod", Err("HTTP 500")), ("vast", Err("HTTP 429"))]), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not touching the proxy config"), "{err}");
        assert_eq!(read(&path), second);

        // 4. runpod answers without apple (terminated) → removed; vast still down → kept.
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![])), ("vast", Err("HTTP 429"))]), false).await.unwrap();
        let fourth = read(&path);
        assert!(!fourth.contains("arena8-apple") && fourth.contains("proxy_pass ssh4.vast.ai:31000;"));
    }

    #[tokio::test]
    async fn legacy_file_is_migrated_not_dropped() {
        let path = tmp("legacy.conf");
        std::fs::write(
            &path,
            "# Generated by arena-infra-rs `arena proxy plan`. Do not edit by hand.\n\
             # arena8-apple\nserver {\n    listen 7000;\n    proxy_pass 1.1.1.1:22000;\n    proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}\n\
             # arena8-autumn\nserver {\n    listen 7001;\n    proxy_pass ssh4.vast.ai:31000;\n    proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}\n",
        )
        .unwrap();
        let cfg = cfg(&path);
        let apple = pod("runpod", "arena8-apple", "1.1.1.1", 22000);
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![apple])), ("vast", Err("HTTP 429"))]), false).await.unwrap();
        let text = read(&path);
        assert!(text.contains("# arena-forward name=arena8-apple port=7000 target=1.1.1.1:22000 provider=runpod"));
        // The legacy autumn entry's owner is unknown and vast didn't answer → kept.
        assert!(text.contains("# arena-forward name=arena8-autumn port=7001 target=ssh4.vast.ai:31000\n"));
    }

    #[tokio::test]
    async fn write_is_idempotent_and_honours_abort() {
        let path = tmp("idem.conf");
        let cfg = cfg(&path);
        let l = listing(vec![("runpod", Ok(vec![pod("runpod", "arena8-apple", "1.1.1.1", 22000)]))]);
        let p = prepare_proxy(&cfg, &l).await.unwrap();
        assert!(!p.up_to_date());
        assert_eq!(write_proxy(&cfg, &p).await.unwrap(), Written::WriteOnly);
        let again = prepare_proxy(&cfg, &l).await.unwrap();
        assert!(again.up_to_date(), "a second apply must be a no-op");
        assert_eq!(write_proxy(&cfg, &again).await.unwrap(), Written::Unchanged);

        // Even a hand-assembled prepared config with `abort` set can't be written.
        let mut forced = prepare_proxy(&cfg, &listing(vec![("runpod", Ok(vec![]))])).await.unwrap();
        assert!(!forced.up_to_date());
        forced.plan.abort = Some("test".into());
        assert!(write_proxy(&cfg, &forced).await.is_err());
        assert!(read(&path).contains("proxy_pass 1.1.1.1:22000;"));
    }

    #[tokio::test]
    async fn current_config_missing_is_empty_but_unreadable_is_an_error() {
        let missing = tmp("missing.conf");
        let px = ProxyConfig::from_config(&cfg(&missing)).unwrap();
        assert_eq!(read_current_proxy(&cfg(&missing), &px).await.unwrap(), None);

        // A path we can't read (a directory) must abort, not read as "no forwards".
        let dir = tmp("a-directory");
        std::fs::create_dir_all(&dir).unwrap();
        let px = ProxyConfig::from_config(&cfg(&dir)).unwrap();
        let err = read_current_proxy(&cfg(&dir), &px).await.unwrap_err();
        assert!(err.to_string().contains("refusing to write"), "{err}");
    }

    /// A single backend: `fleet_listing` reports its outcome under its own name.
    struct Single(bool);

    #[async_trait]
    impl Provider for Single {
        fn name(&self) -> &'static str {
            "vast"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            if self.0 {
                Ok(vec![pod("vast", "arena8-autumn", "ssh4.vast.ai", 31000)])
            } else {
                Err(Error::provider("vast list HTTP 429"))
            }
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

    #[tokio::test]
    async fn fleet_listing_keeps_a_failure_as_a_failure() {
        let ok = fleet_listing(&Single(true)).await;
        assert_eq!(ok.pods().len(), 1);
        let bad = fleet_listing(&Single(false)).await;
        assert!(!bad.any_ok());
        assert_eq!(bad.errors(), vec![("vast", "provider error: vast list HTTP 429")]);
    }

    /// A fleet whose per-provider listing the test scripts between lifecycle steps — the
    /// shape `build_fleet` hands the pods commands.
    struct Fleet(std::sync::Mutex<Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>>);

    impl Fleet {
        fn new(v: Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>) -> Self {
            Fleet(std::sync::Mutex::new(v))
        }
        fn set(&self, v: Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>) {
            *self.0.lock().unwrap() = v;
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
            Ok(self.0.lock().unwrap().iter().filter_map(|(_, r)| r.clone().ok()).flatten().collect())
        }
        async fn list_by_provider(&self) -> Vec<(String, Result<Vec<Pod>>)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(p, r)| (p.to_string(), r.clone().map_err(Error::provider)))
                .collect()
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

    fn synced(s: &ProxySync) -> (Written, String, Vec<String>, usize, Vec<String>) {
        match s {
            ProxySync::Synced { written, counts, removed, pending, failed_providers, .. } => {
                (*written, counts.compact(), removed.clone(), *pending, failed_providers.clone())
            }
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sync_skips_without_a_proxy_and_never_lists() {
        // No SSH_PROXY_HOST: a quiet skip, not an error — and the fleet isn't even listed
        // (a fleet that would panic on listing proves it).
        struct Unlistable;
        #[async_trait]
        impl Provider for Unlistable {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> Result<Vec<Pod>> {
                panic!("a skipped sync must not list the fleet")
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
        let cfg = Config::parse("MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(\n  \"apple\"\n)\n");
        match sync_proxy(&cfg, &Unlistable, "terminate").await {
            ProxySync::Skipped(why) => assert!(why.contains("SSH_PROXY_HOST"), "{why}"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sync_follows_a_create_terminate_lifecycle_and_never_drops_on_doubt() {
        let path = tmp("sync.conf");
        let cfg = cfg(&path);
        let apple = pod("runpod", "arena8-apple", "1.1.1.1", 22000);
        let autumn = pod("vast", "arena8-autumn", "ssh4.vast.ai", 31000);
        let booting = Pod { ssh_ip: None, ssh_port: None, ..autumn.clone() };
        let fleet = Fleet::new(vec![("runpod", Ok(vec![apple.clone()])), ("vast", Ok(vec![booting]))]);

        // after create: apple is wired, autumn is still booting → reported as pending.
        let s = sync_proxy(&cfg, &fleet, "create").await;
        assert_eq!(synced(&s), (Written::WriteOnly, "+1 ~0 -0 =0".into(), vec![], 1, vec![]));
        assert!(read(&path).contains("proxy_pass 1.1.1.1:22000;"));

        // the same fleet again → nothing written.
        let s = sync_proxy(&cfg, &fleet, "up").await;
        assert_eq!(synced(&s).0, Written::Unchanged);

        // autumn got its endpoint.
        fleet.set(vec![("runpod", Ok(vec![apple.clone()])), ("vast", Ok(vec![autumn.clone()]))]);
        let s = sync_proxy(&cfg, &fleet, "up").await;
        assert_eq!(synced(&s), (Written::WriteOnly, "+1 ~0 -0 =0".into(), vec![], 0, vec![]));
        match &s {
            ProxySync::Synced { routed, .. } => assert_eq!(routed.len(), 2, "both confirmed by this listing"),
            other => panic!("{other:?}"),
        }
        let both = read(&path);

        // terminate apple, but runpod still lists it (exited, no endpoint) → kept, untouched.
        let exiting = Pod { status: "EXITED".into(), ssh_ip: None, ssh_port: None, ..apple.clone() };
        fleet.set(vec![("runpod", Ok(vec![exiting])), ("vast", Ok(vec![autumn.clone()]))]);
        let s = sync_proxy(&cfg, &fleet, "terminate").await;
        assert_eq!(synced(&s), (Written::Unchanged, "+0 ~0 -0 =1".into(), vec![], 0, vec![]));
        assert_eq!(read(&path), both);

        // vast 429s while runpod has stopped listing apple → apple removed, autumn kept.
        fleet.set(vec![("runpod", Ok(vec![])), ("vast", Err("vast list HTTP 429"))]);
        let s = sync_proxy(&cfg, &fleet, "terminate").await;
        assert_eq!(
            synced(&s),
            (Written::WriteOnly, "+0 ~0 -1 =1".into(), vec!["arena8-apple".into()], 0, vec!["vast".into()])
        );
        let text = read(&path);
        assert!(!text.contains("arena8-apple") && text.contains("proxy_pass ssh4.vast.ai:31000;"));

        // nobody answers → Failed (a warning, never an error), and the file is untouched.
        fleet.set(vec![("runpod", Err("HTTP 500")), ("vast", Err("HTTP 429"))]);
        match sync_proxy(&cfg, &fleet, "rename").await {
            ProxySync::Failed(e) => assert!(e.contains("not touching the proxy config"), "{e}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(read(&path), text);
    }

    /// `cfg` with a reload command instead of write-only. The command is a shell snippet
    /// run via `sh -c` (echo/exit) — never nginx.
    fn cfg_reloading(path: &Path, reload: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=arena8\nSSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\n\
             SSH_PROXY_RELOAD_CMD=\"{reload}\"\nMACHINE_NAME_LIST=(\n  \"apple\"\n  \"autumn\"\n)\n",
            path.display()
        ))
    }

    #[tokio::test]
    async fn a_failed_reload_puts_the_previous_config_back_so_the_next_sync_retries() {
        let path = tmp("reload.conf");
        let marker = tmp("reload.marker");
        let apple = |ip: &str| pod("runpod", "arena8-apple", ip, 22000);
        deploy_proxy(&cfg(&path), &listing(vec![("runpod", Ok(vec![apple("1.1.1.1")]))]), false).await.unwrap();
        let v1 = read(&path);

        // apple moved, but the reload fails: an error, and the live file is v1 again —
        // what nginx is still running.
        let failing = cfg_reloading(
            &path,
            &format!("echo ran >> {}; echo 'nginx: [emerg] boom' >&2; exit 1", marker.display()),
        );
        let moved = listing(vec![("runpod", Ok(vec![apple("2.2.2.2")]))]);
        let err = deploy_proxy(&failing, &moved, false).await.unwrap_err().to_string();
        assert!(err.contains("local nginx reload failed: nginx: [emerg] boom"), "{err}");
        assert!(err.contains("previous config put back"), "{err}");
        assert_eq!(read(&path), v1);

        // So the next sync still sees the difference: it writes and reloads again (it used
        // to report "unchanged" here and never reload).
        let working = cfg_reloading(&path, &format!("echo ran >> {}", marker.display()));
        let (_, written) = deploy_proxy(&working, &moved, false).await.unwrap();
        assert_eq!(written, Written::Reloaded);
        assert!(read(&path).contains("proxy_pass 2.2.2.2:22000;"));
        assert_eq!(read(&marker).lines().count(), 2, "the reload ran again");

        // No file before + a failing reload: no file left behind either.
        let fresh = tmp("reload-fresh.conf");
        assert!(deploy_proxy(&cfg_reloading(&fresh, "exit 1"), &moved, false).await.is_err());
        assert!(!fresh.exists());
    }

    #[tokio::test]
    async fn a_write_planned_against_a_stale_read_is_refused_then_redone() {
        let path = tmp("cas.conf");
        let cfg = cfg(&path);
        let apple = pod("runpod", "arena8-apple", "1.1.1.1", 22000);
        let autumn = pod("vast", "arena8-autumn", "ssh4.vast.ai", 31000);
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![apple.clone()])), ("vast", Ok(vec![]))]), false)
            .await
            .unwrap();
        // Writer A reads and plans: apple moved, vast is down.
        let a_listing = listing(vec![
            ("runpod", Ok(vec![pod("runpod", "arena8-apple", "2.2.2.2", 22000)])),
            ("vast", Err("HTTP 429")),
        ]);
        let a = prepare_proxy(&cfg, &a_listing).await.unwrap();
        // Meanwhile writer B (a lifecycle sync, the cron) adds autumn.
        deploy_proxy(&cfg, &listing(vec![("runpod", Ok(vec![apple])), ("vast", Ok(vec![autumn]))]), false)
            .await
            .unwrap();
        let b_file = read(&path);
        assert!(b_file.contains("proxy_pass ssh4.vast.ai:31000;"));

        // A's write would drop autumn (its merge never saw it): refused, file untouched.
        let err = write_proxy(&cfg, &a).await.unwrap_err();
        assert!(err.is::<ProxyChanged>(), "{err}");
        assert_eq!(read(&path), b_file);
        // Redone against the current file, A's merge moves apple and keeps autumn.
        deploy_proxy(&cfg, &a_listing, false).await.unwrap();
        let merged = read(&path);
        assert!(merged.contains("proxy_pass 2.2.2.2:22000;") && merged.contains("proxy_pass ssh4.vast.ai:31000;"), "{merged}");
    }

    /// A fresh, empty temp directory (removed with its contents at the end of the test).
    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp_dir(name: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!("arena-proxy-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    #[test]
    fn replace_file_swaps_atomically_and_keeps_mode_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("replace");
        let real = dir.0.join("real.conf");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.0.join("link.conf");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        replace_file(&link, "new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is kept");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        let names: Vec<String> =
            std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "no temp file left behind: {names:?}");

        // A directory we can't create files in (only the file is writable): falls back to
        // the in-place write instead of failing a deploy that used to work.
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555)).unwrap();
        replace_file(&real, "newer").unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "newer");
    }

    #[test]
    fn remote_install_script_puts_the_previous_config_back_on_a_failed_reload() {
        // Plain POSIX sh, so it runs here against temp files; the "reload commands" are
        // true/false/exit — nothing touches nginx or SSH.
        let dir = tmp_dir("install");
        let live = dir.0.join("proxy.conf");
        let upload = dir.0.join("upload.conf");
        let run_at = |live: &Path, reload: Option<&str>| -> (Option<i32>, Option<String>) {
            std::fs::write(&upload, "NEW").unwrap();
            let script = remote_install_script(&live.to_string_lossy(), &upload.to_string_lossy(), reload);
            let out = std::process::Command::new("sh").args(["-c", &script]).output().unwrap();
            assert!(!upload.exists(), "upload cleaned up");
            assert!(!dir.0.join("upload.conf.prev").exists(), "backup cleaned up");
            (out.status.code(), std::fs::read_to_string(live).ok())
        };
        let run = |reload: Option<&str>| run_at(&live, reload);
        let (new, old) = (Some("NEW".to_string()), Some("OLD".to_string()));

        std::fs::write(&live, "OLD").unwrap();
        assert_eq!(run(Some("true")), (Some(0), new.clone()));
        std::fs::write(&live, "OLD").unwrap();
        assert_eq!(run(Some("echo bad >&2; exit 1")), (Some(RELOAD_FAILED_RESTORED), old.clone()));
        // A trailing `# comment` in the reload command can't swallow the rest of the script.
        assert_eq!(run(Some("false # reload")), (Some(RELOAD_FAILED_RESTORED), old.clone()));
        // The restore itself failing (here: the backup vanished) says so.
        assert_eq!(run(Some("rm -f \"$b\"; exit 1")).0, Some(RELOAD_FAILED_NOT_RESTORED));
        // No live file before: a failed reload leaves none; write-only just installs.
        std::fs::remove_file(&live).unwrap();
        assert_eq!(run(Some("exit 1")), (Some(RELOAD_FAILED_RESTORED), None));
        assert_eq!(run(None), (Some(0), new));
        // Installing where it can't (a missing directory) fails before anything changed.
        let nowhere = dir.0.join("missing/proxy.conf");
        assert_eq!(run_at(&nowhere, Some("true")), (Some(INSTALL_FAILED), None));
    }

    #[test]
    fn proxy_plan_without_the_current_config_refuses_out() {
        let out = tmp("plan-out.conf");
        let remote = Config::parse(
            "MACHINE_NAME_PREFIX=arena8\nSSH_PROXY_HOST=proxy.example.com\nPROXY_LOCAL=false\n\
             MACHINE_NAME_LIST=(\n  \"apple\"\n)\n",
        );
        let l = listing(vec![("runpod", Ok(vec![pod("runpod", "arena8-apple", "1.1.1.1", 22000)]))]);
        // A remote proxy's config isn't read by `proxy plan` (it never connects), so the
        // output is listing-only: saving it for a manual deploy is refused.
        let err = emit_proxy_plan(&l, &remote, Some(&out)).unwrap_err().to_string();
        assert!(err.contains("--out needs the current proxy config"), "{err}");
        assert!(!out.exists());
        emit_proxy_plan(&l, &remote, None).unwrap();
        // A local proxy (no file yet = a known, empty config) merges and may save it.
        let path = tmp("plan-local.conf");
        emit_proxy_plan(&l, &cfg(&path), Some(&out)).unwrap();
        assert!(read(&out).contains("proxy_pass 1.1.1.1:22000;"));
    }

    /// Creates `arena8-apple` (with an endpoint at once, like Hetzner) and fails on any
    /// other name; lists what it created.
    struct HalfCreate(std::sync::Mutex<Vec<Pod>>);

    #[async_trait]
    impl Provider for HalfCreate {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
            if spec.name != "arena8-apple" {
                return Err(Error::provider(format!("create {}: HTTP 400 bad request", spec.name)));
            }
            let p = pod("runpod", &spec.name, "1.1.1.1", 22000);
            self.0.lock().unwrap().push(p.clone());
            Ok(p)
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

    #[tokio::test]
    async fn a_create_that_fails_part_way_still_syncs_the_pods_it_made() {
        let path = tmp("partial-create.conf");
        let cmd = PodCmd::Create {
            names: vec!["apple".into(), "autumn".into()],
            count: None,
            add: None,
            gpu: None,
            gpus: None,
            cloud: None,
            max_price: None,
            order: Default::default(),
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            skip_proxy: false,
            dry_run: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 0,
        };
        let remote = std::sync::Arc::new(arena_core::remote::FakeRemote::new());
        let err = handle_pods(cmd, &HalfCreate(Default::default()), remote, &cfg(&path), true).await.unwrap_err();
        assert!(err.to_string().contains("creating arena8-autumn"), "{err}");
        assert!(read(&path).contains("proxy_pass 1.1.1.1:22000;"), "apple (made before the failure) is forwarded");
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
            Ok(self.made.lock().unwrap().clone())
        }
        async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
            self.calls.lock().unwrap().push((spec.name.clone(), spec.gpu_type.clone(), spec.cloud_type.clone()));
            if self.dry.contains(&spec.gpu_type.as_str()) {
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
            cloud: cloud.map(String::from),
            max_price,
            order: Order::Cheapest,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
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
        for o in [ov(None, None), ov(Some("3090"), None), ov(Some("A4000"), Some("secure"))] {
            assert!(route(&o, None, false).unwrap().is_none(), "{o:?}");
            assert!(route(&o, None, true).unwrap().is_none(), "{o:?}");
        }
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
            cloud: Some("community,secure".into()),
            max_price: Some(0.30),
            order: Order::Listed,
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            dry_run: true,
            no_wait: false,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: false,
            timeout: 600,
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
            cloud: None,
            max_price: None,
            order: Order::Listed, // 3090 first despite the price
            disk: None,
            volume: None,
            image: None,
            bootstrap: false,
            dry_run: false,
            no_wait: true,
            keep_trying: false,
            retry_mins: 0,
            retry_secs: 60,
            no_setup: true,
            timeout: 600,
            interval: 12,
        };
        handle_pods(up, &fake, fake_remote(), &cfg(), true).await.unwrap();
        assert_eq!(
            fake.calls(),
            [call("arena8-apple", "NVIDIA GeForce RTX 3090", "COMMUNITY"), call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]
        );
    }

    #[tokio::test]
    async fn offers_plans_without_a_fleet_or_a_key() {
        // Off RunPod nothing is priced, so a cap leaves nothing — but it's a view, not an error.
        handle_offers(&cfg(), "vast", Some("A4000,3090".into()), Some("community,secure".into()), Some(1.0), None, Order::Cheapest, true)
            .await
            .unwrap();
        handle_offers(&cfg(), "runpod", Some("A4000,3090".into()), None, None, Some(2), Order::Cheapest, false).await.unwrap();
        assert!(handle_offers(&cfg(), "lambda", None, None, None, None, Order::Cheapest, false).await.is_err());
        let base = spec_with_overrides(&cfg(), &SpecOverrides { gpus: Some(2), ..Default::default() });
        let req = arena_core::placement::Request::from_flags(Some("3090,A4000"), None, &base, Some(0.40), Order::Cheapest).unwrap();
        let plan = plan_placement(&cfg(), "runpod", &req).await.unwrap();
        assert_eq!(plan.options.len(), 1);
        assert_eq!(plan.notes[0], "no RUNPOD_API_KEY — prices are preset estimates (~)");
        assert_eq!(
            plan_header(&plan),
            "Placement options on runpod (2 GPU(s) per pod, cheapest first, max $0.40/h per pod):"
        );
    }

    #[test]
    fn placement_flags_parse() {
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        match parse(&["arena", "offers", "--gpu", "A4000,3090", "--max-price", "$0.5", "--gpus", "2", "--order", "listed", "--json"]).unwrap() {
            Cmd::Offers { gpu, max_price, gpus, order, json, .. } => {
                assert_eq!((gpu.as_deref(), max_price, gpus, order, json), (Some("A4000,3090"), Some(0.5), Some(2), Order::Listed, true));
            }
            _ => panic!("not offers"),
        }
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
        handle_backup, handle_copy, handle_deep_test, handle_init_branches,
        handle_pods, handle_run, handle_set_branch, local_size, marker_present, probe_gpus, proxy_reaches_pod,
        render_deep_test, render_run, replace_copy_failed, run_fleet, select, target_is_pod, BackupTally, Cli, Cmd,
        CopyPlan, PodCmd, RunResult, Select, SelectByFlag, Selected, Unscoped, BACKUP_TIMEOUT, BRANCH_TIMEOUT,
        CP_BASE_TIMEOUT, POD_COPY_TIMEOUT, RUN_TIMEOUT_SECS, TEST_TIMEOUT,
    };
    use arena_core::selector::SelectArgs;
    use arena_core::backup::{backup_command, checkout_command, init_branch_command, BackupConfig};
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
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
        ))
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
        let ok = |name: &str, text: &str| RunResult { name: name.into(), text: text.into(), ok: true };
        assert_eq!(
            results,
            [
                ok("devtest-apple", "hello"),
                RunResult {
                    name: "devtest-bloom".into(),
                    text: "exit Some(127): zsh: command not found: nvidia-smi".into(),
                    ok: false
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
                "exit Some(127): zsh: command not found: nvidia-smi",
                "\n── devtest-cloud ",
                "",
                "\n2 ok, 1 failed",
            ]
        );
        let (lines, _) = render_run(&results, true);
        assert_eq!(lines[1], "devtest-bloom          ✗ exit Some(127): zsh: command not found: nvidia-smi");
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
        let plain = PodCmd::Test { deep: false, sel: Select::default(), json: false, verbose: false };
        let err = handle_pods(plain, &fleet(), fake.clone(), &cfg(), true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed");
        assert_eq!(start.elapsed(), TEST_TIMEOUT);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, timeout, .. }
            if cmd.contains("import torch") && *timeout == Some(TEST_TIMEOUT))));

        // `pods run --timeout 45`.
        let fake = Arc::new(FakeRemote::new());
        let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["nvidia-smi".into()], timeout: 45, dry_run: false };
        handle_pods(run, &fleet(), fake.clone(), &cfg(), true).await.unwrap();
        assert_eq!(fake.calls().len(), 3);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(Duration::from_secs(45)))));

        // A dry run reaches no pod.
        let fake = Arc::new(FakeRemote::new());
        let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["reboot".into()], timeout: 45, dry_run: true };
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
        let tally = backup_fleet(&remote, jobs, |l| lines.push(l.to_string())).await;
        assert_eq!(tally, BackupTally { pushed: 1, unchanged: 1, skipped: 1, failed: 2 });
        assert_eq!(start.elapsed(), BACKUP_TIMEOUT);
        // The wedged pod reports last, after its budget; the rest as soon as they answered.
        assert_eq!(lines[4], "[5/5] ✗ devtest-bloom: timed out after 300s");
        let mut first: Vec<String> = unnumbered(&lines[..4]);
        first.sort();
        assert_eq!(
            first,
            [
                "= devtest-cloud (no changes, on feature-y)",
                "⊘ devtest-delta (skipped — on protected branch main)",
                "✓ devtest-apple -> autocommit-x",
                "✗ devtest-echo (exit Some(128)): fatal: Could not read from remote repository.",
            ]
        );

        // Through the command: each pod's own commit message, the budget, a non-zero result.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::exit(1, "boom")]);
        let err = handle_backup(fake.clone(), &cfg(), true, None, &everyone(&fleet())).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let want = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"));
            assert_eq!(execs(&fake, port), [(want, Some(BACKUP_TIMEOUT))]);
        }
        // Dry run: nothing reaches a pod.
        let fake = Arc::new(FakeRemote::new());
        handle_backup(fake.clone(), &cfg(), false, None, &everyone(&fleet())).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn set_branch_dry_run_runs_nothing_and_apply_runs_the_checkout_on_each_pod() {
        let fake = Arc::new(FakeRemote::new());
        handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, true, true).await.unwrap();
        assert!(fake.calls().is_empty(), "dry run: {:?}", fake.calls());

        // Gentle, whole fleet: the ff-only checkout on every pod, within the budget.
        handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true).await.unwrap();
        let gentle = checkout_command(REPO, "main", Some(KEY), false);
        for port in [22001, 22002, 22003] {
            assert_eq!(execs(&fake, port), [(gentle.clone(), Some(BRANCH_TIMEOUT))], "port {port}");
        }

        // --hard on one pod: only that pod, the destructive command.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "main", "bloom", "--hard"]), &fleet(), fake.clone(), &cfg(), true).await.unwrap();
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        assert!(hard.contains("reset --hard"));
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(execs(&fake, 22002), [(hard, Some(BRANCH_TIMEOUT))]);

        // Concurrent: a wedged pod costs the run one budget (not one per pod after it), fails it.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::hang()]);
        let start = Instant::now();
        let err =
            handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
        assert_eq!(start.elapsed(), BRANCH_TIMEOUT);
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn init_branches_runs_each_pods_branch_command_within_the_budget() {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22003), [FakeReply::exit(1, "error: failed to push some refs")]);
        let err = handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), false, true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to init branch");
        let bcfg = BackupConfig::from_config(&cfg(), 1, 2);
        for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
            let want = init_branch_command(&bcfg, &format!("devtest-{name}"));
            assert!(want.contains(&format!("autocommit-devtest-w1d2-{name}")));
            assert_eq!(execs(&fake, port), [(want, Some(BRANCH_TIMEOUT))]);
        }
        let fake = Arc::new(FakeRemote::new());
        handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), true, true).await.unwrap();
        assert!(fake.calls().is_empty(), "dry run");
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

    #[tokio::test(start_paused = true)]
    async fn a_direct_pod_copy_that_times_out_stops_instead_of_falling_back() {
        let f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
        let fake = FakeRemote::new();
        // identity check, plant the marker, the direct rsync (wedged), clean the marker.
        fake.script(&host(22001), [FakeReply::stdout("id-devtest-apple"), FakeReply::ok(), FakeReply::hang()]);
        let start = Instant::now();
        let err = copy_pod_files(&cfg(), &f, &fake, "id-devtest-apple", "id-devtest-apple-new").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("direct pod-to-pod copy timed out after 7200s — NOT swapping"), "{msg}");
        assert_eq!(start.elapsed(), POD_COPY_TIMEOUT);
        let src = execs(&fake, 22001);
        assert_eq!(src.len(), 4, "{src:?}");
        assert!(src[2].0.starts_with("rsync ") && src[2].1 == Some(POD_COPY_TIMEOUT), "{src:?}");
        assert!(src[3].0.starts_with("rm -f ") && src[3].1 == Some(PROBE_TIMEOUT), "source marker cleaned: {src:?}");
        assert!(fake.calls_to(&host(22002)).is_empty(), "no via-local push, no delivery check");
        // It doesn't promise a plain re-run continues: `replace` refuses while `-new` exists.
        assert!(!msg.contains("Re-run"), "{msg}");
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
    const DEEP_HEALTHY: &str = "\
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
    const DEEP_CUINIT_999: &str = "\
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
        let deep = |json: bool| PodCmd::Test { deep: true, sel: Select::default(), json, verbose: false };
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
                    "exit Some(255): ssh: connect to host 10.0.0.1 port 22002: Connection refused"
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
        // Without names, a pod without an endpoint is skipped (with a note), not failed.
        let fake = Arc::new(FakeRemote::new());
        let all: Vec<_> = (0..3).map(|_| FakeReply::stdout(DEEP_HEALTHY)).collect();
        for port in [22002, 22003] {
            fake.script(&host(port), all.clone());
        }
        let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &everyone(&f)).await.unwrap();
        assert_eq!(results.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["devtest-bloom", "devtest-cloud"]);
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
            Cmd::Pods(PodCmd::Test { deep: false, ref sel, json: false, verbose: false }) if sel.targets.is_empty()
        ));
        match parse(&["arena", "pods", "test", "--deep", "apple", "bloom", "--json", "-v"]).unwrap() {
            Cmd::Pods(PodCmd::Test { deep, sel, json, verbose }) => {
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
        disk_fate, handle_pods, handle_rename, handle_restart, handle_terminate, rename_followup_steps,
        rename_report, wipe_refusal, Cli, Cmd, DiskFate, DiskOp, KeyBook, PlannedRename, PodCmd, RenameRequest,
        RestartOpts, SettleWait,
    };
    use arena_core::openrouter::{KeyApi, KeyInfo};
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
    use arena_core::setup::name_file_command;
    use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
    use async_trait::async_trait;
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
    }

    impl Fleet {
        fn new(pods: Vec<Pod>) -> Self {
            Self { pods: Mutex::new(pods), calls: Mutex::default(), volumes: vec![], no_spec: vec![], fail_rename: vec![] }
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
            Ok(self.pods.lock().unwrap().clone())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised by lifecycle tests")
        }
        async fn stop_pod(&self, id: &str) -> Result<()> {
            self.record(format!("stop {id}"));
            Ok(())
        }
        async fn restart_pod(&self, id: &str) -> Result<()> {
            self.record(format!("restart {id}"));
            Ok(())
        }
        async fn terminate_pod(&self, id: &str) -> Result<()> {
            self.record(format!("terminate {id}"));
            self.pods.lock().unwrap().retain(|p| p.id != id);
            Ok(())
        }
        async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
            if self.fail_rename.contains(&id) {
                return Err(Error::provider("rename refused"));
            }
            self.record(format!("rename {id} {new_name}"));
            for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.name = new_name.to_string();
            }
            Ok(())
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

    /// Setup + a write-only proxy in `dir` (never reloads nginx, never SSHes anywhere).
    fn cfg(dir: &Path) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo charlie delta echo @solo-gpu)\n\
             ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
             SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\n",
            dir.join("proxy.conf").display()
        ))
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

    /// The provisioning API: keys by name, recording deletes/renames; `broken` names fail.
    #[derive(Default)]
    struct FakeKeys {
        keys: Mutex<Vec<KeyInfo>>,
        calls: Mutex<Vec<String>>,
        broken: Vec<&'static str>,
    }

    impl FakeKeys {
        fn with(names: &[(&str, &str)]) -> Self {
            let keys = names
                .iter()
                .map(|(hash, name)| KeyInfo {
                    hash: hash.to_string(),
                    name: Some(name.to_string()),
                    label: None,
                    disabled: false,
                    limit: Some(5.0),
                    usage: None,
                })
                .collect();
            Self { keys: Mutex::new(keys), ..Default::default() }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn names(&self) -> Vec<String> {
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
        // What the operator is told about both.
        let report = rename_report(
            &[("solo-gpu".into(), "no SSH endpoint".into()), ("devtest-echo".into(), "exit 255: refused".into())],
            &["devtest-echo already has its own OpenRouter key".into()],
        );
        assert!(report[0].contains("MACHINE_NAME env var keeps the old name") && report[0].contains("reimage"), "{report:?}");
        assert!(report[1].contains("re-run `arena pods setup solo-gpu devtest-echo`") && report[1].contains("no SSH endpoint"), "{report:?}");
        assert_eq!(report[2], "⚠ keys: devtest-echo already has its own OpenRouter key");
        assert_eq!(rename_report(&[], &[]).len(), 1, "just the MACHINE_NAME note");
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
        handle_terminate(&f, &cfg, &book, Some("alpha"), true, true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-alpha"]);
        // Every key named after the machine is revoked; its row dropped, the others kept.
        assert_eq!(keys.calls(), ["delete h-a", "delete h-a2"]);
        assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
        // A machine with no key and no row: terminated, nothing to revoke — not a failure.
        handle_terminate(&f, &cfg, &book, Some("charlie"), true, true, false, true).await.unwrap();
        assert_eq!(f.calls(), ["terminate r-alpha", "terminate r-charlie"]);
        assert_eq!(keys.calls().len(), 2);
        // A dry run touches nothing, on either side.
        handle_terminate(&f, &cfg, &book, Some("bravo"), true, true, true, true).await.unwrap();
        assert_eq!((f.calls().len(), keys.calls().len()), (2, 2));
        // Without --revoke-key the key and row stay.
        handle_terminate(&f, &cfg, &book, Some("bravo"), false, true, false, true).await.unwrap();
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
        let e = handle_terminate(&f, &cfg, &book, None, true, false, false, true).await.unwrap_err().to_string();
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
        let e = handle_terminate(&f, &cfg(&dir.0), &no_api(&dir.0.join("k.csv")), Some("alpha"), true, true, false, true)
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
        // Setup ran again on alpha (and only alpha): deploy key, then the config command —
        // which writes ~/.name again.
        let calls = remote.calls();
        assert!(calls.iter().all(|c| c.host() == "10.0.0.1:22001"), "{calls:?}");
        assert!(matches!(&calls[0], RemoteCall::Copy { remote, .. } if remote == "/root/.ssh/id_ed25519"), "{calls:?}");
        assert!(matches!(&calls[1], RemoteCall::Exec { cmd, .. } if cmd.contains(&name_file_command("alpha"))), "{calls:?}");
        // …and the proxy was synced afterwards.
        assert!(proxy_file(&dir.0).contains("name=devtest-alpha"));
    }

    #[tokio::test]
    async fn restart_needs_no_wipe_ok_when_something_survives() {
        let dir = tmpdir("restart-volume");
        let cfg = cfg(&dir.0);
        let keys_dir = dir.0.display().to_string();
        // A pod with a volume: its /workspace survives, so no --wipe-ok — still re-set-up.
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir).await.unwrap();
        assert_eq!(f.calls(), ["restart r-bravo"]);
        assert!(!remote.calls_to("10.0.0.1:22002").is_empty(), "setup ran");
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

    #[tokio::test]
    async fn a_restart_whose_setup_fails_says_not_to_restart_again() {
        let dir = tmpdir("restart-setup-fails");
        let cfg = cfg(&dir.0);
        let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
        remote.script("10.0.0.1:22001", [FakeReply::ok(), FakeReply::exit(1, "fatal: not a git repository")]);
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

    #[test]
    fn disk_fate_text_and_gate() {
        let cases: &[(DiskFate, bool, &[&str])] = &[
            (DiskFate::Kept, false, &["disk kept"]),
            (DiskFate::Wiped { volume_gb: Some(50) }, false, &["WIPES the container disk", "everything outside /workspace is lost", "50 GB volume at /workspace survives"]),
            (DiskFate::Wiped { volume_gb: Some(0) }, true, &["WIPES the container disk", "NO persistent volume", "~/.name"]),
            (DiskFate::Wiped { volume_gb: None }, true, &["couldn't read whether it has a volume", "assume nothing survives"]),
        ];
        for (fate, gated, says) in cases {
            assert_eq!(fate.needs_wipe_ok(), *gated, "{fate:?}");
            let text = fate.describe(DiskOp::Restart);
            for s in *says {
                assert!(text.contains(s), "{fate:?}: {text}");
            }
        }
        assert!(DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Stop).contains("starts again as a fresh image"));
        assert!(DiskFate::Kept.describe(DiskOp::Stop).contains("powered off"));
        let r = wipe_refusal(DiskOp::Restart, &["devtest-a", "devtest-b"]);
        assert!(r.starts_with("refusing to restart devtest-a, devtest-b without --wipe-ok"), "{r}");
        assert!(r.contains("`arena pods backup devtest-a devtest-b`") && r.contains("--yes alone isn't enough"), "{r}");
        assert!(wipe_refusal(DiskOp::Stop, &["x"]).contains("terminate instead"));
    }

    fn pods_cmd(argv: &[&str]) -> PodCmd {
        match Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).map(|c| c.cmd) {
            Ok(Cmd::Pods(p)) => p,
            other => panic!("{argv:?}: {:?}", other.err()),
        }
    }

    #[test]
    fn the_new_flags_parse() {
        assert!(matches!(
            pods_cmd(&["restart", "alpha", "--wipe-ok", "--no-setup", "--skip-proxy"]),
            PodCmd::Restart { wipe_ok: true, no_setup: true, skip_proxy: true, dry_run: false, .. }
        ));
        assert!(matches!(pods_cmd(&["restart", "alpha"]), PodCmd::Restart { wipe_ok: false, no_setup: false, .. }));
        assert!(matches!(pods_cmd(&["stop", "alpha", "--wipe-ok"]), PodCmd::Stop { wipe_ok: true, .. }));
        assert!(matches!(pods_cmd(&["terminate", "alpha", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: false, .. }));
        assert!(matches!(pods_cmd(&["terminate", "--all", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: true, .. }));
    }
}
