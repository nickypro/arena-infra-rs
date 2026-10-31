//! `pods spare up|list|down`: capacity spares on Hetzner ([`arena_core::spare`]) — servers made
//! ahead of need so a move to a bigger type doesn't wait on capacity when it's wanted (the
//! operator's swapper made `cx43-spare-N` every 3 minutes, in fsn1/nbg1/hel1). Taken by `pods
//! migrate copy <name> --onto-spare` / `replace --onto-spare`.

use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::spare::{self, SpareCloud};
use arena_core::{Config, Pod, PodSpec};
use clap::Subcommand;

use crate::pods::create::{create_extra, pin_vm_image};
use crate::pods::lock::print_would_refuse;
use crate::{confirm, Interrupt};

#[derive(Subcommand)]
pub(crate) enum SpareCmd {
    /// Make N capacity spares (Hetzner), taken later by `--onto-spare`
    ///
    /// Named `<prefix>-spare-N` and labelled as spares: nobody's machine — left out of `--all`,
    /// the proxy, ssh-config, backups and keys; counted by `pods list`, balance, spend and
    /// `teardown --check`. Each create walks every location HETZNER_LOCATION allows; with
    /// --keep-trying (or --retry-mins) it waits for capacity.
    #[command(verbatim_doc_comment)]
    Up {
        /// How many to make (at most 20).
        count: usize,
        /// Server type, or a comma list of fallbacks (default: HETZNER_SERVER_TYPE)
        #[arg(long, value_name = "TYPE")]
        server_type: Option<String>,
        /// VM image: arena, a snapshot id or a name (default: HETZNER_IMAGE)
        #[arg(long)]
        image: Option<String>,
        /// On no capacity, keep trying for up to an hour
        #[arg(long)]
        keep_trying: bool,
        /// Keep retrying for up to this many minutes while capacity is short (0 = once)
        #[arg(long, default_value_t = 0)]
        retry_mins: u64,
        /// Seconds between retry rounds.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
        /// Don't wait for each to come up (running, with its IP)
        #[arg(long)]
        no_wait: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// List this cohort's spares: type, where, state, age, cost
    List {
        /// JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Delete spares — named ones, or --all
    Down {
        /// Spare names (`spare-1` or `devtest-spare-1`) or ids.
        names: Vec<String>,
        /// Every one of this cohort's spares.
        #[arg(long, conflicts_with = "names")]
        all: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

/// How long `spare up` waits for a new spare to be running with its IP.
const READY_WAIT: Duration = Duration::from_secs(10 * 60);

/// `pods spare …` against the configured Hetzner project.
pub(crate) async fn handle_spare(cmd: SpareCmd, cfg: &Config, yes: bool, interrupt: &Interrupt) -> Result<()> {
    let cloud = crate::image::hetzner(cfg).context("capacity spares are Hetzner servers")?;
    let tracking = crate::spend::SpendCtx::from_config(cfg);
    let tracked = crate::spend::Tracked::for_spares(&cloud, &tracking);
    spare_on(cmd, cfg, &tracked, &cloud, yes, interrupt).await
}

/// [`handle_spare`] once the backend is built: `provider` makes and deletes (through the
/// spend ledger), `spares` lists — the seam tests drive with a fake.
pub(crate) async fn spare_on(cmd: SpareCmd, cfg: &Config, provider: &dyn Provider, spares: &dyn SpareCloud, yes: bool, interrupt: &Interrupt) -> Result<()> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let now = arena_core::snapshot::unix_now();
    match cmd {
        SpareCmd::List { json } => {
            let all = spares.spares().await.context("listing the capacity spares")?;
            crate::spend::SpendCtx::from_config(cfg).spares(provider.name(), &all, now);
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else if all.is_empty() {
                println!("no spares (make some: `arena pods spare up 2 --server-type cx43 --keep-trying`)");
            } else {
                print!("{}", spare::render(&all, now));
            }
            Ok(())
        }
        SpareCmd::Down { names, all, dry_run } => {
            if names.is_empty() && !all {
                anyhow::bail!("name the spares to delete (`spare-1` or its id), or pass --all");
            }
            let listed = spares.spares().await.context("listing the capacity spares")?;
            let chosen: Vec<Pod> = if all {
                listed.clone()
            } else {
                let mut out = Vec::new();
                for n in &names {
                    let full = arena_core::naming::qualify(&prefix, n);
                    match listed.iter().find(|p| p.id == n.trim() || p.name == n.trim() || p.name == full) {
                        Some(p) => out.push(p.clone()),
                        None => anyhow::bail!(
                            "{n} isn't one of this cohort's spares ({}) — only a spare is deleted here; a fleet machine is `pods terminate`'s",
                            if listed.is_empty() { "there are none".to_string() } else { listed.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ") }
                        ),
                    }
                }
                out
            };
            if chosen.is_empty() {
                println!("no spares to delete.");
                return Ok(());
            }
            let list = chosen.iter().map(|p| format!("{} (id={})", p.name, p.id)).collect::<Vec<_>>().join(", ");
            if dry_run {
                println!("[dry-run] would delete {} spare(s): {list}\n[dry-run] nothing changed.", chosen.len());
                return Ok(());
            }
            if !confirm(yes, &format!("Delete {} spare(s): {list}? (irreversible; nothing of anyone's is on a spare)", chosen.len()))? {
                println!("aborted.");
                return Ok(());
            }
            // Under the claim lock, from a fresh look: one a move took while this waited at its
            // prompt is a fleet machine with someone's home on it now — never deleted here.
            let _claim = crate::pods::adopt::claim_spares(cfg)?;
            let now_spares = spares.spares().await.context("listing the capacity spares again before deleting")?;
            let policy = arena_core::retry::RetryPolicy::default();
            let mut failed = Vec::new();
            for p in &chosen {
                if !still_spare(&now_spares, p) {
                    eprintln!("[kept] {} (id={}): no longer a spare (taken by a move meanwhile?) — not deleted", p.name, p.id);
                    failed.push(p.name.clone());
                    continue;
                }
                match arena_core::retry::retrying(&policy, || provider.terminate_pod(&p.id)).await {
                    Ok(()) => println!("[deleted] {} (id={})", p.name, p.id),
                    Err(e) => {
                        eprintln!("[failed] {}: {e}", p.name);
                        failed.push(p.name.clone());
                    }
                }
            }
            if let Ok(left) = spares.spares().await {
                crate::spend::SpendCtx::from_config(cfg).spares(provider.name(), &left, arena_core::snapshot::unix_now());
            }
            if !failed.is_empty() {
                anyhow::bail!("couldn't delete {}", failed.join(", "));
            }
            Ok(())
        }
        SpareCmd::Up { count, server_type, image, keep_trying, retry_mins, retry_secs, no_wait, dry_run } => {
            if !(1..=spare::MAX_UP).contains(&count) {
                anyhow::bail!("spare up takes 1 to {} spares at a time (each one bills from its create)", spare::MAX_UP);
            }
            let types = crate::pods::replace::server_types("hetzner", server_type.as_deref())?;
            let (_, vm_image) = crate::pods::create::image_flag("hetzner", image)?;
            let fleet = provider.list_pods().await.context("listing the fleet")?;
            let have = spares.spares().await.context("listing the capacity spares")?;
            let taken: Vec<&str> = fleet.iter().chain(&have).map(|p| p.name.as_str()).collect();
            let names = spare::free_names(&prefix, &taken, count);
            let mut base = PodSpec::from_config(cfg);
            base.vm_image = vm_image;
            if let [one] = types.as_slice() {
                base.server_type = Some(one.clone());
            }
            base.api_extra = Some(spare_extra(provider, cfg, &prefix)?);
            if types.len() <= 1 {
                pin_vm_image("hetzner", cfg, &mut base).await?;
            }
            let req = arena_core::placement::Request::from_flags(None, None, &base, None, arena_core::placement::Order::Listed)?
                .with_server_types(&types);
            let plan = if req.is_single() {
                crate::pods::placement::single_option_plan("hetzner", &base)
            } else {
                arena_core::placement::plan_options(&req, "hetzner", &arena_core::placement::PriceBook::unpriced())
            };
            let opts = arena_core::provider::hetzner::HetznerOpts::from_config(cfg);
            let what: Vec<String> = plan.options.iter().map(|o| provider.describe(&arena_core::placement::spec_for(&base, "x", o))).collect();
            let rounds = crate::pods::create::replacement_rounds(keep_trying, retry_mins, retry_secs);
            let waits = if rounds.window.is_zero() {
                "one try (--keep-trying / --retry-mins to wait for capacity)".to_string()
            } else {
                format!("retried every {}s for up to {} min while no location has room", rounds.every.as_secs(), rounds.window.as_secs() / 60)
            };
            println!(
                "spare up {count}: {} — {}\n  each: {}\n  {}; labelled {}={} (no one's machine: no proxy, backup or keys)",
                names.join(", "),
                opts.locations.describe(),
                what.join(" | "),
                waits,
                spare::SPARE_LABEL,
                spare::label_value(&prefix)
            );
            if !have.is_empty() {
                println!("  ({} spare(s) already: `arena pods spare list`)", have.len());
            }
            if dry_run {
                if plan.options.is_empty() {
                    print_would_refuse("nothing to create: no option left to try");
                }
                println!("[dry-run] nothing created.");
                return Ok(());
            }
            if !confirm(yes, &format!("Create {count} spare(s) ({})? They bill until `arena pods spare down`.", names.join(", ")))? {
                println!("aborted.");
                return Ok(());
            }
            let mut show = |p: &arena_core::placement::Progress| {
                if p.is_created() {
                    println!("  {}", p.line());
                } else {
                    eprintln!("  {}", p.line());
                }
            };
            let run = arena_core::placement::place(provider, &base, &names, &plan, rounds, None, || interrupt.wait(), &mut show).await;
            let made = run.created.clone();
            for p in &made {
                println!("[created] {} id={}{}", p.name, p.id, p.location.as_deref().map(|l| format!(" in {l}")).unwrap_or_default());
            }
            if let Ok(all) = spares.spares().await {
                crate::spend::SpendCtx::from_config(cfg).spares(provider.name(), &all, arena_core::snapshot::unix_now());
            }
            let missing = names.len() - made.len();
            if !no_wait && !made.is_empty() {
                println!("waiting for {} to come up…", made.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));
                let ready = wait_ready(spares, &made, READY_WAIT).await;
                for p in &made {
                    match ready.iter().find(|r| r.id == p.id) {
                        Some(r) => println!("[ready] {} {} ({})", r.name, r.ssh_ip.as_deref().unwrap_or("?"), r.gpu_type.as_deref().unwrap_or("?")),
                        None => eprintln!("[not up yet] {} — `arena pods spare list` shows it", p.name),
                    }
                }
            }
            if missing > 0 {
                anyhow::bail!(
                    "{missing} of {count} spare(s) not made ({}) — {} made",
                    end_words(&run.end),
                    made.len()
                );
            }
            println!("take one with `arena pods migrate copy <name> --onto-spare` (or `pods replace <name> --onto-spare`).");
            Ok(())
        }
    }
}

/// Whether `p` (as listed before the prompt) is still one of `now` — this cohort's spares as
/// listed under the claim lock — by id AND name. Pure.
fn still_spare(now: &[Pod], p: &Pod) -> bool {
    now.iter().any(|n| n.id == p.id && n.name == p.name)
}

/// What a spare's create merges in: `CREATE_EXTRA_JSON` (the cohort's create default, checked
/// against Hetzner's own fields) with the spare label added to its labels.
fn spare_extra(provider: &dyn Provider, cfg: &Config, prefix: &str) -> Result<arena_core::apiextra::Extra> {
    let mut extra = serde_json::Value::Object(create_extra(provider, cfg, None)?.unwrap_or_default());
    let mut labels = serde_json::Map::new();
    labels.insert("labels".into(), serde_json::Value::Object(spare::labels(prefix)));
    arena_core::apiextra::deep_merge(&mut extra, &serde_json::Value::Object(labels));
    Ok(extra.as_object().cloned().unwrap_or_default())
}

/// Why a placement run ended short, for the summary.
fn end_words(end: &arena_core::placement::End) -> String {
    use arena_core::placement::End;
    match end {
        End::Exhausted => "no location had room — --keep-trying / --retry-mins wait for it".into(),
        End::WindowElapsed => "no room within the retry window".into(),
        End::Interrupted => "interrupted".into(),
        End::Aborted { error, .. } | End::Failed { error, .. } => error.to_string(),
        End::Filled => "?".into(),
    }
}

/// Wait (polling the spare listing every 10 s, up to `limit`) until each of `made` is ready
/// ([`spare::ready`]); returns the ready ones as last listed.
async fn wait_ready(spares: &dyn SpareCloud, made: &[Pod], limit: Duration) -> Vec<Pod> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let listed = spares.spares().await.unwrap_or_default();
        let ready: Vec<Pod> = listed.into_iter().filter(|p| spare::ready(p) && made.iter().any(|m| m.id == p.id)).collect();
        if ready.len() == made.len() || tokio::time::Instant::now() >= deadline {
            return ready;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
