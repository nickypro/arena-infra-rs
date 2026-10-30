//! The clap command tree: `Cli`, `Cmd`, `PodCmd` and the target-selection flags fleet commands share
//! (a command family's own subcommands live with the family: `cron::CronCmd`, `keys::KeysCmd`, …).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use arena_core::selector::SelectArgs;
use arena_core::Config;
use clap::{Parser, Subcommand};

use crate::cron::CronCmd;
use crate::keys::KeysCmd;
use crate::pods::RUN_TIMEOUT_SECS;
use crate::{archive, image, jobs, money, offers, parse_max_price, reap, spend, ConfigCmd, MigrateCmd, PlanCmd, ProxyCmd};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

#[derive(Parser)]
#[command(
    name = "arena",
    version = concat!(env!("ARENA_VERSION"), " (", env!("ARENA_COMMIT"), ")"),
    about = "Streamlined ARENA infra control plane",
    // Cisco-style shorthands: any unambiguous prefix works (`arena po l` == `pods
    // list`). Ambiguous prefixes (e.g. `p` for pods/proxy) error and ask you to
    // disambiguate. Applies at each level.
    infer_subcommands = true
)]
pub(crate) struct Cli {
    /// Path to config.env
    #[arg(long, default_value = DEFAULT_CONFIG, global = true)]
    pub config: PathBuf,

    /// Provider: runpod, vast, hetzner or shadeform
    #[arg(long, default_value = "runpod", global = true)]
    pub provider: String,

    /// Skip the "Proceed? [y/N]" prompt (required when there's no terminal)
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

// Parsed once per run: `pods`' many flags make it the largest variant by far, and boxing it
// would only cost every `Cmd::Pods(PodCmd::…)` match a deref.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Pods: create, set up, run, back up, stop, terminate
    #[command(subcommand, infer_subcommands = true)]
    Pods(PodCmd),
    /// Open the interactive dashboard (arena-tui)
    Tui,
    /// Install the newest release, after saying what changed since this build
    ///
    /// Reads the latest GitHub release and shows its changelog entries this build lacks, the
    /// "action needed" and config lines first. Then it downloads `arena` and `arena-tui` for
    /// this platform, checks them against the release's SHA256SUMS and their build-provenance
    /// attestations (Sigstore: built by this repository's release.yml from master), checks the
    /// new `arena` runs here, asks, and swaps both in where this binary is — the old ones kept
    /// as `arena.prev` / `arena-tui.prev`. Dashboards still on the old build are listed.
    /// Any check that fails installs nothing. Needs no config and no GitHub login.
    #[command(verbatim_doc_comment)]
    Update {
        /// Only say whether a newer release exists and what it changes (exit 10 if one does)
        #[arg(long)]
        check: bool,
        /// Download and check the new binaries, attestations included, but install nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Do NOT check build provenance: install only binaries with these SHA-256s, checked by hand
        ///
        /// arena's and arena-tui's, for this platform, as `gh attestation verify` checked them
        /// elsewhere: only for a release this build can't verify (say, after Sigstore rotated
        /// its keys). How: docs/REFERENCE.md.
        #[arg(long, value_name = "SHA256,SHA256", value_delimiter = ',')]
        skip_attestation: Vec<String>,
    },
    /// SSH proxy forwards: preview (plan) or write + reload (apply)
    #[command(subcommand, infer_subcommands = true)]
    Proxy(ProxyCmd),
    /// Check or preview a provisioning plan file (read-only)
    #[command(subcommand, infer_subcommands = true)]
    Plan(PlanCmd),
    /// Check, show or set config.env keys
    #[command(subcommand, infer_subcommands = true)]
    Config(ConfigCmd),
    /// Install, remove or show arena's crontab lines
    ///
    /// Edits your crontab; only arena-managed lines are touched.
    #[command(subcommand, infer_subcommands = true)]
    Cron(CronCmd),
    /// Manage the cohort's OpenRouter API keys
    ///
    /// Needs OPENROUTER_PROVISIONING_KEY in the config.
    #[command(subcommand, infer_subcommands = true)]
    Keys(KeysCmd),
    /// RunPod's GPU types as before (`offers runpod`: per tier)
    ///
    /// Live from RunPod where available, else the local presets. Hidden: `offers` covers it.
    #[command(hide = true)]
    Gpus {
        /// JSON instead of a table (also lists the uncreatable entries the table hides)
        #[arg(long)]
        json: bool,
    },
    /// What create/up would try, with prices; or everything a provider sells
    ///
    /// Read-only. `arena offers`: each configured provider's options create/up would try
    /// (Shadeform only with --provider), # = their order. PER POD is $/h (~ = estimate;
    /// Hetzner in €); STOCK is a hint. -v adds the notes. --gpu, --cloud, --max-price and
    /// --server-type preview another setup.
    ///
    /// `arena offers runpod|vast|hetzner|shadeform|all`: everything on sale. RunPod: every
    /// type per tier, MIG slices and `unlisted` types (seen on pods, RUNPOD_EXTRA_GPU_TYPES)
    /// included. Vast: the cheapest offer per card and host class (datacenter, verified,
    /// unverified). Hetzner: server types by family. # = what up would try; flags filter.
    #[command(verbatim_doc_comment)]
    Offers {
        /// runpod, vast, hetzner, shadeform or all: everything on sale there (default: what up
        /// would try)
        #[arg(value_name = "PROVIDER", value_parser = clap::builder::PossibleValuesParser::new(offers::VIEWS), hide_possible_values = true)]
        view: Option<String>,
        /// GPU type(s), comma-separated, e.g. `A4000,3090` (default: config GPU_TYPE)
        #[arg(long)]
        gpu: Option<String>,
        /// Tiers: RunPod community,secure; Vast datacenter,verified,unverified (default: CLOUD_TYPE)
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod: options above it, or with no known price, are dropped.
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// GPUs per pod (default: config NUM_GPUS) — prices and the cap are per pod.
        #[arg(long)]
        gpus: Option<u32>,
        /// Hetzner only: server type, e.g. cx23 or cpx42 (overrides HETZNER_SERVER_TYPE)
        #[arg(long, value_name = "TYPE")]
        server_type: Option<String>,
        /// `cheapest` or `listed`
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
        /// `offers all`'s old spelling
        #[arg(long, hide = true, conflicts_with = "view")]
        all: bool,
        /// The notes under the table too
        #[arg(long, short = 'v')]
        verbose: bool,
        /// JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Print the ~/.ssh/config for pods, or keep yours current (--install)
    ///
    /// --install writes PATH (default SSH_CONFIG_FILE, else ~/.ssh/arena.conf) and makes
    /// `Include PATH` the first line of ~/.ssh/config (shows the change, asks, keeps a copy of
    /// the old file). With SSH_CONFIG_FILE set, every pod change and `pods list` rewrite PATH
    /// when an endpoint moved. --uninstall takes the Include line out again.
    ///
    /// --admin: staff's config on the proxy box instead — every machine (cohort pods and @
    /// staff boxes) at its live endpoint with the admin key (ADMIN_SSH_KEY_PATH) alone. With
    /// --install: PATH defaults to SSH_ADMIN_CONFIG_FILE, else ~/.ssh/arena-admin.conf, kept
    /// current like the participants' file once SSH_ADMIN_CONFIG_FILE is set.
    #[command(verbatim_doc_comment, group(clap::ArgGroup::new("setup").args(["install", "uninstall"])))]
    SshConfig {
        /// Use the proxy layout (stable `SSH_PROXY_*` ports) instead of direct pod IPs.
        #[arg(long, conflicts_with_all = ["install", "uninstall", "admin"])]
        proxy: bool,
        /// Staff's config (every machine, live endpoints, the admin key) — for the proxy box.
        #[arg(long)]
        admin: bool,
        /// Also write the rendered config to this local path (else just prints it).
        #[arg(long, conflicts_with_all = ["install", "uninstall"])]
        out: Option<PathBuf>,
        /// Keep PATH current and Include it from ~/.ssh/config (one-time setup)
        #[arg(long, value_name = "PATH", num_args = 0..=1, conflicts_with = "uninstall")]
        install: Option<Option<String>>,
        /// Take the Include line --install added out of ~/.ssh/config (PATH stays)
        #[arg(long, value_name = "PATH", num_args = 0..=1)]
        uninstall: Option<Option<String>>,
        /// With --install/--uninstall: print what would change, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"], requires = "setup")]
        dry_run: bool,
    },
    /// End-of-program check that nothing is left billing (read-only)
    ///
    /// Checks every pod on every provider (a stopped pod still bills its disk), RunPod network
    /// volumes, arena cron lines and `at` jobs, the cohort's enabled OpenRouter keys and the
    /// local proxy forwards. Prints a ✓/✗/? checklist with the command that cleans up each item;
    /// deletes nothing. Exits non-zero unless all is clear (`?` = couldn't be read, never
    /// "empty").
    #[command(verbatim_doc_comment)]
    Teardown {
        /// Run the check (required: teardown itself deletes nothing)
        #[arg(long)]
        check: bool,
        /// Print the checklist as JSON (for scripts); the exit status is the same.
        #[arg(long)]
        json: bool,
    },
    /// Prepaid balance, burn rate and runway per provider account
    ///
    /// At $0 RunPod terminates pods without a network volume and Vast stops every instance;
    /// Hetzner is postpaid. A balance is live, est. (the last live reading minus spend since;
    /// errs low) or `?`. Critical under 24 h, low under BALANCE_WARN_HOURS. Reads the accounts
    /// and records the reading locally; changes nothing else. Exits 0 unless --check.
    #[command(verbatim_doc_comment)]
    #[command(args_conflicts_with_subcommands = true)]
    Balance {
        #[command(subcommand)]
        cmd: Option<BalanceCmd>,
        /// Print the accounts as JSON (for scripts).
        #[arg(long)]
        json: bool,
        /// Exit 2 if any account is critical, 3 if unknown while burning, 1 if low, else 0
        #[arg(long)]
        check: bool,
        /// No live read: show only the fallback estimate (writes nothing)
        #[arg(long)]
        no_live: bool,
        /// Also print the burn's parts, spend limit, billing history and last failures
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// What each machine instance cost and who it was for (compute + AI)
    ///
    /// For organisers. One row per owner (`pods up/create --owner`), then "(no owner)" (a
    /// forgotten --owner shows there) and the totals; currencies are never summed. --by machine:
    /// one row per instance — a name created again is a new row. Lists the fleet and the AI
    /// keys, reads RunPod's per-pod billing, records what it saw in the local ledger
    /// (spend.json, which pods list, the dashboard and the snapshot cron keep current) and
    /// reports from it; ~ marks an estimate, the notes say which way it errs. Changes nothing
    /// else. `spend assign <machine|instance id|key hash> <owner>` sets or fixes an owner.
    #[command(verbatim_doc_comment)]
    #[command(args_conflicts_with_subcommands = true)]
    Spend {
        #[command(subcommand)]
        cmd: Option<spend::SpendCmd>,
        /// One row per `owner` (default) or per `machine` instance
        #[arg(long, value_name = "WHAT", value_parser = clap::builder::PossibleValuesParser::new(spend::BY), default_value = "owner")]
        by: String,
        /// Only what was spent from this UTC date on (YYYY-MM-DD, or an RFC 3339 time)
        #[arg(long, value_name = "DATE", value_parser = arena_core::spend::parse_since)]
        since: Option<u64>,
        /// CSV instead of a table
        #[arg(long, conflicts_with = "json")]
        csv: bool,
        /// The whole report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Read-only GET passthrough to a provider's REST API
    #[command(subcommand, infer_subcommands = true)]
    Api(ApiCmd),
    /// Pre-built Hetzner images: bake, list, prune (HETZNER_IMAGE=arena)
    ///
    /// A Hetzner VM from Ubuntu takes ~10 min of setup to build the env; from an arena image,
    /// about one. `build` bakes one in this project, `list` shows them and which one
    /// HETZNER_IMAGE=arena (or `--image arena` on pods create/up) picks, `prune` deletes old ones.
    #[command(verbatim_doc_comment)]
    #[command(subcommand, infer_subcommands = true)]
    Image(image::ImageCmd),
    /// The backup archive VM (second copy of cohort backups)
    ///
    /// An always-on Hetzner VM with two volumes (dated snapshots, all files), filled from
    /// LOCAL_BACKUP_DIR by cron. Participants can read it over sftp once you turn access on.
    /// Never part of the fleet.
    #[command(verbatim_doc_comment)]
    #[command(subcommand, infer_subcommands = true)]
    Archive(archive::ArchiveCmd),
    /// Read-only fleet overview: pods, SSH, proxy, last health check
    ///
    /// `pods list`'s columns plus whether each cohort machine's SSH port answers, its proxy port
    /// and its last `pods test --deep` / `up --check` verdict. Only TCP-probes the SSH port (no
    /// login, no command); never runs a check, changes nothing.
    #[command(verbatim_doc_comment)]
    Snapshot {
        /// Full snapshot as JSON (ids, endpoints, costs): for scripts, never for publishing
        #[arg(long)]
        json: bool,
        /// Public JSON for web/fleet.html (cohort machines only; no IPs, ids or costs)
        #[arg(long)]
        public: bool,
        /// Write to FILE (atomically) instead of stdout
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Skip the SSH-port probe (a running pod with an endpoint counts as up)
        #[arg(long)]
        no_probe: bool,
    },
}

// ---- Target selection (PLAN 1.C) --------------------------------------------------------
//
// Every fleet command picks its pods with the same syntax, parsed and resolved by
// `arena_core::selector`: names (`apple`, `arena8-apple`, `@james-gpu`), provider ids,
// ranges `apple..mayor`, `all`/`--all`, filtered by `--exclude`, `--gpus N`, `--on
// <provider>`. Typos fail loudly. The flags live in one struct (`SelectOpts`); the targets
// themselves are positional (`Select`) except where a command's positionals are already
// taken — `run`'s command, `cp`'s file/dest — which take `-t/--target` (`SelectByFlag`).
// (`pull` took its label positionally and its pods by `-t` until a live `pods pull bravo`
// meant "every pod, into backup/bravo/"; it is positional like the rest now — see
// `pull_label_and_targets`.) Plain `//` comments here: a doc comment on a flattened struct can leak
// into the subcommand's help text. Each command flattens its selection as its *last* field:
// the "Target selection" help heading carries over to every arg declared after it.

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
pub(crate) struct SelectOpts {
    /// Every pod (narrow it with --exclude / --gpus / --on).
    #[arg(long)]
    all: bool,
    /// Leave these pods out (same syntax as targets; an unknown name is an error)
    #[arg(long, value_name = "TARGET")]
    pub exclude: Vec<String>,
    /// Only pods with exactly N GPUs
    #[arg(long, value_name = "N")]
    gpus: Option<u32>,
    /// Only pods on this provider (--provider is where new pods go)
    #[arg(long, value_name = "PROVIDER")]
    on: Option<String>,
    /// Old spelling of the targets (with --all: only these)
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

/// Who a fleet-wide backup command (`backup`, `pull`, `init-branches`, `set-branch --all`) —
/// or `ask`/`test`, which reach into pods too — acts on when it names no pod: this cohort's
/// ([`split_cohort`]), or with the flag every pod.
#[derive(clap::Args, Debug, Clone, Copy, Default)]
pub(crate) struct CohortScope {
    /// With no pods named (or --all): every pod, staff boxes and other prefixes too
    #[arg(long)]
    pub include_non_cohort: bool,
}

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
pub(crate) struct Select {
    /// Pods: names (`apple`, `@box`), ids, ranges (`apple..mayor`) or `all`
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,
    #[command(flatten)]
    pub opts: SelectOpts,
}

impl Select {
    pub(crate) fn args(&self) -> SelectArgs {
        self.opts.args(&self.targets)
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target selection")]
pub(crate) struct SelectByFlag {
    /// Only these pods: names, ids, ranges or `all` (default: every pod)
    #[arg(long = "target", short = 't', value_name = "TARGET")]
    pub targets: Vec<String>,
    #[command(flatten)]
    opts: SelectOpts,
}

impl SelectByFlag {
    pub(crate) fn args(&self) -> SelectArgs {
        self.opts.args(&self.targets)
    }
}

/// `arena balance set`.
#[derive(Subcommand)]
pub(crate) enum BalanceCmd {
    /// Record a balance read off the provider's console (USD)
    ///
    /// For when every balance API is down: estimates count down from it until the next live
    /// reading. A local write for the configured key's account; no confirm.
    #[command(verbatim_doc_comment)]
    Set {
        /// `runpod`, `vast` or `shadeform` (Hetzner is postpaid: nothing to set).
        #[arg(value_parser = parse_balance_account)]
        account: arena_core::balance::Account,
        /// The balance in USD, e.g. 250 or 31.20 (may be negative).
        #[arg(value_parser = arena_core::balance::parse_amount, allow_negative_numbers = true)]
        usd: f64,
    },
}

/// `pods ask --ai`: claude or codex.
fn parse_ai(s: &str) -> std::result::Result<arena_core::ask::Ai, String> {
    arena_core::ask::Ai::parse(s).ok_or_else(|| format!("`{s}` isn't claude or codex"))
}

/// `pods ask --effort`: low … max.
fn parse_effort(s: &str) -> std::result::Result<arena_core::ask::Effort, String> {
    arena_core::ask::Effort::parse(s).ok_or_else(|| format!("`{s}` isn't low, medium, high, xhigh or max"))
}

/// `balance set`'s account: a prepaid one.
fn parse_balance_account(s: &str) -> std::result::Result<arena_core::balance::Account, String> {
    arena_core::balance::Account::from_name(s).ok_or_else(|| {
        if s.trim().eq_ignore_ascii_case("hetzner") {
            "hetzner is postpaid — there is no balance to set".to_string()
        } else {
            format!("`{s}` isn't a prepaid account (runpod, vast or shadeform)")
        }
    })
}

#[derive(Subcommand)]
pub(crate) enum ApiCmd {
    /// GET one API path and print the JSON (read-only)
    ///
    /// PATH is relative to the API base, e.g. `pods`, `billing/pods` (RunPod v2), `instances/`
    /// (Vast), `servers` (Hetzner), `instances/types` (Shadeform). Absolute URLs and `..` are
    /// refused, so the key only ever goes to the provider. Secrets in the response are redacted
    /// unless --raw; the API key never prints.
    #[command(verbatim_doc_comment)]
    Get {
        /// runpod (follows RUNPOD_API), runpod-v1, runpod-v2, vast, hetzner or shadeform
        provider: String,
        /// Path under the provider's API base, e.g. `pods` (a query string is fine).
        path: String,
        /// Don't redact secrets in the body (the API key itself never prints)
        #[arg(long)]
        raw: bool,
        /// Print the API's rate limits for this request instead of the body
        #[arg(long, conflicts_with = "raw")]
        limits: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum PodCmd {
    /// List current pods (read-only).
    List {
        /// JSON instead of a table (adds GPU count, cost, maintenance window)
        #[arg(long)]
        json: bool,
        /// Probe GPUs over SSH even with --json (which skips it otherwise)
        #[arg(long)]
        probe: bool,
        /// Skip the GPU probe over SSH (faster; the GPU column may show `-`)
        #[arg(long)]
        no_probe: bool,
    },
    /// Create pods: by name, up to `-n` in total, or `-a` more
    ///
    /// Tries the --gpu/--cloud options one create at a time until one has capacity, then syncs
    /// the proxy. `pods up` also sets the new pods up.
    #[command(verbatim_doc_comment)]
    Create {
        /// Machine names (`apple bloom`); bare names get the prefix. Not with -n/-a
        names: Vec<String>,
        /// Top up to N pods ON --provider; other providers don't count (not with -a)
        #[arg(short = 'n', long)]
        count: Option<usize>,
        /// Add N pods on --provider (not with -n)
        #[arg(short = 'a', long)]
        add: Option<usize>,
        /// GPU type, or a comma list of fallbacks (`A4000,3090`); see `arena offers`
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// Hetzner only: server type, e.g. cx23 or cpx42 (overrides HETZNER_SERVER_TYPE)
        #[arg(long, value_name = "TYPE")]
        server_type: Option<String>,
        /// Tier: RunPod COMMUNITY/SECURE, Vast datacenter/verified/unverified; a list falls back
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod; options above it or with no known price aren't tried
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// Order to try the --gpu/--cloud options in: `cheapest` or `listed`
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
        /// Container disk size in GB (overrides config DISK_GB).
        #[arg(long)]
        disk: Option<u32>,
        /// Persistent volume size in GB (overrides config VOLUME_GB; default 0).
        #[arg(long)]
        volume: Option<u32>,
        /// Docker image (overrides IMAGE); Hetzner: VM image (arena, a snapshot id, a name)
        #[arg(long)]
        image: Option<String>,
        /// Install + start sshd on boot, for non-arena base images
        #[arg(long)]
        bootstrap: bool,
        /// Extra JSON merged into the create request, e.g. '{"dataCenterIds":["EU-RO-1"]}'
        #[arg(long, value_name = "JSON")]
        api_json: Option<String>,
        #[command(flatten)]
        reap: ReapAtCreate,
        #[command(flatten)]
        owner: spend::OwnerArgs,
        /// Don't sync the proxy afterwards
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// On no capacity, wait and retry (one GPU/cloud only; else use --retry-mins)
        #[arg(long)]
        keep_trying: bool,
        /// Keep retrying for up to this many minutes while capacity is short (0 = once)
        #[arg(long, default_value_t = 0)]
        retry_mins: u64,
        /// Seconds between retry rounds.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
    },
    /// Create pods and set each up: endpoint, proxy, setup, keys
    ///
    /// Each pod runs its own pipeline (SSH endpoint → proxy → setup → --check → API keys) and
    /// prints READY or FAILED <stage> as soon as it's done; one slow pod never holds up another.
    /// Exits non-zero unless every requested name is READY. A pod that fails is left running
    /// (Shadeform: deleted, since it can't be stopped and bills until then).
    ///
    /// A pod up without an SSH endpoint for --endpoint-timeout, or (RunPod) with no container
    /// at a --timeout of 10+ min, is TERMINATED and its name recreated, up to --check-attempts.
    ///
    /// --check: a pod that FAILs the deep check on its host (driver, CUDA, GPU) is TERMINATED and
    /// its name recreated, up to --check-attempts. A software/setup failure isn't replaced.
    /// Ctrl+C: no new attempts, nothing terminated.
    #[command(verbatim_doc_comment)]
    Up {
        /// Machine names (`apple bloom`); bare names get the prefix. Not with -n/-a
        names: Vec<String>,
        /// Top up to N pods ON --provider; other providers don't count (not with -a)
        #[arg(short = 'n', long)]
        count: Option<usize>,
        /// Add N pods on --provider (not with -n)
        #[arg(short = 'a', long)]
        add: Option<usize>,
        /// GPU type, or a comma list of fallbacks (`A4000,3090`); see `arena offers`
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (overrides config NUM_GPUS).
        #[arg(long)]
        gpus: Option<u32>,
        /// Hetzner only: server type, e.g. cx23 or cpx42 (overrides HETZNER_SERVER_TYPE)
        #[arg(long, value_name = "TYPE")]
        server_type: Option<String>,
        /// Tier: RunPod COMMUNITY/SECURE, Vast datacenter/verified/unverified; a list falls back
        #[arg(long)]
        cloud: Option<String>,
        /// Max $/h per pod; options above it or with no known price aren't tried
        #[arg(long, value_parser = parse_max_price)]
        max_price: Option<f64>,
        /// Order to try the --gpu/--cloud options in: `cheapest` or `listed`
        #[arg(long, default_value = "cheapest")]
        order: arena_core::placement::Order,
        /// Container disk size in GB (overrides config DISK_GB).
        #[arg(long)]
        disk: Option<u32>,
        /// Persistent volume size in GB (overrides config VOLUME_GB; default 0).
        #[arg(long)]
        volume: Option<u32>,
        /// Docker image (overrides IMAGE); Hetzner: VM image (arena, a snapshot id, a name)
        #[arg(long)]
        image: Option<String>,
        /// Install + start sshd on boot, for non-arena base images
        #[arg(long)]
        bootstrap: bool,
        /// Extra JSON merged into the create request, e.g. '{"dataCenterIds":["EU-RO-1"]}'
        #[arg(long, value_name = "JSON")]
        api_json: Option<String>,
        #[command(flatten)]
        reap: ReapAtCreate,
        #[command(flatten)]
        owner: spend::OwnerArgs,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// Don't poll after creating; just print ids (run `proxy plan` later).
        #[arg(long, conflicts_with = "check")]
        no_wait: bool,
        /// On no capacity, wait and retry (one GPU/cloud only; else use --retry-mins)
        #[arg(long)]
        keep_trying: bool,
        /// Keep retrying for up to this many minutes while capacity is short (0 = once)
        #[arg(long, default_value_t = 0)]
        retry_mins: u64,
        /// Seconds between retry rounds.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
        /// Skip setup over SSH (and so the API keys)
        #[arg(long)]
        no_setup: bool,
        /// Deep-check each pod; TERMINATE and recreate any that FAIL on a bad host
        #[arg(long)]
        check: bool,
        /// Placements per name, the first create included (2 = one retry)
        ///
        /// Bounds replacing a pod that FAILs --check on its host (the last one is left running
        /// for a look), and — with or without --check — one that never came up or is up without
        /// an SSH endpoint (such a pod is always terminated).
        #[arg(long, default_value_t = 2, conflicts_with = "no_wait", value_parser = clap::value_parser!(u32).range(1..=10))]
        check_attempts: u32,
        /// Allow machines that earlier `up --check`s marked bad (remembered 72 h)
        #[arg(long)]
        retry_bad_hosts: bool,
        /// Lock each pod once READY (RunPod v2) so it can't be stopped or deleted
        #[arg(long, conflicts_with = "no_wait")]
        lock: bool,
        /// Seconds each pod may take to get an endpoint and answer SSH (else FAILED)
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Minutes a pod may be up without an SSH endpoint before it's replaced [default: 5]
        ///
        /// RunPod: its container running behind RunPod's SSH proxy only (a host with no public
        /// IP) — its image pull doesn't count. Overrides config ENDPOINT_TIMEOUT_MINS.
        // u8: PodCmd stays under clippy's large-variant bound (minutes are 1..=120 anyway).
        #[arg(long, value_name = "MINS", value_parser = clap::value_parser!(u8).range(1..=arena_core::pipeline::MAX_ENDPOINT_TIMEOUT_MINS as i64))]
        endpoint_timeout: Option<u8>,
        /// Seconds between endpoint polls
        #[arg(long, default_value_t = 12)]
        interval: u64,
    },
    /// Provision pods over SSH: keys, repo, tokens, VS Code
    ///
    /// Per pod: deploy key + GitHub SSH config, authorized keys, the ARENA repo (fetch the
    /// default branch, update the current one), submodules, ~/.name, broadcast tokens (HF, Claude
    /// Code), a VS Code server warm-up, then API keys if keys/*_api_keys.csv exist. With a
    /// /workspace volume the repo is moved onto it, so a restart keeps the work. Confirms first;
    /// no targets = every pod.
    #[command(verbatim_doc_comment)]
    #[allow(clippy::doc_lazy_continuation)] // verbatim `--help` text: its layout is the output, not Markdown
    Setup {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// DISCARDS local changes: check out the default branch and hard-reset it
        #[arg(long)]
        force: bool,
        /// Hugging Face token to broadcast (overrides config HF_TOKEN).
        #[arg(long)]
        hf_token: Option<String>,
        /// Claude Code OAuth token to broadcast (overrides config CLAUDE_CODE_OAUTH_TOKEN).
        #[arg(long)]
        cc_token: Option<String>,
        /// Also install zsh, oh-my-zsh, powerlevel10k and dotfiles (for bare images)
        #[arg(long)]
        zsh_install: bool,
        /// Skip the VS Code warm-up (VSCODE_PREINSTALL=0 turns it off everywhere)
        #[arg(long)]
        no_vscode: bool,
        /// Per-pod budget in seconds for the main step (default 300; Hetzner VMs 1800)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: Option<u64>,
        #[command(flatten)]
        sel: Select,
    },
    /// Stop billing pods (RunPod/Vast: WIPES the container disk)
    ///
    /// Stops every selected pod that is billing: running, starting/provisioning/… or in ERROR.
    /// Pods already stopped, or in an unknown state, are skipped. Needs targets or --all.
    ///
    /// On RunPod and Vast only a /workspace volume survives a stop. A pod whose repo isn't on one
    /// needs --wipe-ok (even with --yes): back it up first. Hetzner VMs keep their disk.
    /// `pods start` brings pods back.
    #[command(verbatim_doc_comment)]
    Stop {
        /// Also stop pods whose work would be WIPED (required for them, even with --yes)
        #[arg(long)]
        wipe_ok: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Restart a pod in place (RunPod WIPES its disk; RunPod v1, Vast may)
    ///
    /// On RunPod (REST v2, the default) everything outside a /workspace volume (VOLUME_GB) is
    /// lost (files, ~/.name, keys); RunPod v1 and Vast may lose it, so they're gated the same.
    /// Refused without --wipe-ok unless the pod's repo is on a volume, even with --yes. Then
    /// waits for SSH, re-runs setup and syncs the proxy. Hetzner's reset keeps the disk. A
    /// stopped pod is refused: use `pods start`.
    #[command(verbatim_doc_comment)]
    Restart {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Restart even if work on the container disk may be WIPED (required, even with --yes)
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
    /// Start stopped pods again (and set them up again)
    ///
    /// RunPod/Vast pods come back as a fresh image (only a /workspace volume survived the stop),
    /// so each gets `setup` again and the proxy is synced; Hetzner VMs kept their disk. A start
    /// can be refused when the host has no free GPU: the pod stays stopped.
    #[command(verbatim_doc_comment)]
    Start {
        /// Don't re-run setup (the pods come back as the bare image)
        #[arg(long)]
        no_setup: bool,
        /// Don't sync the proxy afterwards.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Rename a pod (no restart) and re-point the proxy
    ///
    /// `rename <old> <new>`: <new> must be a free MACHINE_NAME_LIST name. `--from-prefix X`
    /// renames every `X-<name>` pod to the current prefix. All renames are checked first. Also
    /// rewrites ~/.name and moves the OpenRouter key mapping. The pod's MACHINE_NAME env var
    /// keeps the old name until a reimage.
    #[command(verbatim_doc_comment)]
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
    /// Lock pods so nothing can stop or delete them (RunPod v2)
    ///
    /// RunPod then refuses stop, restart and terminate from any client, its web console included,
    /// until `pods unlock`; setup, backups, `run` and SSH still work. arena's own stop, restart,
    /// reimage, terminate, replace and migrate refuse a locked pod (`terminate --unlock` lifts
    /// it). Pods on a backend that can't lock make the exit non-zero.
    #[command(verbatim_doc_comment)]
    Lock {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Unlock pods so they can be stopped or terminated again
    Unlock {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Reimage pods in place: new image, WIPES the container disk
    ///
    /// Same host and name, fresh container from --image; nothing is copied over (`replace` keeps
    /// files). Refused without --wipe-ok unless the repo is on a /workspace volume, even with
    /// --yes. Then waits for SSH, re-runs setup and syncs the proxy.
    #[command(verbatim_doc_comment)]
    Reimage {
        /// Image to use (default: RUNPOD_DOCKER_IMAGE from config).
        #[arg(long)]
        image: Option<String>,
        /// Reimage even pods whose work isn't on a volume (required, even with --yes)
        #[arg(long)]
        wipe_ok: bool,
        /// Don't re-run setup on the reimaged pods (they come back as the bare image).
        #[arg(long)]
        no_setup: bool,
        /// Don't wait for endpoints / redeploy the proxy afterwards.
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Move a pod to a fresh host, keeping its name and files
    ///
    /// Builds a new pod, copies the files over, then swaps it into the name so its proxy port and
    /// ssh-config carry over: for when the host is the problem (`restart` stays on it). The old
    /// pod is parked as `<name>-old`, then terminated unless --keep-old. Same spec as the source
    /// unless overridden.
    #[command(verbatim_doc_comment)]
    Replace {
        /// Machine name (e.g. arena8-apple) or raw provider id to replace.
        target: String,
        /// GPU type, or a comma list of fallbacks (default: same as the source)
        #[arg(long)]
        gpu: Option<String>,
        /// GPUs per pod (default: same as the source).
        #[arg(long)]
        gpus: Option<u32>,
        /// RunPod tier COMMUNITY/SECURE, or a comma list (default: same as the source)
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
        #[command(flatten)]
        place: PlaceFlags,
        /// Keep the parked `<name>-old` pod instead of terminating it at the end
        #[arg(long)]
        keep_old: bool,
        /// Skip the final `proxy apply` (re-point the proxy yourself later)
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print the full plan, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Staged move to a fresh host: copy, test, cut over (safer)
    ///
    /// The participant keeps working and can test the new pod before the cutover, which is
    /// verified and reverts itself if it breaks: `copy` (repeatable) → `cutover` → `finish` (or
    /// `revert`); `status` shows where a pair is.
    #[command(verbatim_doc_comment)]
    Migrate {
        #[command(subcommand)]
        cmd: MigrateCmd,
    },
    /// Terminate (delete) a pod, or every pod with --all
    ///
    /// IRREVERSIBLE: the pod and its disk are gone. One pod at a time (no ranges or lists); a
    /// name two pods share is refused (pass the id).
    #[command(verbatim_doc_comment)]
    Terminate {
        /// Machine name (e.g. arena8-apple) or raw provider id. Omit with --all.
        target: Option<String>,
        /// Terminate EVERY pod the provider reports (the whole fleet)
        #[arg(long, conflicts_with = "target")]
        all: bool,
        /// Unlock locked pods, then terminate them (else a locked pod is refused)
        ///
        /// With --all, only the cohort's: a locked staff/off-list box is kept and the exit fails.
        #[arg(long)]
        unlock: bool,
        /// Also revoke each terminated machine's OpenRouter key and drop its CSV row
        #[arg(long)]
        revoke_key: bool,
        /// Don't sync the proxy afterwards (the forward stays until `proxy apply`)
        #[arg(long)]
        skip_proxy: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Git-push each pod's branch, then rsync its home locally
    ///
    /// Pushes the current branch (never main/master), then runs `pods pull`; --no-pull = git
    /// only. No targets (or --all) = this cohort's reachable pods (`<prefix>-*`, not @ staff
    /// entries); name a staff box to include it, or --include-non-cohort for every pod.
    /// A pod mid-merge is copied instead; one left alone BUSY_AUTO_RESOLVE_HOURS (3) is
    /// finished, per file and hunk by the side changed last, then backed up.
    #[command(verbatim_doc_comment)]
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
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Rsync each pod's home to the local backup folder
    ///
    /// Writes a dated snapshot (files under --max-size) and the complete `big/` mirror. No
    /// targets (or --all) = this cohort's pods with an SSH endpoint (not @ staff entries;
    /// --include-non-cohort: every pod).
    /// Each pod's rsync is time-limited (BACKUP_TIMEOUT_SECS).
    #[command(verbatim_doc_comment)]
    Pull {
        /// Snapshot label, e.g. `w1d3` (default: the computed wNdM)
        #[arg(long)]
        label: Option<String>,
        /// Local backup dir (default: config LOCAL_BACKUP_DIR, else ./backup)
        #[arg(long)]
        dir: Option<String>,
        /// Max file size in the dated snapshot (default: BACKUP_MAX_SIZE, else 50M)
        #[arg(long)]
        max_size: Option<String>,
        /// Path under the home dir to pull (default: BACKUP_REMOTE_PATH, else all)
        #[arg(long)]
        remote_path: Option<String>,
        /// Exclude `.git` (backed up by default)
        #[arg(long)]
        no_git: bool,
        /// Skip the all-files `big/` mirror; write only the dated snapshot
        #[arg(long)]
        no_big: bool,
        /// Preview only: print the rsync commands, copy nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// The old spelling of the targets (`pods pull -t bravo`), still accepted.
        #[arg(long = "target", short = 't', value_name = "TARGET", hide = true)]
        target_flag: Vec<String>,
        #[command(flatten)]
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Capture the week's shared baseline from one fresh pod
    ///
    /// Saves the image's own files (older than the pod's container) so every pod's dated
    /// snapshots hard-link to them instead of copying. Run it once a week, or when the image
    /// changes, on a pod `up` just made. Refused if the container is over a day old or the week
    /// already has a baseline (--replace moves it aside; nothing is deleted).
    #[command(verbatim_doc_comment)]
    Baseline {
        /// The pod: machine name (e.g. arena8-apple), bare name, @name or provider id.
        target: String,
        /// The week it is for (default: the current one, from ARENA_START_DATE).
        #[arg(long)]
        week: Option<u32>,
        /// Local backup dir (default: config LOCAL_BACKUP_DIR, else ./backup)
        #[arg(long)]
        dir: Option<String>,
        /// Capture again; the week's current baseline is moved aside (nothing deleted)
        #[arg(long)]
        replace: bool,
        /// Preview only: ask the pod (read-only) and print the plan and the rsync commands.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Push a local backup back onto one pod (never deletes)
    ///
    /// Default source: the pod's newest wNdM snapshot (--from big: every file). Files on the pod
    /// newer than the backup's copy stay (--overwrite-newer replaces them); any file it replaces
    /// is kept under .arena-restore/<time>/. Never pushes ~/.ssh, shell rc and history files,
    /// ~/.name or (without --with-git) .git. Confirms first, showing source, destination and
    /// size.
    #[command(verbatim_doc_comment)]
    Restore {
        /// Machine name (e.g. arena8-apple) or raw provider id.
        target: String,
        /// Snapshot label (e.g. `w1d3`) or `big` (default: the newest snapshot)
        #[arg(long)]
        from: Option<String>,
        /// Restore only this path in the backup, e.g. ARENA_materials/chapter1
        #[arg(long)]
        path: Option<String>,
        /// Local backup dir (default: config LOCAL_BACKUP_DIR, else ./backup)
        #[arg(long)]
        dir: Option<String>,
        /// Budget in seconds for the whole transfer (default 7200)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: Option<u64>,
        /// Also replace pod files NEWER than the backup's copy (replaced ones are kept)
        #[arg(long)]
        overwrite_newer: bool,
        /// Push .git dirs from a snapshot too (it may lack git packs over its size cap)
        #[arg(long)]
        with_git: bool,
        /// Preview only: print the plan and the rsync command, copy nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Create + push each pod's wNdM autocommit branch
    ///
    /// No targets (or --all) = this cohort's pods (`<prefix>-*`, not @ staff entries;
    /// --include-non-cohort: every pod).
    #[command(verbatim_doc_comment)]
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
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Switch pods' git branch (ff-only; --hard force-resets)
    ///
    /// --all = this cohort's pods (`<prefix>-*`, not @ staff entries); name a staff box to
    /// include it, or --include-non-cohort for every pod. On main/master no backup commits until
    /// `init-branches` (or --init-branches).
    #[command(verbatim_doc_comment)]
    SetBranch {
        /// Branch to check out (e.g. main, or a feature branch).
        branch: String,
        /// DESTRUCTIVE: reset to `origin/<branch>`, dropping local commits/changes; overwrites files it tracks
        #[arg(long)]
        hard: bool,
        /// Then create + push the day branch of each pod that switched, as `init-branches` does
        #[arg(long)]
        init_branches: bool,
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Run a shell command on pods over SSH (in parallel)
    ///
    /// Runs in an interactive shell with the conda env active (CONDA_ENV, default arena-env).
    /// Flags go BEFORE the command; everything after it is the command (quote it if needed:
    /// `run 'ls -t'`). Default: every pod; narrow it with -t. --background returns at once with a
    /// job id (see `pods jobs` / `pods logs`).
    #[command(verbatim_doc_comment)]
    Run {
        /// The command to run (everything after the flags)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        /// Start it detached and return a job id (stop it with `pods jobs --kill`)
        #[arg(long, visible_alias = "bg", conflicts_with = "timeout")]
        background: bool,
        /// Per-pod budget in seconds; a pod over it counts as failed
        #[arg(long, default_value_t = RUN_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: u64,
        /// Preview only: print the command + target pods, run nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: SelectByFlag,
    },
    /// List background jobs on each pod (or --kill one)
    ///
    /// Newest first: id, start time, status, command. Read-only except --kill. A job id among the
    /// targets shows just that job.
    #[command(verbatim_doc_comment)]
    Jobs {
        /// Stop job JOB: SIGTERM to its process group (confirms first)
        #[arg(long, value_name = "JOB")]
        kill: Option<String>,
        #[command(flatten)]
        sel: Select,
    },
    /// Show a background job's log on each pod
    ///
    /// JOB (from `pods jobs`; default: each pod's newest) can go among the targets. --follow
    /// keeps printing until every job has ended and exits non-zero if any didn't exit 0.
    /// Read-only: Ctrl+C stops following, never the job.
    #[command(verbatim_doc_comment)]
    Logs {
        /// Lines to show per pod (from at most the last 1 MiB of its log).
        #[arg(short = 'n', long, default_value_t = jobs::DEFAULT_TAIL, value_parser = clap::value_parser!(u64).range(0..=100_000))]
        tail: u64,
        /// Keep printing until the jobs end (Ctrl+C leaves them running)
        #[arg(short = 'f', long)]
        follow: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Health check: torch version, or --deep GPU tests (read-only)
    ///
    /// --deep: per-GPU CUDA op, driver vs MIN_DRIVER_VERSION, device count, GPU↔GPU copy + NCCL,
    /// HF download speed, disk, load, maintenance window. One pass/warn/fail row per pod; exits
    /// non-zero on any FAIL. No targets (or --all) = this cohort's pods with an SSH endpoint
    /// (not @ staff entries; --include-non-cohort: every pod).
    #[command(verbatim_doc_comment)]
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
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Report pods that look abandoned (read-only; reaps nothing)
    ///
    /// Probes each billing pod: inbound connections (SSH, VS Code, Jupyter), GPU use, newest file
    /// change, uptime. A pod idle by every measure for --hours is a candidate, with the
    /// `pods backup` / `pods terminate` commands printed (not run). Unreadable = unknown, never a
    /// candidate. Not seen: sessions via RunPod's ssh.runpod.io proxy; check with the group
    /// before acting.
    #[command(verbatim_doc_comment)]
    Idle {
        /// Hours a pod must have been idle by every measure
        #[arg(long, default_value_t = arena_core::idle::DEFAULT_HOURS, value_parser = money::parse_hours)]
        hours: f64,
        /// Print the report as JSON (verdicts, readings, suggested commands).
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Ask claude or codex about pods' current state (reads only)
    ///
    /// Reads each pod once over SSH (listing, GPU, disks, logins, top processes, the repo's
    /// working tree, jobs and the newest job's log — commands without their values, keys and
    /// tokens masked) and sends it all in
    /// one prompt to claude (ASK_AI, the default) or codex, with no tools. The answer is printed,
    /// and saved with the prompt under the state dir (<state>/<prefix>/ask/). Name pods, or
    /// --all for this cohort's. Quote the question: `arena pods ask apple "why is it slow?"`.
    /// Each AI's model and effort: ASK_CLAUDE_MODEL/_EFFORT, ASK_CODEX_MODEL/_EFFORT (a small
    /// model at low effort answers in seconds: `--ai codex --model gpt-6-luna --effort low`).
    #[command(verbatim_doc_comment)]
    Ask {
        /// claude or codex (default: ASK_AI, else claude)
        #[arg(long, value_parser = parse_ai)]
        ai: Option<arena_core::ask::Ai>,
        /// The AI's model (default: ASK_CLAUDE_MODEL / ASK_CODEX_MODEL, else its CLI's own)
        #[arg(long)]
        model: Option<String>,
        /// low, medium, high, xhigh or max (default: ASK_CLAUDE_EFFORT / ASK_CODEX_EFFORT, else the CLI's)
        #[arg(long, value_parser = parse_effort)]
        effort: Option<arena_core::ask::Effort>,
        /// Seconds the AI may take to answer
        #[arg(long, default_value_t = arena_core::ask::DEFAULT_TIMEOUT.as_secs(), value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Print the prompt that would be sent; send (and save) nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        /// The question (or a quoted word among the pods; default: their state, anything wrong?)
        #[arg(short = 'q', long)]
        question: Option<String>,
        #[command(flatten)]
        cohort: CohortScope,
        #[command(flatten)]
        sel: Select,
    },
    /// Opt-in auto-reaper for expired or idle pods (off by default)
    ///
    /// Terminates pods past an end time or idle for a window, after a verified backup. Nothing
    /// happens without a policy (`reap set`, or `up --expire-in/--idle-after`), and only
    /// `reap run --apply` with REAP_ENABLED=1 in the config file acts. `reap run` alone just
    /// looks.
    #[command(verbatim_doc_comment)]
    #[command(subcommand, infer_subcommands = true)]
    Reap(reap::ReapCmd),
    /// Copy API keys and tokens into pods' shells
    ///
    /// Per-host keys from `<keys-dir>/<provider>_api_keys.csv`, plus the HF and Claude Code
    /// tokens. No targets = every reachable pod.
    #[command(verbatim_doc_comment)]
    CopyKeys {
        /// Directory holding the per-host `<provider>_api_keys.csv` files.
        #[arg(long, default_value = "./keys")]
        keys_dir: String,
        /// Hugging Face token for every pod (overrides config HF_TOKEN)
        #[arg(long)]
        hf_token: Option<String>,
        /// Claude Code OAuth token for every pod (overrides config CLAUDE_CODE_OAUTH_TOKEN)
        #[arg(long)]
        cc_token: Option<String>,
        /// Preview only: print what would be set per pod (key values redacted).
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Copy a local file or dir (-r) to every pod (scp)
    #[command(visible_alias = "copy")]
    Cp {
        /// Local file (or directory, with -r) to copy.
        file: PathBuf,
        /// Destination path on each pod. Omit to mirror the repo path / land in `~`.
        dest: Option<String>,
        /// Recurse into a directory (scp -r).
        #[arg(short = 'r', long)]
        recursive: bool,
        /// Per-pod scp budget in seconds (default: 10 min + 1 s per MB sent)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=arena_core::setup::MAX_STEP_TIMEOUT_SECS))]
        timeout: Option<u64>,
        /// Preview only: print the scp commands, copy nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: SelectByFlag,
    },
}


/// `up`/`create`'s reap policy flags (`arena pods reap`): resolved ONCE at command start to one
/// absolute end time, recorded for each pod the moment its create returns (`--check`
/// replacements included). Refused before anything is created when the reaper's state dir
/// (ARENA_STATE_DIR, explicit and absolute) isn't usable.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct ReapAtCreate {
    /// Have the opt-in reaper terminate each pod this long from now (`8h`, `3d`)
    #[arg(long, value_name = "DUR", conflicts_with = "expire_at")]
    expire_in: Option<String>,
    /// Have the opt-in reaper terminate each pod at this time (RFC 3339 or local)
    #[arg(long, value_name = "TIME")]
    expire_at: Option<String>,
    /// Have the opt-in reaper terminate each pod after this long fully idle (`8h`)
    #[arg(long, value_name = "DUR")]
    idle_after: Option<String>,
}

impl ReapAtCreate {
    /// The policy and where to record it (`None`: no flag given). Errors before any create —
    /// also when `provider` (where the pods go) is one the reaper never acts on (RR2).
    pub(crate) fn resolve(&self, cfg: &Config, provider: &str) -> Result<Option<(reap::CreatePolicy, arena_core::reap::store::Store)>> {
        let asked = self.expire_in.is_some() || self.expire_at.is_some() || self.idle_after.is_some();
        if let (true, Some(why)) = (asked, reap::create_flags_refusal(cfg, provider)) {
            anyhow::bail!("{why}");
        }
        reap::resolve_create_policy(
            cfg,
            self.expire_in.as_deref(),
            self.expire_at.as_deref(),
            self.idle_after.as_deref(),
            arena_core::snapshot::unix_now(),
            &reap::real::resolve_local,
        )
    }
}

/// Where `replace` / `migrate copy` may land the new pod beyond `--gpu` / `--cloud` (live
/// finding #19: a `--gpu A,B` list went to the API as one invalid id, with no capacity
/// retry): the same placement as `create` / `up` — each option tried one create at a time,
/// cheapest first (or as listed), with a retry window while capacity is short.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct PlaceFlags {
    /// Max $/h for the new pod; options above it or unpriced aren't tried
    #[arg(long, value_parser = parse_max_price)]
    pub max_price: Option<f64>,
    /// Order to try several --gpu/--cloud options in: `cheapest` or `listed`.
    #[arg(long, default_value = "cheapest")]
    pub order: arena_core::placement::Order,
    /// Keep retrying every option for up to this many minutes (0 = once)
    #[arg(long, default_value_t = 0)]
    pub retry_mins: u64,
    /// Seconds between retry rounds.
    #[arg(long, default_value_t = 60)]
    pub retry_secs: u64,
}

impl PlaceFlags {
    /// The retry window these flags ask for (zero = one round).
    pub(crate) fn rounds(&self) -> arena_core::placement::Rounds {
        arena_core::placement::Rounds {
            window: Duration::from_secs(self.retry_mins * 60),
            every: Duration::from_secs(self.retry_secs.max(1)),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
