//! `arena claude`: Claude Code accounts per machine — the pool and who is on which (`accounts`),
//! moving machines (`assign`, `off`) and putting each one's own token on it (`push`) — and the
//! Claude Code stage setup, `up` and `copy-keys` run. The pool, the planners, the state file and the
//! push itself are `arena_core::claude`; here is the wiring: listing the fleet, the confirm, what
//! is printed.

use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::claude::{self, Change, Machine, Outcome, Pool, State, Want, Was};
use arena_core::provider::Provider;
use arena_core::proxy::Listing;
use arena_core::remote::Remote;
use arena_core::selector::{Naming, SelectArgs, Selector};
use arena_core::snapshot::unix_now;
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};
use clap::Subcommand;

use crate::cli::{Select, SelectOpts};
use crate::confirm;
use crate::pods::list::{enrich_best_effort, ENRICH_TIMEOUT};
use crate::pods::selection::{split_cohort, stopped_skip, Selected, Unscoped};

#[derive(Subcommand)]
pub(crate) enum ClaudeCmd {
    /// The account pool: each account's machines and people (read-only)
    ///
    /// The accounts in CLAUDE_TOKENS_FILE (else config's one CLAUDE_CODE_OAUTH_TOKEN), the pods on
    /// each and their spend owners; * marks a pod whose box may not have its account's current
    /// token yet. Never prints a token. Lists the fleet, and forgets machines that are gone.
    #[command(verbatim_doc_comment)]
    Accounts {
        /// The pool and the machines as JSON (for scripts; no tokens)
        #[arg(long)]
        json: bool,
    },
    /// Put pods on an account (number, email or none) now, or --spread them
    ///
    /// `arena claude assign apple bloom 2` pins them to account #2 and pushes its token to them;
    /// `… none` takes their token off; `--spread` puts them on the least-loaded account again.
    /// `--owner NAME ACCOUNT` pins a person's machines (spend owners: `pods up --owner`), now and
    /// later; `--owner NAME --spread` takes the pin off. A stopped pod gets its token when it's set
    /// up again.
    #[command(verbatim_doc_comment)]
    Assign {
        /// The pods (names, ids, ranges), then the account: its number, its email, or `none`
        #[arg(value_name = "TARGET… ACCOUNT")]
        args: Vec<String>,
        /// Spread them (the account with the fewest people first) instead of naming an account
        #[arg(long)]
        spread: bool,
        /// A person's machines (spend owners), now and later, instead of pods
        #[arg(long, value_name = "NAME")]
        owner: Option<String>,
        /// Record the assignment only; `arena claude push` puts the tokens on later
        #[arg(long)]
        no_push: bool,
        /// Preview only: say who moves where, change nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        opts: SelectOpts,
    },
    /// Put each pod's own account's token on it (default: those out of date)
    ///
    /// With no pods named: every cohort pod whose box may not have its account's current token —
    /// never pushed, moved, its token rotated in CLAUDE_TOKENS_FILE, or not on an account yet (it
    /// gets one by spread). Named pods (or --all) are pushed whatever their state. A re-push
    /// changes nothing on a pod already right. The token goes by scp, never in a command.
    #[command(verbatim_doc_comment)]
    Push {
        /// Preview only: say what each pod would get, change nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Take the Claude Code token off pods (they stay off until assigned)
    Off {
        /// Preview only: say what would change, change nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
}

/// `arena claude …`.
pub(crate) async fn handle_claude(cmd: ClaudeCmd, provider: &dyn Provider, remote: Arc<dyn Remote>, cfg: &Config, yes: bool) -> Result<()> {
    let now = unix_now();
    match cmd {
        ClaudeCmd::Accounts { json } => accounts(provider, cfg, json).await,
        ClaudeCmd::Assign { args, spread, owner, no_push, dry_run, opts } => {
            let fleet = Fleet::list(provider, cfg).await?;
            let pool = pool_or_bail(cfg)?;
            let (sel_args, want) = match owner {
                Some(o) => return assign_owner(remote.as_ref(), cfg, &fleet, &pool, &o, &args, spread, no_push, dry_run, yes, now).await,
                None => assign_args(&args, spread, &opts, &pool)?,
            };
            let sel = fleet.select(provider, cfg, &sel_args, Unscoped::Refuse("assign")).await?;
            apply(remote.as_ref(), cfg, &fleet, &pool, &sel, &want, Push::Changed { skip: no_push }, dry_run, yes, now).await
        }
        ClaudeCmd::Push { dry_run, sel } => {
            let fleet = Fleet::list(provider, cfg).await?;
            let pool = pool_or_bail(cfg)?;
            let args = sel.args();
            let picked = fleet.select(provider, cfg, &args, Unscoped::All).await?;
            let (picked, left_out) = split_cohort(&Naming::from_config(cfg), picked);
            if let Some(line) = crate::pods::selection::non_cohort_line(Naming::from_config(cfg).prefix, &left_out) {
                println!("{line}");
            }
            // Nothing named and no --all (filters alone don't scope it, as everywhere).
            let unscoped = Selector::parse(&args, &Naming::from_config(cfg))?.is_unscoped();
            let state = load_state(cfg)?;
            let picked = if unscoped {
                // The default: only the pods that need it.
                let due: Vec<Pod> = picked.pods.into_iter().filter(|p| due(&state, &pool, p)).collect();
                if due.is_empty() {
                    println!("Every cohort pod is up to date with its Claude Code account (name pods, or --all, to push again).");
                    return Ok(());
                }
                Selected { pods: due, named: false }
            } else {
                picked
            };
            apply(remote.as_ref(), cfg, &fleet, &pool, &picked, &Want::Ensure, Push::All, dry_run, yes, now).await
        }
        ClaudeCmd::Off { dry_run, sel } => {
            let fleet = Fleet::list(provider, cfg).await?;
            // Off needs no token: it works with a pool that's broken, or gone (a leak's first step).
            let pool = claude::load_pool(cfg).unwrap_or_else(|_| Pool::empty());
            let picked = fleet.select(provider, cfg, &sel.args(), Unscoped::Refuse("take the Claude Code token off")).await?;
            apply(remote.as_ref(), cfg, &fleet, &pool, &picked, &Want::Pin(None), Push::Changed { skip: false }, dry_run, yes, now).await
        }
    }
}

/// The pool, or why there's none to push from.
fn pool_or_bail(cfg: &Config) -> Result<Pool> {
    let pool = claude::load_pool(cfg).map_err(|e| anyhow::anyhow!(e))?;
    for n in &pool.notes {
        eprintln!("⚠ {n}");
    }
    if pool.is_empty() {
        anyhow::bail!(
            "no Claude Code accounts: set {} to a tokens file (`email -> sk-ant-oat01-…` per line, chmod 600; \
             `claude setup-token` makes each token), or {} for a single one",
            claude::TOKENS_FILE_KEY,
            claude::TOKEN_KEY
        );
    }
    Ok(pool)
}

fn load_state(cfg: &Config) -> Result<State> {
    let path = claude::state_path(cfg).map_err(|e| anyhow::anyhow!("no state dir for claude.json: {e}"))?;
    claude::load(&path).map_err(|e| anyhow::anyhow!(e))
}

/// Whether `pod` needs a push: no assignment, an account no longer in the pool, or a box that may
/// not have its account's current token.
fn due(state: &State, pool: &Pool, pod: &Pod) -> bool {
    match state.of(pod) {
        None => true,
        Some(a) => a.account.as_ref().is_some_and(|e| pool.get(e).is_none()) || claude::out_of_date(a, pool),
    }
}

/// The fleet as one listing: every provider's pods, the providers that failed said, and kept for
/// the selection and to forget machines that are gone.
struct Fleet {
    listing: Listing,
    pods: Vec<Pod>,
    owners: claude::Owners,
}

impl Fleet {
    async fn list(provider: &dyn Provider, cfg: &Config) -> Result<Fleet> {
        let listing = Listing::from_results(provider.list_by_provider().await);
        for (p, e) in listing.errors() {
            eprintln!("⚠ {p}: listing failed ({e}) — its pods aren't here");
        }
        if !listing.any_ok() {
            anyhow::bail!("no provider listed its pods");
        }
        let pods = listing.pods();
        Ok(Fleet { listing, pods, owners: claude::owners_for(cfg) })
    }

    /// Resolve the operator's selection against this listing (as `pods::selection::select` does
    /// against its own).
    async fn select(&self, provider: &dyn Provider, cfg: &Config, args: &SelectArgs, unscoped: Unscoped) -> Result<Selected> {
        let naming = Naming::from_config(cfg);
        let selector = Selector::parse(args, &naming)?;
        if let (true, Unscoped::Refuse(verb)) = (selector.is_unscoped(), unscoped) {
            anyhow::bail!("name the pods to {verb} (names, ids or a range like apple..mayor), or pass --all");
        }
        let mut pods = self.pods.clone();
        if selector.gpus.is_some() {
            if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
                eprintln!("{warning}");
            }
        }
        let picked = selector.resolve(&naming, &pods)?;
        Ok(Selected { pods: picked.into_iter().map(|i| pods[i].clone()).collect(), named: args.names_pods() })
    }

    fn machines(&self, pods: &[Pod]) -> Vec<Machine> {
        pods.iter().map(|p| Machine::new(p, &self.owners)).collect()
    }
}

/// `assign`'s positionals: the targets and what to do. `--spread`: all of them are targets.
/// Otherwise the last is the account (`none` = off), and the rest the targets (none with --all).
fn assign_args(args: &[String], spread: bool, opts: &SelectOpts, pool: &Pool) -> Result<(SelectArgs, Want)> {
    if spread {
        return Ok((opts.args(args), Want::Spread));
    }
    let Some((account, targets)) = args.split_last() else {
        anyhow::bail!("name the pods and the account (`arena claude assign apple bloom 2`), or --spread them");
    };
    let sel = opts.args(targets);
    if !sel.names_pods() && !sel.all {
        anyhow::bail!("name the pods to put on {account} (`arena claude assign apple bloom {account}`), or pass --all");
    }
    Ok((sel, Want::Pin(account_arg(account, pool)?)))
}

/// An account as typed: `none`, its number, or its email.
fn account_arg(s: &str, pool: &Pool) -> Result<Option<String>> {
    if s.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    match pool.find(s) {
        Some(a) => Ok(Some(a.email.clone())),
        None => anyhow::bail!("no account {s} in the pool — there are: {}, or `none`", pool.listed()),
    }
}

/// `#2 bob@x`, or the email of an account no longer in the pool, or `none`.
fn label(pool: &Pool, account: Option<&str>) -> String {
    match account {
        None => "none".into(),
        Some(e) => pool.get(e).map(claude::Account::label).unwrap_or_else(|| format!("{e} (not in the pool)")),
    }
}

/// One change, for the preview: `apple   #1 a@x → #2 b@x  (pinned)`.
fn change_line(pool: &Pool, c: &Change, width: usize) -> String {
    let was = match &c.was {
        Was::Nothing => "no account yet".to_string(),
        Was::Off => "none".to_string(),
        Was::On(e) => label(pool, Some(e)),
    };
    format!("  {:<width$}  {was} → {}  ({})", c.name, label(pool, c.to.as_deref()), c.because())
}

/// Which pods `apply` pushes after the assignment.
#[derive(Debug, Clone, Copy)]
enum Push {
    /// Every target (`claude push`).
    All,
    /// The ones whose box may not have their account's token now (`assign`, `off`); none with
    /// `skip` (`--no-push`).
    Changed { skip: bool },
}

/// Assign `sel` as `want` says, preview, confirm, record, then push the pods `push` says to that
/// can be reached: what `assign`, `off` and `push` do.
#[allow(clippy::too_many_arguments)]
async fn apply(
    remote: &dyn Remote,
    cfg: &Config,
    fleet: &Fleet,
    pool: &Pool,
    sel: &Selected,
    want: &Want,
    push: Push,
    dry_run: bool,
    yes: bool,
    now: u64,
) -> Result<()> {
    let path = claude::state_path(cfg).map_err(|e| anyhow::anyhow!("no state dir for claude.json: {e}"))?;
    let targets = fleet.machines(&sel.pods);
    // The preview, from the state as it is (the record below plans again under the lock).
    let state = claude::prune(load_state(cfg)?, &fleet.listing);
    let (planned, changes) = claude::assign(state, pool, &fleet.owners, &targets, want, now);
    let to_push = |s: &State| -> Vec<Pod> {
        sel.pods
            .iter()
            .filter(|p| match push {
                Push::All => s.of(p).is_some(),
                Push::Changed { skip } => !skip && s.of(p).is_some_and(|a| claude::out_of_date(a, pool)),
            })
            .cloned()
            .collect()
    };
    let width = sel.pods.iter().map(|p| p.name.chars().count()).max().unwrap_or(0);
    if changes.is_empty() {
        println!("No assignment changes ({} pod(s) already so).", sel.pods.len());
    } else {
        println!("Claude Code accounts ({}):", pool.describe());
        for c in &changes {
            println!("{}", change_line(pool, c, width));
        }
    }
    let pushing = to_push(&planned);
    if dry_run {
        for p in &pushing {
            let to = planned.of(p).and_then(|a| a.account.as_deref());
            println!("  would push to {}: {}", p.name, if to.is_some() { label(pool, to) } else { "token taken off".into() });
        }
        println!("Preview only — run without --dry-run to {}.", if pushing.is_empty() { "record it" } else { "record it and push" });
        return Ok(());
    }
    if changes.is_empty() && pushing.is_empty() {
        return Ok(());
    }
    let what = if pushing.is_empty() {
        format!("Will record {} change(s) (no push now: `arena claude push` later)", changes.len())
    } else {
        let names: Vec<&str> = pushing.iter().map(|p| p.name.as_str()).collect();
        format!(
            "Will record {} change(s) and put each pod's own Claude Code token in ~/.bashrc & ~/.zshrc (replacing any \
             there) on {} pod(s): {}",
            changes.len(),
            pushing.len(),
            names.join(", ")
        )
    };
    if !confirm(yes, &what)? {
        println!("aborted.");
        return Ok(());
    }
    let (state, changes) = claude::record(&path, |s| claude::assign(claude::prune(s, &fleet.listing), pool, &fleet.owners, &targets, want, now))
        .map_err(|e| anyhow::anyhow!(e))?;
    let pushing = to_push(&state);
    if pushing.is_empty() {
        println!("Recorded.");
        return Ok(());
    }
    let failed = push_pods(remote, cfg, &path, pool, &state, &pushing, &changes, now).await?;
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) didn't get their Claude Code token");
    }
    Ok(())
}

/// Push `pods` (their SSH targets; a stopped or unreachable one is said and left for its next
/// setup) and print each outcome: how many failed.
#[allow(clippy::too_many_arguments)]
async fn push_pods(remote: &dyn Remote, cfg: &Config, path: &std::path::Path, pool: &Pool, state: &State, pods: &[Pod], changes: &[Change], now: u64) -> Result<usize> {
    let mut reach = Vec::new();
    for p in pods {
        if arena_core::status::is_stopped(&p.status) {
            println!("  {} — its token goes on when it's set up again (`arena pods start` does it)", stopped_skip(p));
            continue;
        }
        match SshTarget::from_pod(p, cfg) {
            Ok(t) => reach.push((p.clone(), t)),
            Err(_) => println!(
                "  skip {} — {} (`arena claude push {}` once it has one)",
                p.name,
                arena_core::fleet::no_endpoint_reason(p).unwrap_or_else(|| "no SSH endpoint yet".into()),
                p.name
            ),
        }
    }
    if reach.is_empty() {
        return Ok(0);
    }
    let outcomes = claude::push_all(remote, path, pool, state, &reach, changes, now).await.map_err(|e| anyhow::anyhow!(e))?;
    Ok(print_outcomes(&outcomes))
}

/// Each outcome on its own line, then the tally: how many failed.
fn print_outcomes(outcomes: &[Outcome]) -> usize {
    let total = outcomes.len();
    let mut failed = 0;
    for (i, o) in outcomes.iter().enumerate() {
        println!("[{}/{total}] {}", i + 1, o.line());
        failed += usize::from(o.result.is_err());
    }
    println!("Done: {} pushed, {failed} failed. (Shells already open keep their old token until they start again.)", total - failed);
    failed
}

/// `claude assign --owner NAME ACCOUNT|none` / `--owner NAME --spread`.
#[allow(clippy::too_many_arguments)]
async fn assign_owner(
    remote: &dyn Remote,
    cfg: &Config,
    fleet: &Fleet,
    pool: &Pool,
    owner: &str,
    args: &[String],
    spread: bool,
    no_push: bool,
    dry_run: bool,
    yes: bool,
    now: u64,
) -> Result<()> {
    let owner = arena_core::spend::parse_owner(owner).map_err(|e| anyhow::anyhow!(e))?;
    let pin = match (spread, args) {
        (true, []) => None,
        (false, [account]) => Some(account_arg(account, pool)?),
        (true, _) => anyhow::bail!("--owner {owner} --spread takes no pods or account"),
        (false, _) => anyhow::bail!("--owner {owner} takes one account: `arena claude assign --owner {owner} 2` (or none, or --spread)"),
    };
    let theirs: Vec<Pod> = fleet
        .pods
        .iter()
        .filter(|p| fleet.owners.get(&(p.provider.clone(), p.id.clone())) == Some(&owner))
        .cloned()
        .collect();
    let machines = fleet.machines(&theirs);
    let path = claude::state_path(cfg).map_err(|e| anyhow::anyhow!("no state dir for claude.json: {e}"))?;
    let shown = match &pin {
        None => "no pin (spread)".to_string(),
        Some(a) => label(pool, a.as_deref()),
    };
    let (planned, changes) = claude::pin_owner(load_state(cfg)?, pool, &fleet.owners, &owner, pin.clone(), &machines, now);
    println!("{owner}: {shown} — for their {} machine(s) now and the ones they get later", theirs.len());
    let width = theirs.iter().map(|p| p.name.chars().count()).max().unwrap_or(0);
    for c in &changes {
        println!("{}", change_line(pool, c, width));
    }
    if theirs.is_empty() {
        println!("  (no machine of theirs is up now: spend knows owners from `pods up --owner` and `arena spend assign`)");
    }
    let pushing: Vec<Pod> =
        if no_push { vec![] } else { theirs.iter().filter(|p| planned.of(p).is_some_and(|a| claude::out_of_date(a, pool))).cloned().collect() };
    if dry_run {
        println!("Preview only — run without --dry-run to record it{}.", if pushing.is_empty() { "" } else { " and push" });
        return Ok(());
    }
    if !confirm(yes, &format!("Will pin {owner} to {shown} and push to {} pod(s)", pushing.len()))? {
        println!("aborted.");
        return Ok(());
    }
    let (state, changes) =
        claude::record(&path, |s| claude::pin_owner(s, pool, &fleet.owners, &owner, pin, &machines, now)).map_err(|e| anyhow::anyhow!(e))?;
    let pushing: Vec<Pod> = pushing.into_iter().filter(|p| state.of(p).is_some_and(|a| claude::out_of_date(a, pool))).collect();
    if pushing.is_empty() {
        println!("Recorded.");
        return Ok(());
    }
    let failed = push_pods(remote, cfg, &path, pool, &state, &pushing, &changes, now).await?;
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) didn't get their Claude Code token");
    }
    Ok(())
}

/// `claude accounts`: the listing, machines that are gone forgotten, the view.
async fn accounts(provider: &dyn Provider, cfg: &Config, json: bool) -> Result<()> {
    let pool = match claude::load_pool(cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ {e}");
            Pool::empty()
        }
    };
    for n in &pool.notes {
        eprintln!("⚠ {n}");
    }
    let fleet = Fleet::list(provider, cfg).await?;
    let path = claude::state_path(cfg).map_err(|e| anyhow::anyhow!("no state dir for claude.json: {e}"))?;
    let (state, ()) = claude::record(&path, |s| (claude::prune(s, &fleet.listing), ())).map_err(|e| anyhow::anyhow!(e))?;
    let machines = fleet.machines(&fleet.pods);
    let v = claude::view(&state, &pool, &machines);
    if json {
        println!("{}", serde_json::to_string_pretty(&v).context("rendering JSON")?);
        return Ok(());
    }
    if pool.is_empty() && state.machines.is_empty() {
        println!(
            "Claude Code accounts: {}.\nSet {} to a tokens file (`email -> sk-ant-oat01-…` per line, chmod 600) for one account \
             per machine; `arena claude push` then spreads them.",
            pool.describe(),
            claude::TOKENS_FILE_KEY
        );
        return Ok(());
    }
    print!("{}", claude::render(&v));
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The stage setup, `up` and `copy-keys` run
// ---------------------------------------------------------------------------------------

/// What setup/up say about Claude Code before they start: the pool (and that each pod gets its
/// own account), `--cc-token` for every pod, or nothing to do. `Err`: the pool can't be read.
pub(crate) fn plan_line(cfg: &Config, token: Option<&str>) -> std::result::Result<Option<String>, String> {
    if token.is_some() {
        return Ok(Some("Claude Code: --cc-token's token on every pod (outside `arena claude`'s accounts)".into()));
    }
    let pool = claude::load_pool(cfg)?;
    Ok((!pool.is_empty()).then(|| format!("Claude Code: {} — each pod on its own account (new ones by spread)", pool.describe())))
}

/// The Claude Code stage for `pods` (each reachable, with its target): the ones with no account get
/// one by spread, then each its own account's token (`token`: `--cc-token`'s, to every pod). Prints
/// a line per pod; nothing when there's no pool. How many failed — a pool or state that can't be
/// read counts every pod.
pub(crate) async fn stage(remote: &dyn Remote, cfg: &Config, pods: &[(Pod, SshTarget)], token: Option<&str>) -> usize {
    match claude::stage(remote, cfg, pods, token, unix_now()).await {
        Ok(None) => 0,
        Ok(Some(outcomes)) => {
            println!("Claude Code (each pod its own account's token):");
            let mut failed = 0;
            for o in &outcomes {
                println!("  {}", o.line());
                failed += usize::from(o.result.is_err());
            }
            failed
        }
        Err(e) => {
            println!("Claude Code: not pushed — {e}");
            pods.len()
        }
    }
}

/// What each of `pods` would get from [`stage`], for a dry run: `(name, what)`. Nothing recorded.
pub(crate) fn preview(cfg: &Config, pods: &[Pod], token: Option<&str>) -> std::result::Result<Vec<(String, String)>, String> {
    if token.is_some() {
        return Ok(pods.iter().map(|p| (p.name.clone(), "--cc-token's token".to_string())).collect());
    }
    let pool = claude::load_pool(cfg)?;
    if pool.is_empty() {
        return Ok(Vec::new());
    }
    let state = claude::load(&claude::state_path(cfg)?)?;
    let owners = claude::owners_for(cfg);
    let machines: Vec<Machine> = pods.iter().map(|p| Machine::new(p, &owners)).collect();
    let (planned, changes) = claude::assign(state, &pool, &owners, &machines, &Want::Ensure, unix_now());
    Ok(pods
        .iter()
        .filter_map(|p| {
            let a = planned.of(p)?;
            let why = changes.iter().find(|c| c.id == p.id && c.provider == p.provider).map(|c| format!(" ({})", c.because())).unwrap_or_default();
            Some((p.name.clone(), format!("Claude Code {}{why}", label(&pool, a.account.as_deref()))))
        })
        .collect())
}

/// `config check`'s Claude Code rows: how many accounts the tokens file holds (or why it can't be
/// used — a ✗, added to `missing`), the lines it skipped, and whether `CLAUDE_CODE_OAUTH_TOKEN` is
/// the pool (of one) or unused beside the file. Never a token.
pub(crate) fn config_check_rows(cfg: &Config, missing: &mut Vec<String>) -> Vec<String> {
    use claude::{TOKENS_FILE_KEY as FILE, TOKEN_KEY as TOKEN};
    let set = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    let mut out = Vec::new();
    let pool = claude::load_pool(cfg);
    if set(FILE) {
        match &pool {
            Ok(p) => {
                out.push(format!("  ✓ {FILE:<28} {} (one per machine; tokens never shown)", p.describe()));
                out.extend(p.notes.iter().filter(|n| !n.starts_with(TOKEN)).map(|n| format!("      └ ⚠ {n}")));
            }
            Err(e) => {
                let why = e.strip_prefix(&format!("{FILE} ")).unwrap_or(e);
                out.push(format!("  ✗ {FILE:<28} {why}"));
                missing.push(format!("{FILE} ({why})"));
            }
        }
        if set(TOKEN) {
            out.push(format!("  · {TOKEN:<28} (set — unused: {FILE}'s accounts replace it)"));
        }
        return out;
    }
    out.push(format!("  · {FILE:<28} (missing — optional: a tokens file puts each machine on its own account)"));
    match (&pool, set(TOKEN)) {
        (Ok(_), true) => out.push(format!("  ✓ {TOKEN:<28} (set — a pool of one: every machine on it)")),
        (Err(e), _) => {
            out.push(format!("  ✗ {TOKEN:<28} {e}"));
            missing.push(format!("{TOKEN} (isn't a token)"));
        }
        (Ok(_), false) => out.push(format!("  · {TOKEN:<28} (missing — no Claude Code token goes to the pods)")),
    }
    out
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
